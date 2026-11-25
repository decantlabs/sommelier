<#
.SYNOPSIS
End to end check through the public address: whoami, one CHAT, and reading it back live.

.DESCRIPTION
Takes the token from the environment variable named by -TokenVar (SOMMELIER_TOKEN by
default), never from the command line. Starts a `sommelier tail` that waits for the
check message, sends one CHAT on the topic `check`, and passes when the tail sees it.
An observer token cannot send: the check then stops after whoami and one log read.
Exit code 0 means the whole path works (tunnel, TLS, hub, token, send, fan out).

.EXAMPLE
$env:SOMMELIER_TOKEN = Get-Content "$env:USERPROFILE\.sommelier-token"
powershell -ExecutionPolicy Bypass -File scripts\check.ps1
#>
[CmdletBinding()]
param(
    [string]$Url = $env:SOMMELIER_URL,
    [string]$TokenVar = 'SOMMELIER_TOKEN',
    [string]$Topic = 'check',
    [int]$TimeoutSeconds = 20
)

$ErrorActionPreference = 'Stop'
if (-not $Url) { throw 'no hub address: set SOMMELIER_URL (for example wss://hub.example.com) or pass -Url' }
$root = Split-Path -Parent $PSScriptRoot

function Find-Sommelier {
    foreach ($profileName in 'release', 'debug') {
        $p = Join-Path $root "target\$profileName\sommelier.exe"
        if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command sommelier -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw 'no sommelier.exe found, build it first: cargo build --release -j 4'
}

function Fail([string]$Why) {
    Write-Host "FAIL: $Why" -ForegroundColor Red
    exit 1
}

$exe = Find-Sommelier
$token = [Environment]::GetEnvironmentVariable($TokenVar)
if (-not $token) { Fail "the environment variable $TokenVar is empty, put a token there first" }

# The child processes read the token from the environment, so it never shows in a process list.
$savedToken = $env:SOMMELIER_TOKEN
$savedUrl = $env:SOMMELIER_URL
$env:SOMMELIER_TOKEN = $token
$env:SOMMELIER_URL = $Url
$out = Join-Path ([IO.Path]::GetTempPath()) ("sommelier-check-{0}.out" -f [Guid]::NewGuid().ToString('N'))
$tail = $null

try {
    $who = & $exe whoami --json
    if ($LASTEXITCODE -ne 0) { Fail "whoami through $Url did not work, see the error above" }
    $me = $who | ConvertFrom-Json
    Write-Host "connected  $Url as $($me.name) ($($me.role)), hub at #$($me.last_id)"

    if ($me.role -eq 'observer') {
        & $exe log --last 1 | Out-Host
        if ($LASTEXITCODE -ne 0) { Fail 'log read failed' }
        Write-Host 'OK: read works. An observer token cannot send, the send check was skipped.' -ForegroundColor Green
        exit 0
    }

    $nonce = [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $body = "sommelier check $nonce from $env:COMPUTERNAME"
    $since = [int64]$me.last_id + 1

    # Waits for exactly one matching message. Starting at last_id + 1 means a message
    # sent before the subscription is up still arrives, from the log.
    $tailArgs = @('tail', '--since', $since, '--kind', 'CHAT', '--topic', $Topic, '--from', $me.name, '--max', '1', '--json')
    $tail = Start-Process -FilePath $exe -ArgumentList $tailArgs -NoNewWindow -PassThru -RedirectStandardOutput $out
    $null = $tail.Handle
    Start-Sleep -Milliseconds 1500

    $watch = [Diagnostics.Stopwatch]::StartNew()
    $sent = & $exe send CHAT $body --topic $Topic
    if ($LASTEXITCODE -ne 0) { Fail 'send was refused, see the error above' }
    $id = ($sent | Select-Object -Last 1).Trim()
    Write-Host "sent       $id : $body"

    if (-not $tail.WaitForExit($TimeoutSeconds * 1000)) {
        Fail "the CHAT did not come back within $TimeoutSeconds s"
    }
    $ms = $watch.ElapsedMilliseconds
    $line = Get-Content -Path $out -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $line) { Fail 'the tail ended without a message' }
    $msg = $line | ConvertFrom-Json
    if ($msg.body -ne $body) { Fail "read back a different message: $line" }
    Write-Host "read back  #$($msg.id) from $($msg.from) $ms ms after the send started"
    Write-Host 'OK: tunnel, hub, token, send and live stream all work.' -ForegroundColor Green
    exit 0
} finally {
    if ($tail -and -not $tail.HasExited) { Stop-Process -Id $tail.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item -Path $out -ErrorAction SilentlyContinue
    $env:SOMMELIER_TOKEN = $savedToken
    $env:SOMMELIER_URL = $savedUrl
}

<#
.SYNOPSIS
Starts the sommelier hub and the Cloudflare tunnel in front of it. Ctrl+C stops both.

.DESCRIPTION
Uses target\release\sommelier.exe when it exists, else target\debug\sommelier.exe, else a
sommelier on the PATH. The hub keeps its log (bus.jsonl) and its tokens (tokens.json)
in the data folder, by default %LOCALAPPDATA%\sommelier. The tunnel output goes to
cloudflared.log in the same folder. The process ids are written to hub.pids so that
scripts\stop-hub.ps1 can stop a hub started from another window.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\start-hub.ps1

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\start-hub.ps1 -NoTunnel
#>
[CmdletBinding()]
param(
    [string]$Data = (Join-Path $env:LOCALAPPDATA 'sommelier'),
    [string]$Listen = '127.0.0.1:7300',
    [string]$Tunnel = 'sommelier',
    [string]$PublicUrl = $env:SOMMELIER_PUBLIC_URL,
    [switch]$NoTunnel
)

$ErrorActionPreference = 'Stop'
if (-not $PublicUrl) { throw 'no public address: set SOMMELIER_PUBLIC_URL (for example wss://hub.example.com) or pass -PublicUrl' }
$root = Split-Path -Parent $PSScriptRoot

function Find-Sommelier {
    foreach ($profileName in 'release', 'debug') {
        $p = Join-Path $root "target\$profileName\sommelier.exe"
        if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command sommelier -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "no sommelier.exe found under $root\target or on the PATH. Build it first: cargo build --release -j 4"
}

function Find-Cloudflared {
    $p = Join-Path $env:USERPROFILE '.cargo\bin\cloudflared.exe'
    if (Test-Path $p) { return $p }
    $cmd = Get-Command cloudflared -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw 'cloudflared not found in %USERPROFILE%\.cargo\bin or on the PATH. Use -NoTunnel for a local hub only.'
}

function Test-Listening([string]$HostName, [int]$Port) {
    $c = New-Object System.Net.Sockets.TcpClient
    try { $c.Connect($HostName, $Port); return $true } catch { return $false } finally { $c.Close() }
}

$exe = Find-Sommelier
$cut = $Listen.LastIndexOf(':')
$listenHost = $Listen.Substring(0, $cut)
$listenPort = [int]$Listen.Substring($cut + 1)
if ($listenHost -eq '0.0.0.0') { $listenHost = '127.0.0.1' }

if (Test-Listening $listenHost $listenPort) {
    throw "something already listens on $Listen. Is a hub running? Stop it with scripts\stop-hub.ps1"
}

New-Item -ItemType Directory -Force -Path $Data | Out-Null
$pidFile = Join-Path $Data 'hub.pids'
$cfLog = Join-Path $Data 'cloudflared.log'

Write-Host "binary: $exe"
$hub = Start-Process -FilePath $exe -ArgumentList @('serve', '--listen', $Listen, '--data', "`"$Data`"") -NoNewWindow -PassThru
$null = $hub.Handle  # keeps the exit code readable after the process ends

$up = $false
for ($i = 0; $i -lt 50; $i++) {
    if ($hub.HasExited) { throw "the hub exited at once with code $($hub.ExitCode), see the lines above" }
    if (Test-Listening $listenHost $listenPort) { $up = $true; break }
    Start-Sleep -Milliseconds 200
}
if (-not $up) {
    Stop-Process -Id $hub.Id -Force -ErrorAction SilentlyContinue
    throw "the hub did not open $Listen within 10 s"
}

$cf = $null
if (-not $NoTunnel) {
    $cfExe = Find-Cloudflared
    $cf = Start-Process -FilePath $cfExe -ArgumentList @('tunnel', 'run', $Tunnel) -NoNewWindow -PassThru -RedirectStandardError $cfLog
    $null = $cf.Handle
}

$pids = @{ hub = $hub.Id; tunnel = $(if ($cf) { $cf.Id } else { $null }); data = $Data }
$pids | ConvertTo-Json | Set-Content -Path $pidFile -Encoding UTF8

$members = @(& $exe token list --data $Data).Count

Write-Host ''
Write-Host "hub        ws://$Listen (data in $Data)"
if ($cf) {
    Write-Host "tunnel     $Tunnel (output in $cfLog)"
    Write-Host "hand out   $PublicUrl" -ForegroundColor Green
} else {
    Write-Host 'tunnel     not started (-NoTunnel), the hub is reachable from this machine only'
}
Write-Host "members    $members token(s). Add one: & `"$exe`" token add NAME --data `"$Data`""
Write-Host 'stop       Ctrl+C here, or scripts\stop-hub.ps1 from another window'
Write-Host ''

try {
    while ($true) {
        if ($hub.HasExited) {
            Write-Warning "the hub exited with code $($hub.ExitCode)"
            break
        }
        if ($cf -and $cf.HasExited) {
            Write-Warning "cloudflared exited with code $($cf.ExitCode), see $cfLog"
            break
        }
        Start-Sleep -Seconds 1
    }
} finally {
    foreach ($p in @($cf, $hub)) {
        if ($p -and -not $p.HasExited) {
            Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -Path $pidFile -ErrorAction SilentlyContinue
    Write-Host 'hub and tunnel stopped'
}

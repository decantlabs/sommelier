<#
.SYNOPSIS
Stops a sommelier hub and its Cloudflare tunnel started by scripts\start-hub.ps1.

.DESCRIPTION
Reads the process ids from hub.pids in the data folder. When that file is missing
it looks for a `sommelier.exe serve` process and a `cloudflared tunnel run <Tunnel>`
process and stops those.
#>
[CmdletBinding()]
param(
    [string]$Data = (Join-Path $env:LOCALAPPDATA 'sommelier'),
    [string]$Tunnel = 'sommelier'
)

$ErrorActionPreference = 'Stop'
$pidFile = Join-Path $Data 'hub.pids'
$targets = @()

if (Test-Path $pidFile) {
    $pids = Get-Content -Raw -Path $pidFile | ConvertFrom-Json
    foreach ($entry in @(@{ id = $pids.tunnel; name = 'cloudflared' }, @{ id = $pids.hub; name = 'sommelier' })) {
        if (-not $entry.id) { continue }
        $p = Get-Process -Id $entry.id -ErrorAction SilentlyContinue
        if ($p -and $p.ProcessName -eq $entry.name) { $targets += $p }
    }
}

if ($targets.Count -eq 0) {
    $found = Get-CimInstance Win32_Process | Where-Object {
        ($_.Name -eq 'sommelier.exe' -and $_.CommandLine -match '\sserve(\s|$)') -or
        ($_.Name -eq 'cloudflared.exe' -and $_.CommandLine -match "tunnel\s+run\s+$([regex]::Escape($Tunnel))(\s|$)")
    }
    foreach ($w in $found) {
        $p = Get-Process -Id $w.ProcessId -ErrorAction SilentlyContinue
        if ($p) { $targets += $p }
    }
}

if ($targets.Count -eq 0) {
    Write-Host 'no running hub or tunnel found'
} else {
    foreach ($p in $targets) {
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
        Write-Host "stopped $($p.ProcessName) (pid $($p.Id))"
    }
}
Remove-Item -Path $pidFile -ErrorAction SilentlyContinue

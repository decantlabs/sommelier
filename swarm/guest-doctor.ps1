<#
.SYNOPSIS
  Guest doctor: checks a Windows machine for Decant and writes one report file.

.DESCRIPTION
  Made to be run by the guest's agent (or by hand). It never asks a question and never waits for a key.
  Exit code 0 means the report was written.

  What it does:
    * Reads Windows, CPU, RAM, disks, GPU, settings, services and tool versions.
    * Checks that the paths named by SOMMELIER_TITLE_EXE, SOMMELIER_TITLE_DIR and SOMMELIER_TITLE_LOG exist.
    * Makes a few small probes (a junction, a symlink, a hard link) in a temp folder.
      Every probe removes itself.
  What it does not do: no admin rights, no downloads, no registry writes, no title start,
  and no hashing or listing of the files of any title.

  Output: doctor-report.md next to this script (or in the folder given with -OutDir).
  Personal data is hidden: user name, machine name, home folder, e-mail addresses and tokens.

  Run it with:  powershell -NoProfile -ExecutionPolicy Bypass -File guest-doctor.ps1
  If your agent host blocks junctions (error 448), start it from a plain terminal.

.PARAMETER OutDir
  Folder for the report. Default: the folder of this script.

.PARAMETER OutName
  File name of the report. Default: doctor-report.md
#>
param(
    [string]$OutDir = '',
    [string]$OutName = 'doctor-report.md'
)

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$script:Started = Get-Date
$script:Rows = New-Object System.Collections.ArrayList
$script:Probes = @()   # temp folders made by probes, removed at the end

# ---------------------------------------------------------------------------------------------
# Hiding personal data. Every text passes through Hide() before it is shown or written.
# ---------------------------------------------------------------------------------------------
$script:Hide = New-Object System.Collections.ArrayList   # pairs of (text, replacement)
function Add-Hide([string]$text, [string]$replacement) {
    if ($text -and $text.Length -ge 3) { [void]$script:Hide.Add(@($text, $replacement)) }
}
Add-Hide $env:USERPROFILE '<USER>'
Add-Hide $env:TEMP '<TEMP>'

function Hide([string]$s) {
    if (-not $s) { return $s }
    # Longest text first, so a folder inside another one is hidden by its own name.
    foreach ($pair in ($script:Hide | Sort-Object { -($_[0].Length) })) {
        $s = $s.Replace($pair[0], $pair[1])
        $s = $s.Replace($pair[0].Replace('\', '/'), $pair[1])
    }
    # Any other home folder of any drive.
    $s = [regex]::Replace($s, '(?i)[A-Z]:[\\/]Users[\\/][^\\/\s"'']+', '<USER>')
    # Names of the person and of the machine, as whole words.
    foreach ($n in @($env:USERNAME, $env:COMPUTERNAME, $env:USERDOMAIN, $env:USERDNSDOMAIN)) {
        if ($n -and $n.Length -ge 3) {
            $rep = '<MACHINE>'
            if ($n -eq $env:USERNAME) { $rep = '<USER>' }
            $s = [regex]::Replace($s, '(?i)(?<![A-Za-z0-9])' + [regex]::Escape($n) + '(?![A-Za-z0-9])', $rep)
        }
    }
    # E-mail addresses and well known token shapes.
    $s = [regex]::Replace($s, '[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(\.[A-Za-z0-9\-]+)+', '<EMAIL>')
    $s = [regex]::Replace($s, '(?i)\b(ghp_|gho_|ghs_|github_pat_|sk-|xox[abp]-|eyJ)[A-Za-z0-9_\-\.]{10,}', '<TOKEN>')
    return $s
}

# ---------------------------------------------------------------------------------------------
# Rows of the report. Status is OK, WARN or FAIL. Weight orders the summary (bigger is worse).
# ---------------------------------------------------------------------------------------------
function Add-Row([string]$Section, [string]$Check, [string]$Status, [string]$Value, [string]$Meaning, [string]$Fix = '', [int]$Weight = 0) {
    if ($Weight -eq 0) { if ($Status -eq 'FAIL') { $Weight = 100 } elseif ($Status -eq 'WARN') { $Weight = 50 } }
    $row = [pscustomobject]@{ Section = $Section; Check = (Hide $Check); Status = $Status; Value = (Hide $Value); Meaning = (Hide $Meaning); Fix = (Hide $Fix); Weight = $Weight }
    [void]$script:Rows.Add($row)
}

# Runs one group of checks. A failure of the check itself becomes a WARN row, never a crash.
function Invoke-Check([string]$Section, [scriptblock]$Body) {
    try { & $Body } catch {
        $msg = ($_.Exception.Message -replace '\s+', ' ')
        if ($msg.Length -gt 160) { $msg = $msg.Substring(0, 160) }
        Add-Row $Section 'check could not run' 'WARN' $msg 'This part of the doctor failed on this machine. That is itself worth a look.' '' 20
    }
}

# The Windows error number inside a .NET exception.
function Get-Win32Code($ex) {
    $e = $ex
    while ($e.InnerException) { $e = $e.InnerException }
    return ($e.HResult -band 0xFFFF)
}

function Get-RegValue([string]$Path, [string]$Name) {
    try { return (Get-ItemProperty -Path $Path -Name $Name -ErrorAction Stop).$Name } catch { return $null }
}

function Format-Bytes([double]$b) {
    if ($b -ge 1GB) { return ('{0:N1} GB' -f ($b / 1GB)) }
    if ($b -ge 1MB) { return ('{0:N1} MB' -f ($b / 1MB)) }
    return ('{0:N0} bytes' -f $b)
}

function Get-FileVersionText([string]$Path) {
    if (Test-Path -LiteralPath $Path) { return (Get-Item -LiteralPath $Path).VersionInfo.FileVersion }
    return $null
}

# Native helpers: processor features and process mitigation policies. Plain P/Invoke, no code injection.
$nativeOk = $true
try {
    Add-Type -ErrorAction Stop -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class DoctorNative {
    [DllImport("kernel32.dll")] public static extern bool IsProcessorFeaturePresent(uint f);
    [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll")] public static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetProcessMitigationPolicy(IntPtr h, int policy, out uint flags, ulong size);
}
'@
} catch { $nativeOk = $false }

# RedirectionTrust is policy number 16. The first bit is Enforce, the second is Audit.
function Get-RedirectionTrust([int]$ProcId) {
    if (-not $nativeOk) { return 'unknown (native helper missing)' }
    $own = ($ProcId -eq $PID)
    if ($own) { $h = [DoctorNative]::GetCurrentProcess() }
    else { $h = [DoctorNative]::OpenProcess(0x1000, $false, [uint32]$ProcId) }   # QUERY_LIMITED_INFORMATION
    if ($h -eq [IntPtr]::Zero) { return 'unknown (cannot open process)' }
    try {
        [uint32]$flags = 0
        $ok = [DoctorNative]::GetProcessMitigationPolicy($h, 16, [ref]$flags, [uint64]4)
        if (-not $ok) { return ('unknown (error ' + [Runtime.InteropServices.Marshal]::GetLastWin32Error() + ')') }
        $parts = @()
        if ($flags -band 1) { $parts += 'ENFORCED' }
        if ($flags -band 2) { $parts += 'audit' }
        if ($parts.Count -eq 0) { return 'off' }
        return ($parts -join '+')
    } finally { if (-not $own) { [void][DoctorNative]::CloseHandle($h) } }
}

# ---------------------------------------------------------------------------------------------
# Programs with a time limit (tool versions, mklink). No window, no waiting forever.
# ---------------------------------------------------------------------------------------------
function Invoke-Limited([string]$File, [string]$ArgText, [int]$Seconds) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $File
    $psi.Arguments = $ArgText
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardInput = $true
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    try { $p.StandardInput.Close() } catch {}
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit($Seconds * 1000)) {
        try { $p.Kill() } catch {}
        return $null
    }
    $text = $outTask.Result
    if (-not $text) { $text = $errTask.Result }
    return $text
}

function Get-ToolVersion([string]$Name) {
    $cmd = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $cmd) { return $null }
    # The Microsoft Store stub of python and node is not a real install and would open the Store.
    if ($cmd.Source -like '*\WindowsApps\*' -and (Get-Item $cmd.Source).Length -eq 0) { return 'only a Store stub, not installed' }
    $text = Invoke-Limited $env:ComSpec ('/d /c ""' + $cmd.Source + '" --version"') 10
    if ($null -eq $text) { return 'found, but no answer in 10 s' }
    $line = ($text -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 1)
    if (-not $line) { return 'found, no version text' }
    $line = $line.Trim()
    if ($line.Length -gt 80) { $line = $line.Substring(0, 80) }
    return $line
}

# ---------------------------------------------------------------------------------------------
# Probes: links in a temp folder. Error numbers are reported exactly.
# ---------------------------------------------------------------------------------------------
function New-ProbeFolder([string]$Parent) {
    $dir = Join-Path $Parent ('guest-doctor-probe-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    [void][IO.Directory]::CreateDirectory($dir)
    $script:Probes += $dir
    return $dir
}

# Makes a link with cmd mklink (the launch tool does the same). Returns '' on success or the error text.
# Success is judged by the link existing, because the words of mklink depend on the Windows language.
function New-Link([string]$Kind, [string]$Link, [string]$Target) {
    $flag = @{ junction = '/J'; symdir = '/D'; symfile = ''; hard = '/H' }[$Kind]
    $argText = '/d /c mklink ' + $flag + ' "' + $Link + '" "' + $Target + '"'
    $text = Invoke-Limited $env:ComSpec $argText 20
    if ($null -eq $text) { return 'timeout' }
    if (Test-Path -LiteralPath $Link) { return '' }
    return (($text -replace '\s+', ' ').Trim())
}

# Removes a link and never the target. rmdir on a junction or directory symlink removes only the link.
function Remove-Link([string]$Link, [bool]$IsDir) {
    if ($IsDir) { [void](Invoke-Limited $env:ComSpec ('/d /c rmdir "' + $Link + '"') 20) }
    else { [void](Invoke-Limited $env:ComSpec ('/d /c del /q "' + $Link + '"') 20) }
}

# Opens a file (or lists a folder) and returns the Windows error number, or 0.
function Test-Open([string]$Path, [bool]$IsDir) {
    try {
        if ($IsDir) { [void][IO.Directory]::GetFileSystemEntries($Path) }
        else {
            $fs = [IO.File]::Open($Path, 'Open', 'Read', 'ReadWrite')
            $buf = New-Object byte[] 16
            [void]$fs.Read($buf, 0, 16)
            $fs.Close()
        }
        return 0
    } catch { return (Get-Win32Code $_.Exception) }
}

function Get-CodeText([int]$Code) {
    switch ($Code) {
        0   { return 'works' }
        2   { return 'error 2 (file not found)' }
        3   { return 'error 3 (path not found)' }
        5   { return 'error 5 (access denied)' }
        448 { return 'error 448 (ERROR_UNTRUSTED_MOUNT_POINT, the link is refused)' }
        1314 { return 'error 1314 (a privilege is missing)' }
        default { return ('error ' + $Code) }
    }
}

# Kind: junction, symdir (both point to a folder), symfile, hard (point to a file).
function Test-LinkKind([string]$Section, [string]$Label, [string]$Kind, [string]$Folder, [string]$TargetDir, [string]$TargetFile, [string]$InnerRelative) {
    $link = Join-Path $Folder ('link-' + $Kind)
    $isDir = ($Kind -eq 'junction' -or $Kind -eq 'symdir')
    $target = $TargetFile
    if ($isDir) { $target = $TargetDir }
    $err = New-Link $Kind $link $target
    if ($err) {
        $fix = 'Turn on Developer Mode (Settings, System, For developers) so links need no admin.'
        # Symbolic links are optional (junctions and hard links do the work), so this is only a warning.
        $st = 'FAIL'; $w = 90
        if ($Kind -eq 'symdir' -or $Kind -eq 'symfile') { $st = 'WARN'; $w = 40 }
        Add-Row $Section ($Label + ': create') $st ('not created: ' + $err) 'Windows would not make this link here. Symbolic links need Developer Mode or admin rights.' $fix $w
        return
    }
    $open = 0
    if ($isDir) {
        $open = Test-Open (Join-Path $link $InnerRelative) $false
        if ($open -eq 0) { $open = Test-Open $link $true }
    } else { $open = Test-Open $link $false }
    Remove-Link $link $isDir
    if ($open -eq 0) {
        Add-Row $Section ($Label + ': create and open') 'OK' 'works' 'The link was made and a file opened through it.'
    } elseif ($open -eq 448) {
        Add-Row $Section ($Label + ': create and open') 'FAIL' (Get-CodeText $open) 'The link was made, but Windows refuses to follow it. This is the 448 problem. The launch tool cannot run a title from here.' 'Start the doctor and the title from a plain terminal, not from inside the agent host (RedirectionTrust).' 200
    } else {
        Add-Row $Section ($Label + ': create and open') 'FAIL' (Get-CodeText $open) 'The link was made, but a file could not be opened through it.' '' 90
    }
}

# =============================================================================================
# 1. Windows
# =============================================================================================
$sec = 'System'
Invoke-Check $sec {
    $os = Get-CimInstance Win32_OperatingSystem
    $cv = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    $ubr = Get-RegValue $cv 'UBR'
    $disp = Get-RegValue $cv 'DisplayVersion'
    $build = [int]$os.BuildNumber
    $val = ('{0}, build {1}.{2}, version {3}, {4}' -f $os.Caption, $os.BuildNumber, $ubr, $disp, $os.OSArchitecture)
    if (-not [Environment]::Is64BitOperatingSystem) {
        Add-Row $sec 'Windows version' 'FAIL' $val 'Decant needs 64 bit Windows.' 'Install 64 bit Windows 11.'
    } elseif ($build -lt 22000) {
        Add-Row $sec 'Windows version' 'FAIL' $val 'Too old. Windows 11 is needed.' 'Update to Windows 11.'
    } elseif ($build -gt 26200) {
        Add-Row $sec 'Windows version' 'WARN' $val 'This is a preview (Insider) build. New builds change small things, so it can behave differently from a normal Windows.' '' 30
    } else {
        Add-Row $sec 'Windows version' 'OK' $val 'A normal Windows 11 build.'
    }
    Add-Row $sec 'Windows edition' 'OK' ('' + (Get-RegValue $cv 'EditionID')) 'Home, Pro and the others all work.'
    $ui = (Get-UICulture).Name
    $cu = (Get-Culture).Name
    $lang = ('display language {0}, regional format {1}' -f $ui, $cu)
    if ($ui -ne 'en-US' -or $cu -ne 'en-US') {
        Add-Row $sec 'Language and format' 'WARN' $lang 'Some titles read the language and format of Windows. A language other than en-US is one thing that could explain a stop at start up.' 'If the game stops early, try again with the format set to English (United States) in Settings, Time and language.' 40
    } else {
        Add-Row $sec 'Language and format' 'OK' $lang 'English (United States), the safest choice.'
    }
    $region = 'unknown'
    try { $region = (Get-WinHomeLocation).HomeLocation } catch {}
    Add-Row $sec 'Region and time zone' 'OK' ('region ' + $region + ', time zone ' + (Get-TimeZone).Id) 'Only for comparison with our machine.'
    $up = (Get-Date) - $os.LastBootUpTime
    Add-Row $sec 'Uptime' 'OK' ('{0:N1} days since last start' -f $up.TotalDays) 'Only for comparison.'
}

# =============================================================================================
# 2. CPU, memory, disks
# =============================================================================================
$sec = 'CPU and memory'
Invoke-Check $sec {
    $cpus = @(Get-CimInstance Win32_Processor)
    $cpu = $cpus[0]
    Add-Row $sec 'CPU' 'OK' ('{0}, {1} cores, {2} threads, {3} socket(s), {4}' -f $cpu.Name.Trim(), $cpu.NumberOfCores, $cpu.NumberOfLogicalProcessors, $cpus.Count, $cpu.Description) 'The model of the processor.'
    if ($nativeOk) {
        # Feature numbers of IsProcessorFeaturePresent.
        $feat = [ordered]@{ 'SSE3' = 13; 'SSSE3' = 36; 'SSE4.1' = 37; 'SSE4.2' = 38; 'AVX' = 39; 'AVX2' = 40; 'AVX512F' = 41; 'XSAVE' = 17; 'NX' = 12; 'CMPXCHG16B' = 14 }
        $have = @(); $miss = @()
        foreach ($k in $feat.Keys) { if ([DoctorNative]::IsProcessorFeaturePresent([uint32]$feat[$k])) { $have += $k } else { $miss += $k } }
        $missText = 'none'
        if ($miss.Count -gt 0) { $missText = $miss -join ' ' }
        $line = 'present: ' + ($have -join ' ') + ' / missing: ' + $missText
        if ($miss -contains 'AVX2') {
            Add-Row $sec 'CPU features' 'FAIL' $line 'This processor has no AVX2. Console titles are built for a processor that has it.' 'Run on a computer with a newer processor (Intel 4th generation or AMD Zen 1 or newer).'
        } elseif ($miss -contains 'SSE4.2' -or $miss -contains 'CMPXCHG16B') {
            Add-Row $sec 'CPU features' 'FAIL' $line 'A basic instruction set is missing.' 'Use a newer processor.'
        } else {
            Add-Row $sec 'CPU features' 'OK' $line 'AVX2 and the basic sets are there. (AVX512 is optional.)'
        }
    }
    $ram = [double](Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
    if ($ram -lt 15GB) {
        Add-Row $sec 'Memory (RAM)' 'FAIL' (Format-Bytes $ram) 'Less than 16 GB. Titles need a lot of memory.' 'Add RAM (16 GB at least, 32 GB is better).'
    } elseif ($ram -lt 31GB) {
        Add-Row $sec 'Memory (RAM)' 'OK' (Format-Bytes $ram) '16 GB is enough to start. 32 GB is better.'
    } else {
        Add-Row $sec 'Memory (RAM)' 'OK' (Format-Bytes $ram) 'Plenty.'
    }
    $os = Get-CimInstance Win32_OperatingSystem
    Add-Row $sec 'Virtual memory' 'OK' ('total ' + (Format-Bytes ($os.TotalVirtualMemorySize * 1KB)) + ', free ' + (Format-Bytes ($os.FreeVirtualMemory * 1KB))) 'RAM plus page file. A tiny page file can stop a title that reserves a lot of memory.'
}

$sec = 'Disks'
Invoke-Check $sec {
    foreach ($d in (Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3')) {
        $free = [double]$d.FreeSpace
        $label = ('{0} free of {1}, {2}' -f (Format-Bytes $free), (Format-Bytes ([double]$d.Size)), $d.FileSystem)
        if ($free -lt 5GB) { Add-Row $sec ('Drive ' + $d.DeviceID) 'FAIL' $label 'Almost full.' ('Free space on ' + $d.DeviceID + '.') }
        elseif ($free -lt 30GB) { Add-Row $sec ('Drive ' + $d.DeviceID) 'WARN' $label 'Not much space left. Caches and logs can fill it.' '' 20 }
        else { Add-Row $sec ('Drive ' + $d.DeviceID) 'OK' $label 'Enough space.' }
    }
}

# =============================================================================================
# 3. GPU
# =============================================================================================
$sec = 'GPU and driver'
Invoke-Check $sec {
    $vcs = @(Get-CimInstance Win32_VideoController | Where-Object { $_.Name -notmatch 'Basic|Remote|Virtual|Mirror' })
    if ($vcs.Count -eq 0) { Add-Row $sec 'GPU' 'FAIL' 'no real graphics adapter found' 'Windows shows no graphics card.' 'Install the graphics driver.'; return }
    foreach ($v in $vcs) {
        $ver = $v.DriverVersion
        $nice = $ver
        if ($v.Name -match 'NVIDIA' -and $ver -match '^\d+\.\d+\.(\d+)\.(\d+)$') {
            # The NVIDIA number: the last digit of the third part and the fourth part. 32.0.15.6094 is 560.94.
            $digits = ($Matches[1] + $Matches[2])
            $digits = $digits.Substring($digits.Length - 5)
            $nice = $ver + ' (NVIDIA ' + $digits.Substring(0, 3) + '.' + $digits.Substring(3) + ')'
        }
        $date = ''
        $age = 0
        if ($v.DriverDate) { $date = $v.DriverDate.ToString('yyyy-MM-dd'); $age = ((Get-Date) - $v.DriverDate).TotalDays }
        $val = ('{0}, driver {1}, dated {2}' -f $v.Name, $nice, $date)
        if ($age -gt 540) { Add-Row $sec ('GPU ' + $v.Name) 'WARN' $val 'The graphics driver is more than 18 months old.' 'Install the newest driver from the maker of the card.' 60 }
        else { Add-Row $sec ('GPU ' + $v.Name) 'OK' $val 'The driver is recent.' }
    }
    if ($vcs.Count -gt 1) {
        Add-Row $sec 'More than one GPU' 'WARN' ($vcs.Count.ToString() + ' adapters') 'A title could run on the weak one by mistake (laptops, or an integrated GPU next to a card).' 'In Windows Graphics settings choose the strong card for the game.' 25
    }
    # dxdiag without a window and with a time limit. Only a few lines are read, then the file is deleted.
    $tmp = Join-Path $env:TEMP ('guest-doctor-dxdiag-' + [guid]::NewGuid().ToString('N').Substring(0, 8) + '.txt')
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = 'dxdiag.exe'
        $psi.Arguments = '/whql:off /t "' + $tmp + '"'
        $psi.CreateNoWindow = $true
        $psi.UseShellExecute = $false
        $p = [System.Diagnostics.Process]::Start($psi)
        if (-not $p.WaitForExit(90000)) { try { $p.Kill() } catch {} }
        if (Test-Path -LiteralPath $tmp) {
            $pick = @{}
            foreach ($l in (Get-Content -LiteralPath $tmp -ErrorAction SilentlyContinue)) {
                if ($l -match '^\s*(Feature Levels|DDI Version|Driver Model|Shader Model|Hardware Scheduling|Dedicated Memory)\s*:\s*(.+)$') {
                    if (-not $pick.ContainsKey($Matches[1])) { $pick[$Matches[1]] = $Matches[2].Trim() }
                }
            }
            if ($pick.Count -gt 0) {
                $txt = (($pick.Keys | Sort-Object | ForEach-Object { $_ + '=' + $pick[$_] }) -join ', ')
                $fl = $pick['Feature Levels']
                if ($fl -and $fl -notmatch '12_') {
                    Add-Row $sec 'DirectX 12' 'FAIL' $txt 'The card does not report DirectX 12 feature levels.' 'Update the graphics driver, or use a card that supports DirectX 12.'
                } else {
                    Add-Row $sec 'DirectX 12 (dxdiag)' 'OK' $txt 'Feature levels, shader model and driver model as dxdiag sees them (first adapter).'
                }
            } else { Add-Row $sec 'DirectX 12 (dxdiag)' 'WARN' 'dxdiag wrote no usable lines' 'Could not read the DirectX level.' '' 15 }
        } else { Add-Row $sec 'DirectX 12 (dxdiag)' 'WARN' 'dxdiag did not finish in 90 s' 'Could not read the DirectX level.' '' 15 }
    } finally { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
}

# =============================================================================================
# 4. Windows settings that matter
# =============================================================================================
$sec = 'Windows settings'
Invoke-Check $sec {
    $dev = Get-RegValue 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' 'AllowDevelopmentWithoutDevLicense'
    if ($dev -eq 1) { Add-Row $sec 'Developer Mode' 'OK' 'on' 'Symbolic links work without admin rights.' }
    else { Add-Row $sec 'Developer Mode' 'WARN' 'off' 'Without it, a normal user cannot make symbolic links. Hard links and junctions still work.' 'Turn on Developer Mode: Settings, System, For developers.' 70 }

    $lp = Get-RegValue 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' 'LongPathsEnabled'
    if ($lp -eq 1) { Add-Row $sec 'Long paths' 'OK' 'on' 'Paths longer than 260 characters work.' }
    else { Add-Row $sec 'Long paths' 'WARN' 'off' 'Titles can have deep folders. A very long path can fail to open.' 'Turn on long paths (needs admin): the registry value LongPathsEnabled, or the policy "Enable Win32 long paths".' 35 }

    $dg = $null
    try { $dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard -ErrorAction Stop } catch {}
    if ($dg) {
        $vbs = [int]$dg.VirtualizationBasedSecurityStatus
        $svc = @($dg.SecurityServicesRunning)
        $txt = ('VBS status ' + $vbs + ' (0 off, 1 set up, 2 running), services running: ' + ($svc -join ','))
        if ($svc -contains 2) { Add-Row $sec 'Memory integrity (HVCI)' 'WARN' ('on. ' + $txt) 'Memory integrity checks every driver and code page. It costs some speed and can block tools that patch code.' 'Only if the game runs badly: turn it off in Windows Security, Device security, Core isolation. Ask the owner first.' 30 }
        elseif ($vbs -eq 2) { Add-Row $sec 'Virtualization based security' 'WARN' ('on. ' + $txt) 'Windows runs a protected layer under the system. It costs a little speed.' '' 15 }
        else { Add-Row $sec 'Virtualization based security and HVCI' 'OK' ('off. ' + $txt) 'No extra layer under Windows.' }
    } else { Add-Row $sec 'Virtualization based security and HVCI' 'WARN' 'could not read' 'The DeviceGuard information is not available.' '' 5 }

    $gd = 'HKLM:\SYSTEM\CurrentControlSet\Control\GraphicsDrivers'
    $hags = Get-RegValue $gd 'HwSchMode'
    $hagsText = 'not set'
    if ($hags -eq 2) { $hagsText = 'on' } elseif ($hags -eq 1) { $hagsText = 'off' }
    Add-Row $sec 'Hardware accelerated GPU scheduling' 'OK' $hagsText 'Either setting is allowed. It is listed so we can compare with our machine.'

    $tdrDelay = Get-RegValue $gd 'TdrDelay'
    $tdrLevel = Get-RegValue $gd 'TdrLevel'
    $dText = 'default (2 s)'; if ($null -ne $tdrDelay) { $dText = "$tdrDelay s" }
    $lText = 'default'; if ($null -ne $tdrLevel) { $lText = "$tdrLevel" }
    $tdrText = 'TdrDelay ' + $dText + ', TdrLevel ' + $lText
    if ($tdrLevel -eq 0) { Add-Row $sec 'GPU timeout (TDR)' 'WARN' $tdrText 'The GPU watchdog is switched off. A hung GPU job then freezes the machine instead of resetting the driver.' '' 20 }
    elseif ($null -ne $tdrDelay -and $tdrDelay -lt 2) { Add-Row $sec 'GPU timeout (TDR)' 'WARN' $tdrText 'The GPU time limit is shorter than the default. Long shader jobs can be reset by Windows as "device removed".' 'Remove the TdrDelay value or set it to 10 (needs admin).' 55 }
    else { Add-Row $sec 'GPU timeout (TDR)' 'OK' $tdrText 'Normal.' }

    $plan = Invoke-Limited 'powercfg.exe' '/getactivescheme' 10
    $planName = 'unknown'
    if ($plan -match '\(([^)]+)\)') { $planName = $Matches[1] }
    if ($planName -match 'saver|Energiespar|Economie') { Add-Row $sec 'Power plan' 'WARN' $planName 'A power saving plan slows the processor.' 'Choose Balanced or High performance in Settings, System, Power.' 40 }
    else { Add-Row $sec 'Power plan' 'OK' $planName 'Fine.' }

    $mp = $null
    try { $mp = Get-MpComputerStatus -ErrorAction Stop } catch {}
    if ($mp) {
        $excl = 'unknown'
        try {
            $pref = Get-MpPreference -ErrorAction Stop
            $paths = @($pref.ExclusionPath)
            if ($paths.Count -eq 1 -and "$($paths[0])" -like 'N/A*') { $excl = 'cannot be read without admin rights' }
            else { $excl = (@($paths | Where-Object { $_ }).Count.ToString() + ' folders, ' + @($pref.ExclusionProcess | Where-Object { $_ }).Count + ' processes') }
        } catch {}
        $rt = 'off'; if ($mp.RealTimeProtectionEnabled) { $rt = 'on' }
        Add-Row $sec 'Microsoft Defender' 'OK' ('real time protection ' + $rt + ', exclusions: ' + $excl) 'With real time protection on and no exclusion, Defender scans every file a title opens. That can slow loading a lot. Only counts are shown, never paths.'
    } else { Add-Row $sec 'Microsoft Defender' 'OK' 'not readable (another antivirus may be in charge)' 'No Defender status.' }
    try {
        $av = @(Get-CimInstance -Namespace root\SecurityCenter2 -ClassName AntiVirusProduct -ErrorAction Stop | ForEach-Object { $_.displayName })
        if ($av.Count -gt 0) { Add-Row $sec 'Antivirus products' 'OK' ($av -join ', ') 'Other antivirus products can block or slow the title.' }
    } catch {}
}

# Process mitigation of this process and of the parent chain (the 448 test needs it).
$sec = 'Mitigations'
Invoke-Check $sec {
    $rt = Get-RedirectionTrust $PID
    if ($rt -match 'ENFORCED') {
        Add-Row $sec 'RedirectionTrust, this process' 'FAIL' $rt 'This process refuses to follow junctions made by a normal user. The overlay of the launch tool fails with error 448 here.' 'Run the doctor and the title from a plain terminal outside the agent host.' 200
    } else {
        Add-Row $sec 'RedirectionTrust, this process' 'OK' $rt 'This process follows junctions.'
    }
    try {
        $pm = Get-ProcessMitigation -Id $PID -ErrorAction Stop
        $on = @()
        foreach ($n in 'Dep', 'Aslr', 'Cfg', 'DynamicCode', 'ChildProcess', 'ImageLoad', 'ExtensionPoint', 'StrictHandle', 'SystemCall', 'UserShadowStack', 'BinarySignature', 'FontDisable', 'Payload') {
            $o = $pm.$n
            if ($o) {
                foreach ($prop in $o.PSObject.Properties) {
                    if ("$($prop.Value)" -eq 'ON' -and $prop.Name -notmatch '^Audit') { $on += ($n + '.' + $prop.Name) }
                }
            }
        }
        $onText = 'none switched on'; if ($on.Count -gt 0) { $onText = $on -join ' ' }
        Add-Row $sec 'Other mitigations, this process' 'OK' $onText 'Switched on for this process. A child process inherits them.'
    } catch { Add-Row $sec 'Other mitigations, this process' 'WARN' 'Get-ProcessMitigation not usable' 'Could not list the other policies.' '' 5 }

    # Parent chain: names only, no command lines.
    $procs = @{}
    Get-CimInstance Win32_Process | ForEach-Object { $procs[[int]$_.ProcessId] = $_ }
    $chain = @()
    $cur = $PID
    for ($i = 0; $i -lt 8; $i++) {
        if (-not $procs.ContainsKey([int]$cur)) { break }
        $parent = [int]$procs[[int]$cur].ParentProcessId
        if ($parent -le 0 -or -not $procs.ContainsKey($parent)) { break }
        $chain += [pscustomobject]@{ Id = $parent; Name = $procs[$parent].Name }
        $cur = $parent
    }
    if ($chain.Count -eq 0) { Add-Row $sec 'Parent processes' 'OK' 'none found' 'The parent has already ended.' }
    $enforcedParent = $null
    $n = 0
    foreach ($c in $chain) {
        $n++
        $r = Get-RedirectionTrust $c.Id
        $st = 'OK'; $mean = 'Parent number ' + $n + '.'
        if ($r -match 'ENFORCED') {
            if (-not $enforcedParent) { $enforcedParent = $c.Name }
            $st = 'WARN'; $mean = 'This parent enforces RedirectionTrust, and a child process inherits it.'
        }
        Add-Row $sec ('Parent ' + $n + ': ' + $c.Name) $st ('RedirectionTrust ' + $r) $mean '' 60
    }
    if ($enforcedParent) {
        Add-Row $sec 'Host blocks junctions' 'FAIL' ('inherited from ' + $enforcedParent) 'A program above this one enforces RedirectionTrust. Titles started below it cannot use junctions (error 448).' 'Start the game from a plain terminal that the agent host did not start.' 180
    }
}

# =============================================================================================
# 5. Title paths (only what SOMMELIER_TITLE_* names)
# =============================================================================================
$sec = 'Title paths'
Invoke-Check $sec {
    $exe = $env:SOMMELIER_TITLE_EXE
    $dir = $env:SOMMELIER_TITLE_DIR
    if (-not $exe -and -not $dir) {
        Add-Row $sec 'Title settings' 'OK' 'SOMMELIER_TITLE_EXE and SOMMELIER_TITLE_DIR are not set' 'Nothing to check. Set them to check the title paths.'
        return
    }
    $checked = $null
    if ($exe) {
        if (Test-Path -LiteralPath $exe -PathType Leaf) {
            Add-Row $sec 'SOMMELIER_TITLE_EXE' 'OK' 'exists' 'The executable of the title is there.'
            $checked = Split-Path -Parent (Resolve-Path -LiteralPath $exe).Path
        } else {
            Add-Row $sec 'SOMMELIER_TITLE_EXE' 'FAIL' 'not found' 'The path in SOMMELIER_TITLE_EXE does not exist (a relative path is taken inside the worktree, so give a full path here).' 'Fix the path or install the title.' 120
        }
    }
    if ($dir) {
        if (Test-Path -LiteralPath $dir -PathType Container) {
            Add-Row $sec 'SOMMELIER_TITLE_DIR' 'OK' 'exists' 'The working folder of the title is there.'
            if (-not $checked) { $checked = (Resolve-Path -LiteralPath $dir).Path }
        } else {
            Add-Row $sec 'SOMMELIER_TITLE_DIR' 'FAIL' 'not found' 'The folder in SOMMELIER_TITLE_DIR does not exist.' 'Fix the path.' 100
        }
    }
    if ($env:SOMMELIER_TITLE_LOG) {
        $logParent = Split-Path -Parent $env:SOMMELIER_TITLE_LOG
        if ($logParent -and -not (Test-Path -LiteralPath $logParent -PathType Container)) {
            Add-Row $sec 'SOMMELIER_TITLE_LOG' 'WARN' 'its folder does not exist yet' 'The folder of the log file is created by the title or not at all.' '' 10
        } else { Add-Row $sec 'SOMMELIER_TITLE_LOG' 'OK' 'folder exists' 'The log file can be written there.' }
    }
    if ($checked) {
        Add-Hide $checked '<TITLE>'
        $drive = [IO.Path]::GetPathRoot($checked)
        $disk = Get-CimInstance Win32_LogicalDisk -Filter ("DeviceID='" + $drive.TrimEnd('\') + "'")
        if ($disk -and $disk.FileSystem -ne 'NTFS') {
            Add-Row $sec 'Title drive file system' 'FAIL' $disk.FileSystem 'Junctions and hard links need NTFS.' 'Put the title on an NTFS drive.' 150
        } elseif ($disk) { Add-Row $sec 'Title drive file system' 'OK' ('' + $disk.FileSystem + ' on ' + $drive.TrimEnd('\')) 'NTFS supports the links the launch tool makes.' }
    }
}


# =============================================================================================
# 6. Link tests (the 448 test)
# =============================================================================================
$sec = 'Link tests'
Invoke-Check $sec {
    $base = New-ProbeFolder $env:TEMP
    $tdir = Join-Path $base 'target'
    [void][IO.Directory]::CreateDirectory($tdir)
    $tfile = Join-Path $tdir 'probe.txt'
    [IO.File]::WriteAllText($tfile, 'guest doctor probe')
    $where = Join-Path $base 'links'
    [void][IO.Directory]::CreateDirectory($where)
    Test-LinkKind $sec 'Junction in temp folder' 'junction' $where $tdir $tfile 'probe.txt'
    Test-LinkKind $sec 'Directory symlink in temp folder' 'symdir' $where $tdir $tfile 'probe.txt'
    Test-LinkKind $sec 'File symlink in temp folder' 'symfile' $where $tdir $tfile ''
    Test-LinkKind $sec 'Hard link in temp folder' 'hard' $where $tdir $tfile ''
}


# =============================================================================================
# 7. Components
# =============================================================================================
$sec = 'Components'
Invoke-Check $sec {
    foreach ($s in 'GamingServices', 'GamingServicesNet', 'GameInputSvc') {
        $svc = Get-Service -Name $s -ErrorAction SilentlyContinue
        if (-not $svc) {
            $fix = 'Install Gaming Services from the Microsoft Store (needs the owner).'
            if ($s -eq 'GameInputSvc') { $fix = 'Install the Microsoft GameInput redistributable (needs the owner).' }
            Add-Row $sec ('Service ' + $s) 'FAIL' 'not installed' 'Without it a title can stop early.' $fix 110
        } else { Add-Row $sec ('Service ' + $s) 'OK' ([string]$svc.Status) 'Installed. A stopped service starts on demand.' }
    }
    $gs = $null
    try { $gs = Get-AppxPackage -Name 'Microsoft.GamingServices' -ErrorAction Stop | Select-Object -First 1 } catch {}
    if ($gs) { Add-Row $sec 'Gaming Services package' 'OK' ('version ' + $gs.Version) 'The Store package of Gaming Services.' }
    else { Add-Row $sec 'Gaming Services package' 'WARN' 'not found for this user' 'Gaming Services may be missing.' 'Install Gaming Services from the Microsoft Store.' 60 }
    $sys = Join-Path $env:SystemRoot 'System32'
    foreach ($f in 'xgameruntime.dll', 'GameInput.dll', 'd3d12.dll', 'dxgi.dll', 'd3d10warp.dll') {
        $v = Get-FileVersionText (Join-Path $sys $f)
        $mean = 'File version, for comparison.'
        if ($f -eq 'd3d10warp.dll') { $mean = 'WARP is the software DirectX renderer. Our tests use it as the fallback.' }
        if ($v) { Add-Row $sec ('System file ' + $f) 'OK' $v $mean }
        elseif ($f -eq 'd3d10warp.dll') { Add-Row $sec 'WARP' 'WARN' 'missing' 'The software renderer is missing. Shader tests on WARP cannot run.' '' 15 }
        else { Add-Row $sec ('System file ' + $f) 'WARN' 'missing' 'A system file the layer forwards to is missing.' '' 40 }
    }
    $d12 = Get-FileVersionText (Join-Path $sys 'D3D12\D3D12Core.dll')
    if ($d12) { Add-Row $sec 'System file D3D12Core.dll' 'OK' $d12 'File version, for comparison.' }

    # Visual C++ runtimes (the 14.x family covers 2015 to 2022).
    foreach ($arch in 'X64', 'X86') {
        $k = 'HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\' + $arch
        $inst = Get-RegValue $k 'Installed'
        $ver = Get-RegValue $k 'Version'
        if ($inst -eq 1) { Add-Row $sec ('Visual C++ runtime ' + $arch) 'OK' "$ver" 'Installed.' }
        elseif ($arch -eq 'X64') { Add-Row $sec 'Visual C++ runtime X64' 'WARN' 'not found' 'Our tools need the system runtime.' 'Install the Visual C++ Redistributable 2015 to 2022 (x64).' 45 }
        else { Add-Row $sec 'Visual C++ runtime X86' 'OK' 'not found' 'Only some old tools need it.' }
    }
}

$sec = 'Firewall'
Invoke-Check $sec {
    # The docs ask for an outbound block rule on the title. Only counts are shown.
    $titleExe = if ($env:SOMMELIER_TITLE_PROCESS) { $env:SOMMELIER_TITLE_PROCESS } elseif ($env:SOMMELIER_TITLE_EXE) { Split-Path -Leaf $env:SOMMELIER_TITLE_EXE } else { '' }
    if (-not $titleExe) { Add-Row $sec 'Offline block rule for the title' 'OK' 'skipped' 'Set SOMMELIER_TITLE_EXE or SOMMELIER_TITLE_PROCESS to check the offline block rule.'; return }
    $rules = @()
    try {
        $apps = Get-NetFirewallApplicationFilter -ErrorAction Stop | Where-Object { $_.Program -like ('*' + $titleExe) }
        foreach ($a in $apps) {
            $r = $a | Get-NetFirewallRule -ErrorAction SilentlyContinue
            if ($r) { $rules += $r }
        }
    } catch { Add-Row $sec 'Firewall rules' 'WARN' 'could not read the rules' 'The firewall rules cannot be listed on this machine without admin rights.' '' 10; return }
    $block = @($rules | Where-Object { $_.Direction -eq 'Outbound' -and $_.Action -eq 'Block' -and $_.Enabled -eq 'True' })
    if ($block.Count -gt 0) { Add-Row $sec 'Offline block rule for the title' 'OK' ($block.Count.ToString() + ' outbound block rule(s) on ' + $titleExe) 'The title cannot reach the network.' }
    else { Add-Row $sec 'Offline block rule for the title' 'WARN' 'none found' ('Our rules say the title stays offline. No block rule for ' + $titleExe + ' exists.') 'Add the outbound block rule from GUEST-SETUP.md (needs admin, ask the owner).' 65 }
}

# =============================================================================================
# 8. Tools
# =============================================================================================
$sec = 'Tools'
Invoke-Check $sec {
    # Is Decant built? Looks in the git checkout around the current folder.
    $top = ''
    try { $top = (& git rev-parse --show-toplevel 2>$null) } catch {}
    if ($top) {
        $built = @()
        foreach ($p in 'release', 'debug') {
            $f = Join-Path $top ('target\' + $p + '\decant-launch.exe')
            if (Test-Path -LiteralPath $f) { $built += $p }
        }
        if ($built.Count -gt 0) { Add-Row $sec 'Decant build' 'OK' ('decant-launch.exe in target: ' + ($built -join ', ')) 'The launch tool is built in this checkout.' }
        else { Add-Row $sec 'Decant build' 'WARN' 'no decant-launch.exe in target\release or target\debug' 'Decant is not built in this checkout.' 'Run cargo build --release in the Decant repository.' 30 }
    } else { Add-Row $sec 'Decant build' 'OK' 'not inside a git checkout' 'Run the doctor from the Decant repository to check the build.' }
    foreach ($t in 'git', 'rustc', 'cargo', 'python', 'node', 'claude', 'codex', 'gemini', 'agy') {
        $v = Get-ToolVersion $t
        if ($v) { Add-Row $sec $t 'OK' $v 'Found.' }
        else { Add-Row $sec $t 'OK' 'not found' 'Not installed or not in PATH.' }
    }
}

# =============================================================================================
# Clean up the probes, then write the report
# =============================================================================================
foreach ($p in $script:Probes) {
    try {
        # A link left over is removed first, so a recursive delete can never reach a target.
        foreach ($i in (Get-ChildItem -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue)) {
            if ($i.Attributes -band [IO.FileAttributes]::ReparsePoint) { Remove-Link $i.FullName ([bool]$i.PSIsContainer) }
        }
        if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue }
    } catch {}
}

if (-not $OutDir) { if ($PSScriptRoot) { $OutDir = $PSScriptRoot } else { $OutDir = (Get-Location).Path } }
try { if (-not (Test-Path -LiteralPath $OutDir)) { [void][IO.Directory]::CreateDirectory($OutDir) } } catch { $OutDir = $env:TEMP }
$outPath = Join-Path $OutDir $OutName

$fails = @($script:Rows | Where-Object { $_.Status -eq 'FAIL' }).Count
$warns = @($script:Rows | Where-Object { $_.Status -eq 'WARN' }).Count
$top = @($script:Rows | Where-Object { $_.Status -ne 'OK' } | Sort-Object -Property @{ Expression = 'Weight'; Descending = $true } | Select-Object -First 3)

# One line, no table bars, plain ASCII (Windows messages can come in the local code page).
function Esc([string]$s) { if ($null -eq $s) { return '' }; return ($s -replace '\|', '/' -replace '[\r\n]+', ' ' -replace '[^\x20-\x7E]', '?') }

$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine('# Guest doctor report')
[void]$sb.AppendLine('')
[void]$sb.AppendLine('Made by swarm/guest-doctor.ps1. Personal data is hidden (user, machine, home folder, e-mail, tokens).')
[void]$sb.AppendLine('')
[void]$sb.AppendLine(('Result: {0} FAIL, {1} WARN, {2} checks. Run time {3:N0} s. PowerShell {4}.' -f $fails, $warns, $script:Rows.Count, ((Get-Date) - $script:Started).TotalSeconds, $PSVersionTable.PSVersion))
[void]$sb.AppendLine('')
[void]$sb.AppendLine('## Summary')
[void]$sb.AppendLine('')
if ($top.Count -eq 0) { [void]$sb.AppendLine('No problem found by the checks of this script.') }
$i = 0
foreach ($t in $top) {
    $i++
    $fix = $t.Fix
    if (-not $fix) { $fix = 'Compare with our machine (guest-doctor-compare.py).' }
    [void]$sb.AppendLine(('{0}. [{1}] {2}: {3} What to do: {4}' -f $i, $t.Status, $t.Check, $t.Meaning, $fix))
}
[void]$sb.AppendLine('')
foreach ($section in ($script:Rows | ForEach-Object { $_.Section } | Select-Object -Unique)) {
    [void]$sb.AppendLine('## ' + $section)
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('| Status | Check | Value | Meaning |')
    [void]$sb.AppendLine('|---|---|---|---|')
    foreach ($r in ($script:Rows | Where-Object { $_.Section -eq $section })) {
        [void]$sb.AppendLine(('| {0} | {1} | {2} | {3} |' -f $r.Status, (Esc $r.Check), (Esc $r.Value), (Esc $r.Meaning)))
    }
    [void]$sb.AppendLine('')
}
# One last pass over the whole text, in case something slipped through.
$text = Hide $sb.ToString()
[IO.File]::WriteAllText($outPath, $text, (New-Object System.Text.UTF8Encoding($false)))

Write-Host ''
Write-Host ('Report written: ' + (Hide $outPath))
Write-Host ('{0} FAIL, {1} WARN.' -f $fails, $warns)
if ($top.Count -gt 0) {
    Write-Host 'The most important problems:'
    $i = 0
    foreach ($t in $top) {
        $i++
        $fix = $t.Fix
        if (-not $fix) { $fix = 'Compare with our machine.' }
        Write-Host ('  {0}. [{1}] {2}: {3}' -f $i, $t.Status, $t.Check, $t.Meaning)
        Write-Host ('     What to do: ' + $fix)
    }
} else { Write-Host 'No problem found.' }
exit 0

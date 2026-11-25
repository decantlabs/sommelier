# One picture of the title window (PrintWindow with full content, works for a D3D12 swap chain).
# Usage: shot.ps1 -Process <process name without .exe> -Out <file.png>
param([string]$Out, [string]$Process = '')
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class WinShot {
  [StructLayout(LayoutKind.Sequential)] public struct R { public int L, T, Rr, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
}
'@
$p = Get-Process $Process -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { "shot: no title window"; exit 1 }
$r = New-Object WinShot+R
if (-not [WinShot]::GetWindowRect($p.MainWindowHandle, [ref]$r)) { "shot: no window rectangle"; exit 1 }
$w = $r.Rr - $r.L; $h = $r.B - $r.T
if ($w -le 0 -or $h -le 0) { "shot: empty window"; exit 1 }
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
[WinShot]::PrintWindow($p.MainWindowHandle, $dc, 2) | Out-Null
$g.ReleaseHdc($dc); $g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
# How many of about 10,000 sampled pixels are not black, so a log reader knows without opening the file.
$lit = 0; $n = 0
$sy = [Math]::Max(1, [int]($h / 100)); $sx = [Math]::Max(1, [int]($w / 100))
for ($y = 0; $y -lt $h; $y += $sy) { for ($x = 0; $x -lt $w; $x += $sx) { $c = $bmp.GetPixel($x, $y); $n++; if ($c.R + $c.G + $c.B -gt 24) { $lit++ } } }
$bmp.Dispose()
"shot: ${w}x${h}, $lit of $n sampled pixels are not black"

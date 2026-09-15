# Build packaging\app.ico (and a preview sheet) from a master PNG.
#
# ASCII ONLY: PowerShell 5.1 reads a .ps1 without a BOM as ANSI, and any
# non-ASCII byte inside the script breaks parsing.
#
# Why not a Rust crate: the project must stay dependency-free, so the icon is
# baked offline here and committed as a binary. Re-run if the art changes:
#
#   powershell -ExecutionPolicy Bypass -File packaging\make-ico.ps1 -Src <png>
param(
  [string]$Src = (Join-Path $PSScriptRoot 'app-icon.png'),
  [string]$Out = (Join-Path $PSScriptRoot 'app.ico'),
  [string]$Sheet = (Join-Path $PSScriptRoot 'icon-preview.png')
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace Probe -Name Ico -MemberDefinition @'
[DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
public static extern IntPtr LoadImage(IntPtr hInst, string name, uint type, int cx, int cy, uint fuLoad);
[DllImport("user32.dll", SetLastError=true)]
public static extern bool DestroyIcon(IntPtr h);
'@

# Windows picks a different frame of the .ico depending on where it draws:
#   16 small list / title bar, 20 125%, 24 150%, 32 taskbar + desktop medium,
#   40 250%, 48 explorer medium + alt-tab, 64 400%, 96/128 explorer large,
#   256 explorer extra large (the installer shows this one too). 256 is the cap.
$sizes  = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
# <= this many pixels: stored as DIB (classic, every API reads it).
# Above: stored as PNG (smaller, fine on Vista+ and on the shell APIs).
$dibMax = 64

function New-Scaled([System.Drawing.Image]$src, [int]$size) {
  # shrink in halving steps first: bicubic looks bad when it jumps 1254 -> 16
  $cur = [System.Drawing.Bitmap]::new($src)
  while ($cur.Width -gt $size * 2) {
    $nw = [Math]::Max($size, [int][Math]::Floor($cur.Width / 2))
    $nh = [Math]::Max($size, [int][Math]::Floor($cur.Height / 2))
    $tmp = [System.Drawing.Bitmap]::new($nw, $nh, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($tmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.DrawImage($cur, 0, 0, $nw, $nh)
    $g.Dispose(); $cur.Dispose(); $cur = $tmp
  }
  $bmp = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
  $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
  $g.Clear([System.Drawing.Color]::Transparent)
  $g.DrawImage($cur, 0, 0, $size, $size)
  $g.Dispose(); $cur.Dispose()
  return $bmp
}

function Get-Pixels([System.Drawing.Bitmap]$bmp) {
  $r = [System.Drawing.Rectangle]::new(0, 0, $bmp.Width, $bmp.Height)
  $d = $bmp.LockBits($r, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $bytes = [byte[]]::new($d.Stride * $bmp.Height)
  [System.Runtime.InteropServices.Marshal]::Copy($d.Scan0, $bytes, 0, $bytes.Length)
  $bmp.UnlockBits($d)
  return @{ b = $bytes; stride = $d.Stride }
}

function Get-Png([System.Drawing.Bitmap]$bmp) {
  $ms = [System.IO.MemoryStream]::new()
  $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  $a = $ms.ToArray(); $ms.Dispose()
  return ,$a
}

function New-Dib([System.Drawing.Bitmap]$bmp) {
  $size = $bmp.Width
  $px = Get-Pixels $bmp
  $ms = [System.IO.MemoryStream]::new()
  $w = [System.IO.BinaryWriter]::new($ms)
  $w.Write([int]40); $w.Write([int]$size); $w.Write([int]($size * 2))
  $w.Write([int16]1); $w.Write([int16]32)
  $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0)
  # XOR bitmap: 32bpp BGRA, bottom-up
  for ($y = $size - 1; $y -ge 0; $y--) {
    for ($x = 0; $x -lt $size; $x++) {
      $i = $y * $px.stride + $x * 4
      $w.Write($px.b[$i]); $w.Write($px.b[$i+1]); $w.Write($px.b[$i+2]); $w.Write($px.b[$i+3])
    }
  }
  # AND mask: 1bpp, rows padded to 4 bytes, bottom-up. 1 = transparent.
  $mstride = [int][Math]::Floor(($size + 31) / 32) * 4
  $mask = [byte[]]::new($mstride * $size)
  for ($y = 0; $y -lt $size; $y++) {
    for ($x = 0; $x -lt $size; $x++) {
      if ($px.b[$y * $px.stride + $x * 4 + 3] -lt 128) {
        $idx = ($size - 1 - $y) * $mstride + [int][Math]::Floor($x / 8)
        $mask[$idx] = $mask[$idx] -bor [byte](1 -shl (7 - ($x % 8)))
      }
    }
  }
  $w.Write($mask)
  $w.Flush()
  $a = $ms.ToArray(); $w.Dispose(); $ms.Dispose()
  return ,$a
}

Write-Output "master = $Src"
$master = [System.Drawing.Image]::FromFile($Src)
Write-Output ("  {0}x{1}  {2}" -f $master.Width, $master.Height, $master.PixelFormat)

$frames = @()
foreach ($s in $sizes) {
  $bmp = New-Scaled $master $s
  if ($s -le $dibMax) { $data = New-Dib $bmp } else { $data = Get-Png $bmp }
  $frames += @{ size = $s; data = [byte[]]$data }
  Write-Output ("  {0,4}px {1,8} bytes  {2}" -f $s, $data.Length, $(if ($s -le $dibMax) { "DIB" } else { "PNG" }))
  $bmp.Dispose()
}
$master.Dispose()

$ms = [System.IO.MemoryStream]::new()
$w = [System.IO.BinaryWriter]::new($ms)
$w.Write([int16]0); $w.Write([int16]1); $w.Write([int16]$frames.Count)
$off = 6 + 16 * $frames.Count
foreach ($f in $frames) {
  $s = $f.size
  $dim = [byte]$(if ($s -ge 256) { 0 } else { $s })
  $w.Write($dim); $w.Write($dim); $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([int16]1); $w.Write([int16]32)
  $w.Write([int]$f.data.Length); $w.Write([int]$off)
  $off += $f.data.Length
}
foreach ($f in $frames) { $w.Write([byte[]]$f.data) }
$w.Flush()
$all = $ms.ToArray()
$w.Dispose(); $ms.Dispose()
[System.IO.File]::WriteAllBytes($Out, $all)
Write-Output ("wrote {0}  ({1} bytes; expect {2})" -f $Out, (Get-Item $Out).Length, $off)

# ---- preview sheet: every frame on a checkerboard, small ones upscaled with
# ---- nearest neighbour so the actual pixels are visible
$cell = 150; $cols = 5
$sheetBmp = [System.Drawing.Bitmap]::new([int]($cell * $cols), [int]($cell * 2), [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($sheetBmp)
$g.Clear([System.Drawing.Color]::FromArgb(255, 60, 60, 60))
$font = [System.Drawing.Font]::new('Segoe UI', 9)
$brush = [System.Drawing.Brushes]::White
for ($i = 0; $i -lt $frames.Count; $i++) {
  $cx = ($i % $cols) * $cell
  $cy = [int][Math]::Floor($i / $cols) * $cell
  for ($q = 0; $q -lt 10; $q++) {
    for ($p = 0; $p -lt 10; $p++) {
      $c = if ((($q + $p) % 2) -eq 0) { [System.Drawing.Color]::FromArgb(255, 200, 200, 200) } else { [System.Drawing.Color]::FromArgb(255, 130, 130, 130) }
      $sb = [System.Drawing.SolidBrush]::new($c)
      $g.FillRectangle($sb, ($cx + $p * 12), ($cy + $q * 12), 12, 12)
      $sb.Dispose()
    }
  }
  # Read the frame back out of the .ico we just wrote, through Win32 LoadImage --
  # that is what the shell does, and it also fails loudly if a frame is bad.
  # (System.Drawing's own Icon(path,size,size) cannot decode PNG frames; it
  # returns noise, which is a .NET quirk, not a bad file.)
  $h = [Probe.Ico]::LoadImage([IntPtr]::Zero, $Out, 1, $frames[$i].size, $frames[$i].size, 0x10)
  if ($h -eq [IntPtr]::Zero) { throw ("LoadImage cannot read the " + $frames[$i].size + "px frame") }
  $ic = [System.Drawing.Icon]::FromHandle($h)
  $img = $ic.ToBitmap()
  [Probe.Ico]::DestroyIcon($h) | Out-Null
  Write-Output ("  read back {0}px -> {1}x{2}" -f $frames[$i].size, $img.Width, $img.Height)
  $show = [int][Math]::Min(96, $frames[$i].size * 4)
  $g.InterpolationMode = if ($frames[$i].size -lt 48) { [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor } else { [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic }
  $g.DrawImage($img, [int]($cx + 6), [int]($cy + 6), $show, $show)
  $g.DrawString(("{0}px" -f $frames[$i].size), $font, $brush, [single]($cx + 8), [single]($cy + $cell - 22))
  $img.Dispose(); $ic.Dispose()
}
$g.Dispose()
$sheetBmp.Save($Sheet, [System.Drawing.Imaging.ImageFormat]::Png)
$sheetBmp.Dispose()
Write-Output ("sheet  = {0}" -f $Sheet)

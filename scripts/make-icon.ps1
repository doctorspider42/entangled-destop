# Renders the Entangled Desktop icon and packs it into installer/entangled.ico.
#
# The mark follows the manager's quantum theme (apps/manager/src/theme.rs):
# a deep-space disc (BG_PANEL) carrying two entangled elliptical orbits, one
# cyan (#35e2f0) and one violet (#a86bff), each with a particle on it — the
# same idea `logo::paint_mark` draws procedurally inside the GUI.
#
# Windows-only build-time tooling (System.Drawing); the produced .ico is
# committed, so nothing at build or run time depends on this script. Run it
# again only when the mark changes:
#
#   pwsh -File scripts/make-icon.ps1

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$repo = Split-Path -Parent $PSScriptRoot
$outIco = Join-Path $repo 'installer/entangled.ico'
New-Item -ItemType Directory -Force (Split-Path -Parent $outIco) | Out-Null

# Theme tokens.
$bgPanel = [System.Drawing.Color]::FromArgb(255, 0x0b, 0x10, 0x21)
$stroke  = [System.Drawing.Color]::FromArgb(255, 0x2b, 0x3d, 0x63)   # STROKE_STRONG
$cyan    = [System.Drawing.Color]::FromArgb(255, 0x35, 0xe2, 0xf0)
$violet  = [System.Drawing.Color]::FromArgb(255, 0xa8, 0x6b, 0xff)

function Render-Png([int]$size) {
    $bmp = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)

    $s = $size / 256.0            # everything below is drawn in a 256-unit space
    $c = $size / 2.0

    # The deep-space disc with a faint themed rim.
    $inset = 6 * $s
    $discBrush = [System.Drawing.SolidBrush]::new($bgPanel)
    $g.FillEllipse($discBrush, $inset, $inset, $size - 2 * $inset, $size - 2 * $inset)
    $rimWidth = [Math]::Max(1.0, 6 * $s)
    $rimPen = [System.Drawing.Pen]::new($stroke, [float]$rimWidth)
    $g.DrawEllipse($rimPen, $inset + $rimWidth / 2, $inset + $rimWidth / 2,
        $size - 2 * $inset - $rimWidth, $size - 2 * $inset - $rimWidth)

    # Two entangled orbits: the same ellipse rotated +32 / -32 degrees.
    $orbitW = 176 * $s
    $orbitH = 78 * $s
    $penWidth = [Math]::Max(1.0, 11 * $s)
    foreach ($orbit in @(@{Angle = -32; Color = $cyan }, @{Angle = 32; Color = $violet })) {
        $state = $g.Save()
        $g.TranslateTransform($c, $c)
        $g.RotateTransform($orbit.Angle)
        # A soft glow pass under the crisp stroke (small sizes skip it).
        if ($size -ge 48) {
            $glowColor = [System.Drawing.Color]::FromArgb(70, $orbit.Color)
            $glow = [System.Drawing.Pen]::new($glowColor, [float]($penWidth * 2.4))
            $g.DrawEllipse($glow, -$orbitW / 2, -$orbitH / 2, $orbitW, $orbitH)
            $glow.Dispose()
        }
        $pen = [System.Drawing.Pen]::new($orbit.Color, [float]$penWidth)
        $g.DrawEllipse($pen, -$orbitW / 2, -$orbitH / 2, $orbitW, $orbitH)
        $pen.Dispose()
        $g.Restore($state)
    }

    # One particle per orbit, sitting on the ellipse (parametric point at t,
    # rotated with the orbit) — the "entangled pair".
    foreach ($p in @(
            @{Angle = -32; Color = $cyan;   T = 0.62 },
            @{Angle = 32;  Color = $violet; T = 3.76 })) {
        $x = ($orbitW / 2) * [Math]::Cos($p.T)
        $y = ($orbitH / 2) * [Math]::Sin($p.T)
        $a = $p.Angle * [Math]::PI / 180.0
        $rx = $c + $x * [Math]::Cos($a) - $y * [Math]::Sin($a)
        $ry = $c + $x * [Math]::Sin($a) + $y * [Math]::Cos($a)
        $r = [Math]::Max(1.5, 13 * $s)
        if ($size -ge 48) {
            $halo = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(60, $p.Color))
            $g.FillEllipse($halo, $rx - 1.8 * $r, $ry - 1.8 * $r, 3.6 * $r, 3.6 * $r)
            $halo.Dispose()
        }
        $dot = [System.Drawing.SolidBrush]::new($p.Color)
        $g.FillEllipse($dot, $rx - $r, $ry - $r, 2 * $r, 2 * $r)
        $dot.Dispose()
    }

    $g.Dispose()
    $ms = [System.IO.MemoryStream]::new()
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}

# Pack the PNG frames into a .ico. PNG-compressed entries are valid for every
# size on Windows Vista and later; Explorer, the taskbar and Inno Setup all
# read this layout.
$sizes = 256, 128, 64, 48, 32, 16
$frames = foreach ($size in $sizes) { , (Render-Png $size) }

$ms = [System.IO.MemoryStream]::new()
$w = [System.IO.BinaryWriter]::new($ms)
$w.Write([uint16]0)               # reserved
$w.Write([uint16]1)               # type: icon
$w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $size = $sizes[$i]
    $bytes = $frames[$i]
    $w.Write([byte]($(if ($size -ge 256) { 0 } else { $size })))  # width, 0 = 256
    $w.Write([byte]($(if ($size -ge 256) { 0 } else { $size })))  # height
    $w.Write([byte]0)             # palette
    $w.Write([byte]0)             # reserved
    $w.Write([uint16]1)           # planes
    $w.Write([uint16]32)          # bpp
    $w.Write([uint32]$bytes.Length)
    $w.Write([uint32]$offset)
    $offset += $bytes.Length
}
foreach ($bytes in $frames) { $w.Write($bytes) }
$w.Flush()
[System.IO.File]::WriteAllBytes($outIco, $ms.ToArray())
$w.Dispose()

Write-Host "wrote $outIco ($((Get-Item $outIco).Length) bytes, sizes: $($sizes -join ', '))"

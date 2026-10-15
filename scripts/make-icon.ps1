# Renders the Entangled Desktop icon and packs it into installer/entangled.ico.
#
# The mark is the manager's "linked pair" (apps/manager/src/logo.rs): two rings
# that pass over and under each other — the left one on top at the upper
# crossing, the right one on top at the lower — on a rounded tile in the
# Deep Field palette (apps/manager/src/theme.rs). Same geometry as
# `logo::paint_mark` and `logo::app_icon`, so the file icon, the taskbar and
# the window inside the app all show one mark.
#
# The .ico is committed and is what both the installer and the manager's own
# executable carry (apps/manager/build.rs embeds it), so nothing at build or
# run time depends on this script. Run it again only when the mark changes:
#
#   powershell -File scripts/make-icon.ps1

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$repo = Split-Path -Parent $PSScriptRoot
$outIco = Join-Path $repo 'installer/entangled.ico'
New-Item -ItemType Directory -Force (Split-Path -Parent $outIco) | Out-Null

# Theme tokens.
$tile   = [System.Drawing.Color]::FromArgb(255, 0x11, 0x16, 0x1e)   # CARD
$border = [System.Drawing.Color]::FromArgb(255, 0x2c, 0x36, 0x45)   # STROKE_STRONG
$light  = [System.Drawing.Color]::FromArgb(255, 0xe4, 0xe8, 0xee)   # TEXT
$ice    = [System.Drawing.Color]::FromArgb(255, 0x8c, 0xc8, 0xe0)   # ACCENT

# The mark's own units (logo.rs): a 48-unit square, rings of radius 12 whose
# centres sit 5 units either side of the middle.
$units = 48.0
$radius = 12.0
$offset = 5.0
# Where the rings cross, seen from the left ring's centre (degrees, y down).
$cross = [Math]::Atan2([Math]::Sqrt($radius * $radius - $offset * $offset), $offset) * 180.0 / [Math]::PI
$arcHalf = 20.0
# The coloured arc runs a little past its knockout at both ends, so the two
# anti-aliased ends never meet on the same pixel and leave a seam.
$overlap = 4.0

function Stroke-Units([int]$size) {
    # Heavier as it shrinks, so the over/under gap still reads at 16 px.
    if ($size -ge 128) { return 3.6 }
    if ($size -ge 48) { return 4.0 }
    if ($size -ge 32) { return 4.6 }
    return 5.2
}

function Rounded-Path([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = [System.Drawing.Drawing2D.GraphicsPath]::new()
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function Render-Png([int]$size) {
    $bmp = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    # The tile: rounded, 22% corners, a one-pixel border.
    $edge = [Math]::Max(1.0, $size / 64.0)
    $path = Rounded-Path ($edge / 2) ($edge / 2) ($size - $edge) ($size - $edge) ($size * 0.22)
    $fill = [System.Drawing.SolidBrush]::new($tile)
    $g.FillPath($fill, $path)
    $pen = [System.Drawing.Pen]::new($border, [float]$edge)
    $g.DrawPath($pen, $path)
    $fill.Dispose(); $pen.Dispose(); $path.Dispose()

    # The mark fills 72% of the tile.
    $unit = $size * 0.72 / $units
    $c = $size / 2.0
    $r = $radius * $unit
    $width = (Stroke-Units $size) * $unit
    $gap = 1.8 * $width / 3.4
    $left = $c - $offset * $unit
    $right = $c + $offset * $unit

    $lightPen = [System.Drawing.Pen]::new($light, [float]$width)
    $icePen = [System.Drawing.Pen]::new($ice, [float]$width)
    $knockout = [System.Drawing.Pen]::new($tile, [float]($width + 2 * $gap))

    # The light ring drawn last is on top at both crossings…
    $g.DrawEllipse($icePen, [float]($right - $r), [float]($c - $r), [float](2 * $r), [float](2 * $r))
    $g.DrawEllipse($lightPen, [float]($left - $r), [float]($c - $r), [float](2 * $r), [float](2 * $r))
    # …then each crossing gets the ring that belongs on top there, over a
    # knockout in the tile colour: left over at the top, right over at the
    # bottom. That is what makes them linked.
    $topStart = -$cross - $arcHalf
    $g.DrawArc($knockout, [float]($left - $r), [float]($c - $r), [float](2 * $r), [float](2 * $r), [float]$topStart, [float](2 * $arcHalf))
    $g.DrawArc($lightPen, [float]($left - $r), [float]($c - $r), [float](2 * $r), [float](2 * $r), [float]($topStart - $overlap), [float](2 * $arcHalf + 2 * $overlap))
    $bottomStart = 180.0 - $cross - $arcHalf
    $g.DrawArc($knockout, [float]($right - $r), [float]($c - $r), [float](2 * $r), [float](2 * $r), [float]$bottomStart, [float](2 * $arcHalf))
    $g.DrawArc($icePen, [float]($right - $r), [float]($c - $r), [float](2 * $r), [float](2 * $r), [float]($bottomStart - $overlap), [float](2 * $arcHalf + 2 * $overlap))
    $lightPen.Dispose(); $icePen.Dispose(); $knockout.Dispose()

    $g.Dispose()
    $ms = [System.IO.MemoryStream]::new()
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}

# Pack the PNG frames into a .ico. PNG-compressed entries are valid for every
# size on Windows Vista and later; Explorer, the taskbar, the resource compiler
# and Inno Setup all read this layout.
$sizes = 256, 128, 64, 48, 32, 24, 16
$frames = foreach ($size in $sizes) { , (Render-Png $size) }

$ms = [System.IO.MemoryStream]::new()
$w = [System.IO.BinaryWriter]::new($ms)
$w.Write([uint16]0)               # reserved
$w.Write([uint16]1)               # type: icon
$w.Write([uint16]$sizes.Count)
$offsetBytes = 6 + 16 * $sizes.Count
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
    $w.Write([uint32]$offsetBytes)
    $offsetBytes += $bytes.Length
}
foreach ($bytes in $frames) { $w.Write($bytes) }
$w.Flush()
[System.IO.File]::WriteAllBytes($outIco, $ms.ToArray())
$w.Dispose()

Write-Host "wrote $outIco ($((Get-Item $outIco).Length) bytes, sizes: $($sizes -join ', '))"

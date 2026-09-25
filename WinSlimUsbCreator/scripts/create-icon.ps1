$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$assetDir = Join-Path $PSScriptRoot '..\assets'
New-Item -ItemType Directory -Force -Path $assetDir | Out-Null
$pngPath = Join-Path $assetDir 'icon.png'
$icoPath = Join-Path $assetDir 'icon.ico'

function RoundedPath([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

$bitmap = New-Object System.Drawing.Bitmap 256, 256, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.Clear([System.Drawing.Color]::Transparent)

$tile = RoundedPath 8 8 240 240 64
$tileBrush = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
    [System.Drawing.Rectangle]::new(8, 8, 240, 240),
    [System.Drawing.Color]::FromArgb(255, 252, 253, 255),
    [System.Drawing.Color]::FromArgb(255, 204, 209, 217),
    [single]90
)
$g.FillPath($tileBrush, $tile)
$g.DrawPath((New-Object System.Drawing.Pen ([System.Drawing.Color]::White, 6)), $tile)

# Fill more of the tile while keeping the complete USB silhouette centered.
$deviceTransform = [System.Drawing.Drawing2D.Matrix]::new([single]1.27, [single]0, [single]0, [single]1.13, [single]-34.56, [single]-18.64)
$g.Transform = $deviceTransform

$connector = RoundedPath 100 42 56 43 7
$g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 36, 38, 42))), $connector)
$pinBrush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 209, 214, 220))
foreach ($pinX in @(106, 118, 130, 142)) {
    $g.FillRectangle($pinBrush, $pinX, 49, 7, 19)
}

$body = RoundedPath 66 76 124 142 25
$bodyBrush = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
    [System.Drawing.Rectangle]::new(66, 76, 124, 142),
    [System.Drawing.Color]::FromArgb(255, 49, 51, 55),
    [System.Drawing.Color]::FromArgb(255, 26, 27, 29),
    [single]90
)
$g.FillPath($bodyBrush, $body)
$g.DrawPath((New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 95, 98, 104), 3)), $body)

# Rounded block W based on the supplied WinSlim mark.
$w = New-Object System.Drawing.Drawing2D.GraphicsPath
$w.StartFigure()
$w.AddBezier(84,100,84,96,87,94,91,94)
$w.AddLine(91,94,103,94)
$w.AddBezier(103,94,107,94,109,97,109,101)
$w.AddLine(109,101,109,139)
$w.AddLine(109,139,119,119)
$w.AddBezier(119,119,122,113,125,110,128,110)
$w.AddBezier(128,110,131,110,134,113,137,119)
$w.AddLine(137,119,147,139)
$w.AddLine(147,139,147,101)
$w.AddBezier(147,101,147,97,150,94,154,94)
$w.AddLine(154,94,165,94)
$w.AddBezier(165,94,169,94,172,97,172,101)
$w.AddLine(172,101,172,147)
$w.AddBezier(172,147,172,158,168,172,164,180)
$w.AddBezier(164,180,162,184,159,186,154,186)
$w.AddLine(154,186,143,186)
$w.AddBezier(143,186,139,186,137,184,136,180)
$w.AddLine(136,180,128,156)
$w.AddLine(128,156,120,180)
$w.AddBezier(120,180,119,184,117,186,113,186)
$w.AddLine(113,186,102,186)
$w.AddBezier(102,186,97,186,94,184,92,180)
$w.AddBezier(92,180,88,172,84,158,84,147)
$w.AddLine(84,147,84,100)
$w.CloseFigure()
$wPosition = [System.Drawing.Drawing2D.Matrix]::new([single]1, [single]0, [single]0, [single]1, [single]0, [single]6)
$w.Transform($wPosition)
$g.DrawPath((New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(55, 255, 255, 255), 9)), $w)
$g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)), $w)
$g.ResetTransform()
$bitmap.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bitmap.Dispose()

$image = [System.IO.File]::ReadAllBytes($pngPath)
$stream = [System.IO.File]::Create($icoPath)
$writer = New-Object System.IO.BinaryWriter($stream)
$writer.Write([uint16]0)
$writer.Write([uint16]1)
$writer.Write([uint16]1)
$writer.Write([byte]0)
$writer.Write([byte]0)
$writer.Write([byte]0)
$writer.Write([byte]0)
$writer.Write([uint16]1)
$writer.Write([uint16]32)
$writer.Write([uint32]$image.Length)
$writer.Write([uint32]22)
$writer.Write($image)
$writer.Dispose()
Write-Host "Iconos generados: $pngPath y $icoPath"

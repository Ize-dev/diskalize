# Generates assets/diskalize.ico — a sunburst ring on a dark rounded tile.
# Run once; the .ico is committed so a normal build needs no tooling.
Add-Type -AssemblyName System.Drawing

$sizes = 16, 24, 32, 48, 64, 128, 256
$root = Split-Path -Parent $PSScriptRoot
$assets = Join-Path $root "assets"
New-Item -ItemType Directory -Force $assets | Out-Null

# Ring segments: sweep angle + colour, mirroring the app's sunburst palette.
$ring1 = @(
    @(  -90, 104, '#4D9BFF'),
    @(   14,  74, '#5CD68A'),
    @(   88,  62, '#F2C14E'),
    @(  150,  58, '#E8734A'),
    @(  208,  46, '#C062D6'),
    @(  254,  16, '#4D9BFF')
)
$ring2 = @(
    @(  -90,  58, '#7FB8FF'),
    @(  -32,  46, '#2F7FE0'),
    @(   14,  74, '#84E3AA'),
    @(   88,  62, '#F7D683'),
    @(  150,  58, '#F09A78'),
    @(  208,  62, '#D592E6')
)

function New-Frame([int]$px) {
    $bmp = New-Object System.Drawing.Bitmap $px, $px, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)

    $s = $px / 256.0

    # Rounded dark tile.
    $r = [int](52 * $s)
    $pad = [int](6 * $s)
    $box = New-Object System.Drawing.Rectangle $pad, $pad, ($px - 2 * $pad), ($px - 2 * $pad)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc($box.Left, $box.Top, $d, $d, 180, 90)
    $path.AddArc($box.Right - $d, $box.Top, $d, $d, 270, 90)
    $path.AddArc($box.Right - $d, $box.Bottom - $d, $d, $d, 0, 90)
    $path.AddArc($box.Left, $box.Bottom - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    $bg = New-Object System.Drawing.SolidBrush ([System.Drawing.ColorTranslator]::FromHtml('#171A20'))
    $g.FillPath($bg, $path)

    $cx = $px / 2.0
    $cy = $px / 2.0

    function Draw-Ring($segs, $outer, $inner, $gapDeg) {
        foreach ($seg in $segs) {
            $start = [double]$seg[0]
            $sweep = [double]$seg[1] - $gapDeg
            if ($sweep -le 1) { continue }
            $col = [System.Drawing.ColorTranslator]::FromHtml($seg[2])
            $br = New-Object System.Drawing.SolidBrush $col
            $p2 = New-Object System.Drawing.Drawing2D.GraphicsPath
            $o = New-Object System.Drawing.RectangleF ($cx - $outer), ($cy - $outer), (2 * $outer), (2 * $outer)
            $i = New-Object System.Drawing.RectangleF ($cx - $inner), ($cy - $inner), (2 * $inner), (2 * $inner)
            $p2.AddArc($o, $start, $sweep)
            $p2.AddArc($i, $start + $sweep, -$sweep)
            $p2.CloseFigure()
            $g.FillPath($br, $p2)
            $br.Dispose(); $p2.Dispose()
        }
    }

    if ($px -le 24) {
        # At menu size the two-ring version turns to mush: one ring, four wedges.
        $small = @(
            @( -90, 90, '#4D9BFF'),
            @(   0, 90, '#5CD68A'),
            @(  90, 90, '#F2C14E'),
            @( 180, 90, '#E8734A')
        )
        Draw-Ring $small (100 * $s) (40 * $s) 8
        $hub = New-Object System.Drawing.SolidBrush ([System.Drawing.ColorTranslator]::FromHtml('#0F1116'))
        $hr = 34 * $s
        $g.FillEllipse($hub, ($cx - $hr), ($cy - $hr), (2 * $hr), (2 * $hr))
        $hub.Dispose()
    } else {
        Draw-Ring $ring2 (104 * $s) (69 * $s) 3
        Draw-Ring $ring1 (66 * $s) (34 * $s) 3
        $hub = New-Object System.Drawing.SolidBrush ([System.Drawing.ColorTranslator]::FromHtml('#0F1116'))
        $hr = 30 * $s
        $g.FillEllipse($hub, ($cx - $hr), ($cy - $hr), (2 * $hr), (2 * $hr))
        $hub.Dispose()
    }

    $g.Dispose()
    return $bmp
}

# Assemble a PNG-compressed .ico (supported since Vista).
$frames = @()
foreach ($px in $sizes) {
    $bmp = New-Frame $px
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $frames += , @{ px = $px; bytes = $ms.ToArray() }
    $ms.Dispose(); $bmp.Dispose()
}

$out = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $out
$w.Write([UInt16]0)                 # reserved
$w.Write([UInt16]1)                 # type: icon
$w.Write([UInt16]$frames.Count)
$offset = 6 + 16 * $frames.Count
foreach ($f in $frames) {
    $dim = if ($f.px -ge 256) { 0 } else { $f.px }
    $w.Write([byte]$dim)            # width
    $w.Write([byte]$dim)            # height
    $w.Write([byte]0)               # palette
    $w.Write([byte]0)               # reserved
    $w.Write([UInt16]1)             # planes
    $w.Write([UInt16]32)            # bpp
    $w.Write([UInt32]$f.bytes.Length)
    $w.Write([UInt32]$offset)
    $offset += $f.bytes.Length
}
foreach ($f in $frames) { $w.Write($f.bytes) }
$w.Flush()

$path = Join-Path $assets "diskalize.ico"
[IO.File]::WriteAllBytes($path, $out.ToArray())
$w.Dispose(); $out.Dispose()

"$path  ($([math]::Round((Get-Item $path).Length / 1KB, 1)) KB, $($frames.Count) Größen)"

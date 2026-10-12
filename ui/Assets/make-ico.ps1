$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$ico = Join-Path $here "wsearch.ico"
$png256 = Join-Path $here "wsearch-256.png"
$magick = "C:\Program Files\ImageMagick-7.1.2-Q16-HDRI\magick.exe"
$tmp = Join-Path $here "tmp"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$master = Join-Path $tmp "256.png"

# Diagonal Fluent blue on a Win11 squircle (do not rotate the canvas)
& $magick -size 256x256 xc: -sparse-color Barycentric "16,16 #60CDFF 240,240 #005A9E" (Join-Path $tmp "grad.png")
& $magick -size 256x256 xc:none -fill white -draw "roundrectangle 16,16 240,240 56,56" (Join-Path $tmp "mask.png")
& $magick (Join-Path $tmp "grad.png") (Join-Path $tmp "mask.png") -compose CopyOpacity -composite (Join-Path $tmp "body.png")

# stacked files + magnifier
& $magick (Join-Path $tmp "body.png") -fill "#ffffff38" -draw "roundrectangle 64,70 118,138 8,8" -fill "#ffffff62" -draw "roundrectangle 76,82 130,150 8,8" -stroke white -strokewidth 16 -fill none -draw "circle 126,120 126,164" -draw "stroke-linecap round line 158,154 198,198" PNG32:$master

if (-not (Test-Path $master)) { throw "master missing" }
Copy-Item $master $png256 -Force

$sizes = 16,20,24,32,40,48,64,128,256
$pngs = @()
foreach ($s in $sizes) {
  $out = Join-Path $tmp "$s.png"
  & $magick $master -filter Lanczos -resize "${s}x${s}" PNG32:$out
  $pngs += $out
}
& $magick @pngs $ico
if (-not (Test-Path $ico)) { throw "ico missing" }
Write-Output ("ICO=" + (Get-Item $ico).Length)
Remove-Item -Recurse -Force $tmp

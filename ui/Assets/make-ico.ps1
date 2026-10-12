$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$ico = Join-Path $here "wsearch.ico"
$png256 = Join-Path $here "wsearch-256.png"
$magick = "C:\Program Files\ImageMagick-7.1.2-Q16-HDRI\magick.exe"
$tmp = Join-Path $here "tmp"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

# Full-bleed plate: Windows already pads the taskbar cell. Do not inset the squircle.
$master = Join-Path $tmp "256.png"
$grad = Join-Path $tmp "grad.png"
$mask = Join-Path $tmp "mask.png"
$body = Join-Path $tmp "body.png"
& $magick -size 256x256 xc: -sparse-color Barycentric "0,0 #60CDFF 255,255 #005A9E" $grad
& $magick -size 256x256 xc:none -fill white -draw "roundrectangle 0,0 255,255 52,52" $mask
& $magick $grad $mask -compose CopyOpacity -composite $body
# Larger magnifier, almost edge-to-edge. No stacked-file deco (vanishes at 16px anyway).
& $magick $body -stroke white -strokewidth 22 -fill none -draw "circle 118,112 118,170" -draw "stroke-linecap round line 156,152 214,214" PNG32:$master
Copy-Item $master $png256 -Force

$sizes = 16,20,24,32,40,48,64,128,256
$pngs = @()
foreach ($s in $sizes) {
  $out = Join-Path $tmp "$s.png"
  if ($s -le 24) {
    # Pixel-tight small glyphs: 1px inset max, thick stroke.
    $sw = [Math]::Max(1, [int]($s / 8))
    $cx = [int]($s * 0.46)
    $cy = [int]($s * 0.44)
    $r  = [int]($s * 0.28)
    $x2 = [int]($s * 0.82)
    $y2 = [int]($s * 0.82)
    $hx = [int]($cx + $r * 0.72)
    $hy = [int]($cy + $r * 0.72)
    $rr = [int]($s * 0.22)
    & $magick -size "${s}x${s}" xc:none -fill "#0078D4" -draw "roundrectangle 0,0 $($s-1),$($s-1) $rr,$rr" -stroke white -strokewidth $sw -fill none -draw "circle $cx,$cy $cx,$($cy+$r)" -draw "stroke-linecap round line $hx,$hy $x2,$y2" PNG32:$out
  } else {
    & $magick $master -filter Lanczos -resize "${s}x${s}" PNG32:$out
  }
  $pngs += $out
}
& $magick @pngs $ico
if (-not (Test-Path $ico)) { throw "ico missing" }
Write-Output ("ICO=" + (Get-Item $ico).Length)
Remove-Item -Recurse -Force $tmp

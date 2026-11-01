#!/bin/sh
# Record a real UEFI boot and render it as the README's animated boot capture,
# themed like the other images: the brand palette, a window title bar and
# rounded corners. Needs qemu, asciinema's agg and ffmpeg.
#
# Usage: scripts/brand/boot_gif.sh [out.gif]   (after `cargo xtask iso`)
set -eu
out=${1:-docs/assets/boot.gif}
tmp=build/boot-gif
mkdir -p "$tmp"

# The brand palette as an agg theme: bg, fg, then ANSI 0 to 15.
theme() {
    echo "$1,c9d1dc,0f1218,e5737a,7fb88a,e0b062,7fa6e8,c49be8,7fc4c8,c9d1dc,6b7484,ef8a90,93cc9e,ecc27a,9bbcf0,d4b2f0,96d4d8,f0f6fc"
}
render() { # cast, gif, background
    agg --theme "$(theme "$3")" --font-dir /System/Library/Fonts --font-dir /usr/share/fonts \
        --font-family "SF Mono,Menlo,DejaVu Sans Mono" --font-size 14 --line-height 1.36 \
        --idle-time-limit 3 --last-frame-duration 4 "$1" "$2" >/dev/null 2>&1
}

[ -n "${REUSE_CAST:-}" ] || python3 scripts/brand/record_boot.py "$tmp/boot.cast"
render "$tmp/boot.cast" "$tmp/body.gif" 0f1218

# The title bar: three window dots and the title, centred, on the bar colour.
cols=$(head -1 "$tmp/boot.cast" | sed 's/.*"width": \([0-9]*\).*/\1/')
python3 - "$cols" "$tmp/title.cast" <<'PY'
import json, sys
cols, out = int(sys.argv[1]), sys.argv[2]
title = "qemu-system-x86_64 · q35 · UEFI (OVMF) · serial"
dots = "\x1b[31m●\x1b[0m \x1b[33m●\x1b[0m \x1b[32m●\x1b[0m"
pad = (cols - len(title)) // 2 - 5
line = "\x1b[?25l" + dots + " " * pad + "\x1b[90m" + title + "\x1b[0m"
with open(out, "w") as f:
    f.write(json.dumps({"version": 2, "width": cols, "height": 1}) + "\n")
    f.write(json.dumps([0.0, "o", line]) + "\n")
PY
render "$tmp/title.cast" "$tmp/title.gif" 1a1f28

# Stack the bar on the boot, round the corners (radius 10), and quantize with
# one shared palette so the GIF stays small.
ffmpeg -v error -y -i "$tmp/body.gif" -i "$tmp/title.gif" -filter_complex "
[1:v]loop=-1:1:0,format=rgba[bar];
[0:v]format=rgba[b];
[bar][b]vstack=shortest=1,format=rgba,
geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(hypot(max(10-X,max(X-(W-11),0)),max(10-Y,max(Y-(H-11),0))),10.5)+gt(min(X,W-1-X),10)+gt(min(Y,H-1-Y),10),255,0)'
,split[s0][s1];[s0]palettegen=reserve_transparent=1:stats_mode=diff[p];
[s1][p]paletteuse=dither=none:diff_mode=rectangle:alpha_threshold=128" "$out"
ls -l "$out"

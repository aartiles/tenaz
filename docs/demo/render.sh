#!/bin/sh
# Render docs/demo/demo.gif from demo.tape. Needs cargo, vhs and ffmpeg.
# Run it from anywhere; it works from the repository root.
set -eu
cd "$(dirname "$0")/../.."
raw="$(mktemp -d)/demo.gif"
cargo build --release
vhs docs/demo/demo.tape -o "$raw"
# VHS writes a GIF of about 9 MB. Sixteen colours keep the text sharp and
# bring it under 5 MB.
ffmpeg -v error -y -i "$raw" -filter_complex \
  "split[a][b];[a]palettegen=max_colors=16[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
  docs/demo/demo.gif

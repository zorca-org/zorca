#!/usr/bin/env bash
# Regenerates the application icons from the reusable ZOrca logo master.
# Run from the repository root: ./docs/branding/generate-icons.sh
set -euo pipefail

src=docs/branding/logos/zorca-logo-master.png
out=crates/zed/resources
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The source is opaque RGB, so the area outside the rounded square is black.
# Flood-fill each corner to transparent, then trim the antialiased remainder.
magick "$src" -alpha set -fuzz 12% -fill none \
    -draw 'alpha 0,0 floodfill' \
    -draw 'alpha %[fx:w-1],0 floodfill' \
    -draw 'alpha 0,%[fx:h-1] floodfill' \
    -draw 'alpha %[fx:w-1],%[fx:h-1] floodfill' \
    -trim +repage "$work/mark.png"

# macOS expects a rounded-square mark to occupy 824 of a 1024 canvas; the same
# ratio keeps the icon from looking oversized next to other apps on Linux too.
for pair in "stable:app-icon:100,100,100" \
            "preview:app-icon-preview:100,100,122" \
            "nightly:app-icon-nightly:100,100,155" \
            "dev:app-icon-dev:100,35,100"; do
    base="${pair#*:}"; modulate="${base#*:}"; base="${base%%:*}"
    for size in 512 1024; do
        inner=$((size * 824 / 1024))
        suffix=""; [ "$size" = 1024 ] && suffix="@2x"
        magick "$work/mark.png" -modulate "$modulate" \
            -resize "${inner}x${inner}" \
            -background none -gravity center -extent "${size}x${size}" \
            "$out/${base}${suffix}.png"
    done
    magick "$out/${base}@2x.png" \
        -define icon:auto-resize=256,128,64,48,32,16 "$out/windows/${base}.ico"
done

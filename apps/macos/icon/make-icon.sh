#!/bin/bash
# Renders OpenVirtualSoundcard.svg into the app's icons: OpenVirtualSoundcard.icns for the bundle
# (Finder, Launchpad) and OpenVirtualSoundcard-512.png, which the app gives the window
# and the Dock while it runs. Run it after changing the SVG and commit the
# results; building the app needs neither this script nor its tools.
#
# Needs resvg (cargo install resvg) or rsvg-convert, and python3.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE"

render() {
    if command -v resvg >/dev/null; then
        resvg -w "$1" -h "$1" OpenVirtualSoundcard.svg "$2"
    elif command -v rsvg-convert >/dev/null; then
        rsvg-convert -w "$1" -h "$1" -o "$2" OpenVirtualSoundcard.svg
    else
        echo "make-icon.sh: install resvg (cargo install resvg) or rsvg-convert" >&2
        exit 1
    fi
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
for size in 16 32 64 128 256 512 1024; do
    render "$size" "$WORK/$size.png"
done
cp "$WORK/512.png" OpenVirtualSoundcard-512.png

# An .icns file is a list of (type, length, PNG) entries; macOS 10.7 and
# later take PNG data for all of these types.
python3 - "$WORK" OpenVirtualSoundcard.icns <<'PY'
import struct, sys
work, out = sys.argv[1], sys.argv[2]
entries = [
    (b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128),
    (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024),
    (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512),
]
body = b""
for kind, size in entries:
    png = open(f"{work}/{size}.png", "rb").read()
    body += kind + struct.pack(">I", 8 + len(png)) + png
open(out, "wb").write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
PY
echo "wrote OpenVirtualSoundcard.icns and OpenVirtualSoundcard-512.png"

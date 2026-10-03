#!/bin/bash
# Builds build/OvscProbe.driver and build/ovprobe-daemon.
#
# On a Mac this needs only Xcode's command-line tools and Rust. To cross-build
# elsewhere, set TARGET (e.g. aarch64-apple-darwin), CC (a clang wrapper with
# -target/-isysroot) and the usual cargo linker variables.
set -euo pipefail
cd "$(dirname "$0")"

TARGET=${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
CC=${CC:-clang}
OUT=build
DRIVER=$OUT/OvscProbe.driver

$CC -fsyntax-only bundle/abi_check.c
cargo build --release --target "$TARGET"
REL=target/$TARGET/release

rm -rf "$DRIVER"
mkdir -p "$DRIVER/Contents/MacOS"
cp bundle/Info.plist "$DRIVER/Contents/Info.plist"
$CC -bundle -o "$DRIVER/Contents/MacOS/OvscProbe" \
    "$REL/libodprobe_plugin.a" \
    -framework CoreFoundation -liconv -lSystem -lc -lm \
    -Wl,-exported_symbol,_OvscProbe_Create -Wl,-dead_strip
cp "$REL/ovprobe-daemon" "$OUT/ovprobe-daemon"
cp bundle/org.openvirtualsoundcard.probe.plist "$OUT/"

if command -v codesign >/dev/null; then
    codesign --force --sign - --timestamp=none "$DRIVER"
    codesign --force --sign - --timestamp=none "$OUT/ovprobe-daemon"
fi
echo "built $DRIVER and $OUT/ovprobe-daemon"

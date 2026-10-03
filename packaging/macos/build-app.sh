#!/bin/bash
# Builds OpenVirtualSoundcard.app, the app that shows the daemon's status and changes
# its settings: the release build of apps/macos in an app bundle, with
# Info.plist from apps/macos/Info.plist.in. The bundle is signed ad hoc, or
# with CODESIGN_IDENTITY (hardened runtime and a secure timestamp) when that
# is set. The last line printed is the bundle's path.
#
# Needs a Mac with Rust and the Xcode command-line tools.
#
# UNIVERSAL=1 builds for both aarch64-apple-darwin and x86_64-apple-darwin
# and joins the two with lipo, for releases. Both Rust targets must be
# installed (rustup target add).
#
# Environment:
#   TARGET             Rust target (default: the host; ignored with UNIVERSAL=1)
#   UNIVERSAL          1: arm64 and x86_64 in one universal binary
#   OUT                output directory (default: target/macos)
#   CODESIGN_IDENTITY  signing identity (default: ad hoc)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
SRC=$ROOT/apps/macos

fail() {
    echo "build-app.sh: $*" >&2
    exit 1
}

[ "$(uname -s)" = Darwin ] || fail "builds on macOS only"
for tool in cargo codesign lipo plutil; do
    command -v "$tool" >/dev/null || fail "$tool not found (Rust, or the Xcode command-line tools)"
done
if [ "${UNIVERSAL:-0}" = 1 ]; then
    TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
else
    TARGETS=("${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}")
fi
for target in "${TARGETS[@]}"; do
    case "$target" in
        aarch64-apple-darwin | x86_64-apple-darwin) ;;
        *)
            echo "build-app.sh: TARGET must be an Apple target, not '$target'" >&2
            exit 2
            ;;
    esac
done
OUT=${OUT:-$ROOT/target/macos}
APP=$OUT/OpenVirtualSoundcard.app
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$SRC/Cargo.toml" | head -n 1)
[ -n "$VERSION" ] || fail "no version in apps/macos/Cargo.toml"
# Where cargo puts apps/macos's build, relative paths from here.
BUILD_DIR=${CARGO_TARGET_DIR:-$SRC/target}
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}

BINARIES=()
for target in "${TARGETS[@]}"; do
    # A custom toolchain has no target list; cargo then has the last word.
    if command -v rustup >/dev/null && installed=$(rustup target list --installed 2>/dev/null); then
        grep -qx "$target" <<<"$installed" ||
            fail "the Rust target $target is missing: rustup target add $target"
    fi
    cargo build --release --locked --manifest-path "$SRC/Cargo.toml" --target "$target"
    BINARIES+=("$BUILD_DIR/$target/release/OpenVirtualSoundcard")
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
BIN=$APP/Contents/MacOS/OpenVirtualSoundcard
if [ "${#BINARIES[@]}" -gt 1 ]; then
    lipo -create -output "$BIN" "${BINARIES[@]}"
    ARCHS=" $(lipo -archs "$BIN") "
    for arch in arm64 x86_64; do
        case "$ARCHS" in
            *" $arch "*) ;;
            *) fail "$BIN lacks $arch (it has:$ARCHS)" ;;
        esac
    done
else
    cp "${BINARIES[0]}" "$BIN"
fi
sed "s/@VERSION@/$VERSION/g" "$SRC/Info.plist.in" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
# The icon Finder and Launchpad show (made by apps/macos/icon/make-icon.sh).
cp "$SRC/icon/OpenVirtualSoundcard.icns" "$APP/Contents/Resources/OpenVirtualSoundcard.icns"

# Sign last: signing seals the finished bundle.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    codesign --force --sign "$CODESIGN_IDENTITY" --options runtime --timestamp "$APP"
else
    codesign --force --sign - --timestamp=none "$APP"
fi
codesign --verify --strict "$APP"
echo "built OpenVirtualSoundcard.app (${TARGETS[*]}, $VERSION):"
echo "$APP"

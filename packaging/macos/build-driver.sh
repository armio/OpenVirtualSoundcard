#!/bin/bash
# Builds OpenVirtualSoundcard.driver, the Core Audio server plug-in bundle: the
# ovsc-hal static library linked into an MH_BUNDLE that exports only
# OpenVirtualSoundcard_Factory, with Info.plist from Info.plist.in.
#
# Native mode, on a Mac: needs Rust and the Xcode command-line tools. The
# bundle is signed ad hoc, or with CODESIGN_IDENTITY (hardened runtime and a
# secure timestamp) when that is set.
#
# Cross mode, anywhere else: set TARGET (aarch64-apple-darwin or
# x86_64-apple-darwin), a clang wrapper for that target with the macOS SDK
# (CC, or cargo's CC_<target>), and cargo's linker variables. Signing and
# plutil are skipped, and llvm-nm/llvm-objdump do the structural checks.
#
# UNIVERSAL=1 builds the library for both aarch64-apple-darwin and
# x86_64-apple-darwin, links a bundle binary for each and joins them with
# lipo (llvm-lipo in cross mode), for releases. Both Rust targets must be
# installed (rustup target add); in cross mode cargo needs each target's
# CC_<target> and AR_<target>.
#
# Environment:
#   TARGET             Rust target (default: the host; ignored with UNIVERSAL=1)
#   UNIVERSAL          1: arm64 and x86_64 in one universal binary
#   CC                 clang for the target (default: $CC_<target>, else clang)
#   OUT                output directory (default: target/macos)
#   CODESIGN_IDENTITY  signing identity (default: ad hoc)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

if [ "${UNIVERSAL:-0}" = 1 ]; then
    TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
else
    TARGETS=("${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}")
fi
for target in "${TARGETS[@]}"; do
    case "$target" in
        aarch64-apple-darwin | x86_64-apple-darwin) ;;
        *)
            echo "build-driver.sh: TARGET must be an Apple target, not '$target'" >&2
            exit 2
            ;;
    esac
done
OUT=${OUT:-$ROOT/target/macos}
DRIVER=$OUT/OpenVirtualSoundcard.driver
BIN=$DRIVER/Contents/MacOS/OpenVirtualSoundcard
SLICES=$OUT/slices
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
if [ -z "$VERSION" ]; then
    echo "build-driver.sh: no version in Cargo.toml" >&2
    exit 1
fi
if [ "$(uname -s)" = Darwin ]; then NATIVE=1; else NATIVE=0; fi
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}

# The first of the given tools that exists.
pick() {
    for tool in "$@"; do
        if command -v "$tool" >/dev/null; then
            echo "$tool"
            return
        fi
    done
    echo "build-driver.sh: none of $* found" >&2
    exit 1
}

fail() {
    echo "build-driver.sh: $*" >&2
    exit 1
}

if [ "$NATIVE" = 0 ]; then
    NM=$(pick llvm-nm-18 llvm-nm)
    OBJDUMP=$(pick llvm-objdump-18 llvm-objdump)
fi

# Structural checks on one architecture's binary: a bundle, one exported
# symbol, only system libraries.
check_binary() {
    local bin=$1 exports libs lib
    if [ "$NATIVE" = 1 ]; then
        file "$bin" | grep -q "bundle" || fail "$bin is not a Mach-O bundle"
        exports=$(nm -gU "$bin" | awk '{print $NF}')
        libs=$(otool -L "$bin" | tail -n +2 | awk '{print $1}')
    else
        "$OBJDUMP" --macho --private-header "$bin" | grep -q " BUNDLE " ||
            fail "$bin is not a Mach-O bundle"
        exports=$("$NM" -gU "$bin" | awk '{print $NF}')
        libs=$("$OBJDUMP" --macho --dylibs-used "$bin" | tail -n +2 | awk '{print $1}')
    fi
    [ "$exports" = "_OpenVirtualSoundcard_Factory" ] ||
        fail "the bundle must export only _OpenVirtualSoundcard_Factory, not: ${exports//$'\n'/ }"
    for lib in $libs; do
        case "$lib" in
            /System/Library/Frameworks/CoreFoundation.framework/*) ;;
            /usr/lib/libSystem.B.dylib | /usr/lib/libiconv.2.dylib) ;;
            *) fail "the bundle links $lib" ;;
        esac
    done
}

# One bundle binary per architecture, in $SLICES.
rm -rf "$SLICES"
mkdir -p "$SLICES"
BINARIES=()
for target in "${TARGETS[@]}"; do
    case "$target" in
        aarch64-apple-darwin) arch=arm64 ;;
        x86_64-apple-darwin) arch=x86_64 ;;
    esac
    # A custom toolchain has no target list; cargo then has the last word.
    if command -v rustup >/dev/null && installed=$(rustup target list --installed 2>/dev/null); then
        grep -qx "$target" <<<"$installed" ||
            fail "the Rust target $target is missing: rustup target add $target"
    fi
    cc_var="CC_${target//-/_}"
    cc=${CC:-${!cc_var:-clang}}

    # The Rust ABI mirror must match the SDK this bundle is built against.
    "$cc" -arch "$arch" -fsyntax-only crates/ovsc-hal/abi_check.c

    cargo build --release --locked -p ovsc-hal --lib --target "$target"
    "$cc" -arch "$arch" -bundle -mmacosx-version-min=11.0 -o "$SLICES/OpenVirtualSoundcard-$arch" \
        "target/$target/release/libovsc_hal.a" \
        -framework CoreFoundation -liconv -lSystem -lc -lm \
        -Wl,-exported_symbol,_OpenVirtualSoundcard_Factory -Wl,-dead_strip
    check_binary "$SLICES/OpenVirtualSoundcard-$arch"
    BINARIES+=("$SLICES/OpenVirtualSoundcard-$arch")
done

rm -rf "$DRIVER"
mkdir -p "$DRIVER/Contents/MacOS"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist.in > "$DRIVER/Contents/Info.plist"
if [ "${#BINARIES[@]}" -gt 1 ]; then
    if [ "$NATIVE" = 1 ]; then LIPO=lipo; else LIPO=$(pick llvm-lipo-18 llvm-lipo); fi
    "$LIPO" -create -output "$BIN" "${BINARIES[@]}"
    ARCHS=" $("$LIPO" -archs "$BIN") "
    for arch in arm64 x86_64; do
        case "$ARCHS" in
            *" $arch "*) ;;
            *) fail "$BIN lacks $arch (it has:$ARCHS)" ;;
        esac
    done
else
    cp "${BINARIES[0]}" "$BIN"
fi
if [ "$NATIVE" = 1 ]; then
    plutil -lint "$DRIVER/Contents/Info.plist" >/dev/null
elif command -v python3 >/dev/null; then
    python3 -c 'import plistlib, sys; plistlib.load(open(sys.argv[1], "rb"))' \
        "$DRIVER/Contents/Info.plist"
fi

# Sign last: signing seals the finished bundle.
if [ "$NATIVE" = 1 ]; then
    if [ -n "${CODESIGN_IDENTITY:-}" ]; then
        codesign --force --sign "$CODESIGN_IDENTITY" --options runtime --timestamp "$DRIVER"
    else
        codesign --force --sign - --timestamp=none "$DRIVER"
    fi
    codesign --verify --strict "$DRIVER"
else
    echo "build-driver.sh: cross build, not signed"
fi
echo "built $DRIVER (${TARGETS[*]}, $VERSION)"

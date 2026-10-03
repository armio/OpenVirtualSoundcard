#!/bin/bash
# Builds the OpenVirtualSoundcard installer package, dist/OpenVirtualSoundcard-<version>.pkg (or
# OpenVirtualSoundcard-<version>-<arch>.pkg for a single architecture). It installs
# the driver, the daemon with its launchd job, the log rotation rule and
# the uninstaller, then runs install-lib.sh's steps from its postinstall
# script, as install.sh does.
#
# Needs macOS (pkgbuild, productbuild, lipo, codesign) and the built driver
# and daemon, from the repository root:
#   cargo build --release --locked -p ovsc
#   packaging/macos/build-driver.sh
# or BUILD=1, which builds both here.
#
# Environment:
#   DRIVER              the driver bundle (default: target/macos/OpenVirtualSoundcard.driver)
#   DAEMON              the ovsc binary (default: target/release/ovsc)
#   BUILD               1: build the driver and the daemon first, and package
#                       those (DRIVER and DAEMON must then be unset)
#   UNIVERSAL           1 with BUILD=1: build both for arm64 and x86_64 and
#                       join each pair with lipo (releases)
#   TARGET              with BUILD=1: build both for this Rust target instead
#                       of the host (ignored with UNIVERSAL=1)
#   OUT                 the package's directory (default: dist)
#   CODESIGN_IDENTITY   a Developer ID Application identity: signs the driver
#                       and the daemon with the hardened runtime and a secure
#                       timestamp (default: ad hoc)
#   INSTALLER_IDENTITY  a Developer ID Installer identity: signs the package
#                       with productsign (default: unsigned)
#   NOTARY_PROFILE      a notarytool keychain profile: notarizes the package
#                       and staples the ticket; needs both identities
#
# Do not hand out an unsigned package for download: Gatekeeper blocks it
# and macOS 15 dropped the Control-click way around that.
set -euo pipefail
# The payload's directories get the modes they are staged with.
umask 022
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
cd "$ROOT"

fail() {
    echo "build-pkg.sh: $*" >&2
    exit 1
}

[ "$(uname -s)" = Darwin ] || fail "pkgbuild and productbuild need macOS"
for tool in pkgbuild productbuild pkgutil lipo codesign ditto; do
    command -v "$tool" >/dev/null || fail "$tool not found (Xcode command-line tools)"
done
if [ -n "${NOTARY_PROFILE:-}" ] && { [ -z "${CODESIGN_IDENTITY:-}" ] || [ -z "${INSTALLER_IDENTITY:-}" ]; }; then
    fail "NOTARY_PROFILE needs CODESIGN_IDENTITY and INSTALLER_IDENTITY"
fi

VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || fail "no version in Cargo.toml"
OUT=${OUT:-$ROOT/dist}
STAGE=$ROOT/target/pkg
PKG_ID=org.openvirtualsoundcard.pkg
COMPONENT=OpenVirtualSoundcard-component.pkg

if [ "${BUILD:-0}" = 1 ]; then
    # Only what is built here goes into the package.
    if [ -n "${DRIVER:-}" ] || [ -n "${DAEMON:-}" ]; then
        fail "BUILD=1 packages what it builds: unset DRIVER and DAEMON"
    fi
    # build-driver.sh takes its output directory from OUT as well: pass
    # its default explicitly, not this script's OUT.
    OUT="$ROOT/target/macos" packaging/macos/build-driver.sh
    DRIVER=$ROOT/target/macos/OpenVirtualSoundcard.driver
    if [ "${UNIVERSAL:-0}" = 1 ]; then
        slices=()
        for target in aarch64-apple-darwin x86_64-apple-darwin; do
            cargo build --release --locked -p ovsc --target "$target"
            slices+=("target/$target/release/ovsc")
        done
        mkdir -p target/macos
        lipo -create -output target/macos/ovsc "${slices[@]}"
        DAEMON=$ROOT/target/macos/ovsc
    elif [ -n "${TARGET:-}" ]; then
        cargo build --release --locked -p ovsc --target "$TARGET"
        DAEMON=$ROOT/target/$TARGET/release/ovsc
    else
        cargo build --release --locked -p ovsc
        DAEMON=$ROOT/target/release/ovsc
    fi
fi
DRIVER=${DRIVER:-$ROOT/target/macos/OpenVirtualSoundcard.driver}
DAEMON=${DAEMON:-$ROOT/target/release/ovsc}
[ -f "$DRIVER/Contents/MacOS/OpenVirtualSoundcard" ] ||
    fail "no driver bundle at $DRIVER (packaging/macos/build-driver.sh, or BUILD=1)"
[ -x "$DAEMON" ] || fail "no daemon binary at $DAEMON (cargo build --release -p ovsc, or BUILD=1)"

# The package runs where both the driver and the daemon do; arm64 first.
DRIVER_ARCHS=" $(lipo -archs "$DRIVER/Contents/MacOS/OpenVirtualSoundcard" | xargs) "
DAEMON_ARCHS=" $(lipo -archs "$DAEMON" | xargs) "
ARCHS=
for arch in arm64 x86_64; do
    case "$DRIVER_ARCHS" in *" $arch "*) ;; *) continue ;; esac
    case "$DAEMON_ARCHS" in *" $arch "*) ;; *) continue ;; esac
    ARCHS=${ARCHS:+$ARCHS,}$arch
done
[ -n "$ARCHS" ] ||
    fail "the driver (${DRIVER_ARCHS:1:${#DRIVER_ARCHS}-2}) and the daemon (${DAEMON_ARCHS:1:${#DAEMON_ARCHS}-2}) share no architecture"
if [ "$ARCHS" = arm64,x86_64 ]; then
    PKG=$OUT/OpenVirtualSoundcard-$VERSION.pkg
else
    PKG=$OUT/OpenVirtualSoundcard-$VERSION-$ARCHS.pkg
fi

# The payload, laid out as installed. /etc is a link to /private/etc,
# which a payload must not replace.
SUPPORT="Library/Application Support/OpenVirtualSoundcard"
STAGED_DRIVER=$STAGE/root/Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver
rm -rf "$STAGE"
mkdir -p "${STAGED_DRIVER%/*}" "$STAGE/root/$SUPPORT/bin" \
    "$STAGE/root/Library/LaunchDaemons" "$STAGE/root/private/etc/newsyslog.d" "$STAGE/scripts"
ditto "$DRIVER" "$STAGED_DRIVER"
install -m 755 "$DAEMON" "$STAGE/root/$SUPPORT/bin/ovsc"
install -m 755 "$HERE/uninstall.sh" "$STAGE/root/$SUPPORT/uninstall.sh"
install -m 644 "$HERE/org.openvirtualsoundcard.daemon.plist" "$STAGE/root/Library/LaunchDaemons/"
install -m 644 "$HERE/org.openvirtualsoundcard.newsyslog.conf" \
    "$STAGE/root/private/etc/newsyslog.d/org.openvirtualsoundcard.conf"
install -m 755 "$HERE/scripts/preinstall" "$HERE/scripts/postinstall" "$STAGE/scripts/"
install -m 644 "$HERE/install-lib.sh" "$HERE/ovsc.toml.default" "$STAGE/scripts/"
# The modes as installed, whatever the umask of the builds: Installer gives
# the directories it lays down, /Library/Audio/Plug-Ins/HAL among them, the
# payload's modes.
find "$STAGE/root" "$STAGE/scripts" -type d -exec chmod 755 {} +
find "$STAGED_DRIVER" -type f -exec chmod 644 {} +
chmod 755 "$STAGED_DRIVER/Contents/MacOS/OpenVirtualSoundcard"
# Quarantine and Finder attributes have no place in a payload. Code
# signatures live in the files themselves, not in attributes.
xattr -cr "$STAGE"

# Sign the staged copies, whatever signed the build: last, and both alike.
for code in "$STAGED_DRIVER" "$STAGE/root/$SUPPORT/bin/ovsc"; do
    if [ -n "${CODESIGN_IDENTITY:-}" ]; then
        codesign --force --sign "$CODESIGN_IDENTITY" --options runtime --timestamp "$code"
    else
        codesign --force --sign - --timestamp=none "$code"
    fi
    codesign --verify --strict "$code"
done

mkdir -p "$OUT"
pkgbuild --root "$STAGE/root" --component-plist "$HERE/component.plist" \
    --scripts "$STAGE/scripts" --identifier "$PKG_ID" --version "$VERSION" \
    --install-location / --ownership recommended "$STAGE/$COMPONENT"

# The payload must hold every installed path; uninstall.sh removes them all.
PAYLOAD=$(pkgutil --payload-files "$STAGE/$COMPONENT" | sed 's|^\./||')
for path in Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver/Contents/MacOS/OpenVirtualSoundcard \
    Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver/Contents/Info.plist \
    "$SUPPORT/bin/ovsc" "$SUPPORT/uninstall.sh" \
    Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist \
    private/etc/newsyslog.d/org.openvirtualsoundcard.conf; do
    grep -qxF "$path" <<<"$PAYLOAD" || fail "the payload lacks /$path"
done

sed -e "s/@VERSION@/$VERSION/g" \
    -e "s/hostArchitectures=\"arm64,x86_64\"/hostArchitectures=\"$ARCHS\"/" \
    "$HERE/distribution.xml" >"$STAGE/distribution.xml"
grep -q "hostArchitectures=\"$ARCHS\"" "$STAGE/distribution.xml" ||
    fail "distribution.xml has no hostArchitectures to set"
rm -f "$PKG"
if [ -n "${INSTALLER_IDENTITY:-}" ]; then
    productbuild --distribution "$STAGE/distribution.xml" --package-path "$STAGE" \
        "$STAGE/OpenVirtualSoundcard-unsigned.pkg"
    productsign --sign "$INSTALLER_IDENTITY" --timestamp "$STAGE/OpenVirtualSoundcard-unsigned.pkg" "$PKG"
    pkgutil --check-signature "$PKG"
else
    productbuild --distribution "$STAGE/distribution.xml" --package-path "$STAGE" "$PKG"
fi

if [ -n "${NOTARY_PROFILE:-}" ]; then
    # notarytool's exit status does not say whether Apple accepted it.
    RESULT=$(xcrun notarytool submit "$PKG" --keychain-profile "$NOTARY_PROFILE" --wait 2>&1) || true
    echo "$RESULT"
    grep -q "status: Accepted" <<<"$RESULT" || fail "notarization failed (xcrun notarytool log <id>)"
    xcrun stapler staple "$PKG"
    xcrun stapler validate "$PKG"
fi
echo "built $PKG ($VERSION, $ARCHS)"

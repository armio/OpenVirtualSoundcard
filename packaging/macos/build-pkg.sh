#!/bin/bash
# Builds the OpenVirtualSoundcard installer package,
# dist/OpenVirtualSoundcard-<version>.pkg (or
# OpenVirtualSoundcard-<version>-<arch>.pkg for a single architecture). It
# installs the driver, the daemon with its launchd job, the log rotation
# rule, the uninstaller and, when built, the app in /Applications and the
# third-party licence notices, then runs install-lib.sh's steps from its
# postinstall script, as install.sh does.
#
# Needs macOS (pkgbuild, productbuild, lipo, codesign) and the built driver
# and daemon, from the repository root:
#   cargo build --release --locked -p ovsc
#   packaging/macos/build-driver.sh
#   packaging/macos/build-app.sh                  (optional: the app)
#   packaging/macos/third-party-licenses.sh       (optional: the notices)
# or BUILD=1, which builds all four here (releases).
#
# Environment:
#   DRIVER              the driver bundle (default: target/macos/OpenVirtualSoundcard.driver)
#   DAEMON              the ovsc binary (default: target/release/ovsc)
#   APP                 the app bundle (default: target/macos/OpenVirtualSoundcard.app,
#                       left out if it is not there)
#   NOTICES             the licence notices (default:
#                       target/macos/THIRD-PARTY-LICENSES.html, left out if
#                       it is not there)
#   BUILD               1: build the driver, the daemon, the app and the
#                       notices first, and package those (DRIVER, DAEMON, APP
#                       and NOTICES must then be unset)
#   UNIVERSAL           1 with BUILD=1: build for arm64 and x86_64 and join
#                       each pair with lipo (releases)
#   TARGET              with BUILD=1: build for this Rust target instead of
#                       the host (ignored with UNIVERSAL=1)
#   OUT                 the package's directory (default: dist)
#   CODESIGN_IDENTITY   a Developer ID Application identity: signs the driver,
#                       the daemon and the app with the hardened runtime and
#                       a secure timestamp (default: ad hoc)
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
    if [ -n "${DRIVER:-}" ] || [ -n "${DAEMON:-}" ] || [ -n "${APP:-}" ] || [ -n "${NOTICES:-}" ]; then
        fail "BUILD=1 packages what it builds: unset DRIVER, DAEMON, APP and NOTICES"
    fi
    # build-driver.sh and build-app.sh take their output directory from OUT
    # as well: pass their default explicitly, not this script's OUT.
    OUT="$ROOT/target/macos" packaging/macos/build-driver.sh
    DRIVER=$ROOT/target/macos/OpenVirtualSoundcard.driver
    OUT="$ROOT/target/macos" packaging/macos/build-app.sh
    APP=$ROOT/target/macos/OpenVirtualSoundcard.app
    packaging/macos/third-party-licenses.sh
    NOTICES=$ROOT/target/macos/THIRD-PARTY-LICENSES.html
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
APP=${APP:-$ROOT/target/macos/OpenVirtualSoundcard.app}
if [ -f "$APP/Contents/MacOS/OpenVirtualSoundcard" ]; then
    echo "build-pkg.sh: packaging the app $APP"
else
    echo "build-pkg.sh: no app at $APP; packaging without it (packaging/macos/build-app.sh)"
    APP=
fi
NOTICES=${NOTICES:-$ROOT/target/macos/THIRD-PARTY-LICENSES.html}
if [ -f "$NOTICES" ]; then
    echo "build-pkg.sh: packaging the licence notices $NOTICES"
else
    echo "build-pkg.sh: no licence notices at $NOTICES; packaging without them (packaging/macos/third-party-licenses.sh)"
    NOTICES=
fi

# The package runs where the driver, the daemon and the app all do; arm64
# first.
DRIVER_ARCHS=" $(lipo -archs "$DRIVER/Contents/MacOS/OpenVirtualSoundcard" | xargs) "
DAEMON_ARCHS=" $(lipo -archs "$DAEMON" | xargs) "
APP_ARCHS=" arm64 x86_64 "
if [ -n "$APP" ]; then
    APP_ARCHS=" $(lipo -archs "$APP/Contents/MacOS/OpenVirtualSoundcard" | xargs) "
fi
ARCHS=
for arch in arm64 x86_64; do
    case "$DRIVER_ARCHS" in *" $arch "*) ;; *) continue ;; esac
    case "$DAEMON_ARCHS" in *" $arch "*) ;; *) continue ;; esac
    case "$APP_ARCHS" in *" $arch "*) ;; *) continue ;; esac
    ARCHS=${ARCHS:+$ARCHS,}$arch
done
[ -n "$ARCHS" ] ||
    fail "the driver (${DRIVER_ARCHS:1:${#DRIVER_ARCHS}-2}), the daemon (${DAEMON_ARCHS:1:${#DAEMON_ARCHS}-2}) and the app (${APP_ARCHS:1:${#APP_ARCHS}-2}) share no architecture"
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
STAGED_APP=$STAGE/root/Applications/OpenVirtualSoundcard.app
if [ -n "$APP" ]; then
    mkdir -p "${STAGED_APP%/*}"
    ditto "$APP" "$STAGED_APP"
fi
if [ -n "$NOTICES" ]; then
    install -m 644 "$NOTICES" "$STAGE/root/$SUPPORT/THIRD-PARTY-LICENSES.html"
    # The app may be copied elsewhere: it carries its own copy.
    if [ -n "$APP" ]; then
        mkdir -p "$STAGED_APP/Contents/Resources"
        install -m 644 "$NOTICES" "$STAGED_APP/Contents/Resources/THIRD-PARTY-LICENSES.html"
    fi
fi
# The modes as installed, whatever the umask of the builds: Installer gives
# the directories it lays down, /Library/Audio/Plug-Ins/HAL among them, the
# payload's modes.
find "$STAGE/root" "$STAGE/scripts" -type d -exec chmod 755 {} +
find "$STAGED_DRIVER" -type f -exec chmod 644 {} +
chmod 755 "$STAGED_DRIVER/Contents/MacOS/OpenVirtualSoundcard"
if [ -n "$APP" ]; then
    find "$STAGED_APP" -type f -exec chmod 644 {} +
    chmod 755 "$STAGED_APP/Contents/MacOS/OpenVirtualSoundcard"
fi
# Quarantine and Finder attributes have no place in a payload. Code
# signatures live in the files themselves, not in attributes.
xattr -cr "$STAGE"

# Sign the staged copies, whatever signed the build: last, and all alike.
CODE=("$STAGED_DRIVER" "$STAGE/root/$SUPPORT/bin/ovsc")
[ -z "$APP" ] || CODE+=("$STAGED_APP")
for code in "${CODE[@]}"; do
    if [ -n "${CODESIGN_IDENTITY:-}" ]; then
        codesign --force --sign "$CODESIGN_IDENTITY" --options runtime --timestamp "$code"
    else
        codesign --force --sign - --timestamp=none "$code"
    fi
    codesign --verify --strict "$code"
done

# pkgbuild's options for the bundles in the payload. Not relocatable:
# otherwise Installer would put a new bundle wherever it finds one with the
# same identifier, such as a build directory. Always installed, whatever
# version is there: the driver, the daemon and the app come from one build.
bundle() {
    cat <<PLIST
	<dict>
		<key>BundleHasStrictIdentifier</key>
		<true/>
		<key>BundleIsRelocatable</key>
		<false/>
		<key>BundleIsVersionChecked</key>
		<false/>
		<key>BundleOverwriteAction</key>
		<string>upgrade</string>
		<key>RootRelativeBundlePath</key>
		<string>$1</string>
	</dict>
PLIST
}
{
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0">'
    echo '<array>'
    bundle Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver
    [ -z "$APP" ] || bundle Applications/OpenVirtualSoundcard.app
    echo '</array>'
    echo '</plist>'
} >"$STAGE/component.plist"
plutil -lint "$STAGE/component.plist" >/dev/null

mkdir -p "$OUT"
pkgbuild --root "$STAGE/root" --component-plist "$STAGE/component.plist" \
    --scripts "$STAGE/scripts" --identifier "$PKG_ID" --version "$VERSION" \
    --install-location / --ownership recommended "$STAGE/$COMPONENT"

# The payload must hold every installed path; uninstall.sh removes them all.
PAYLOAD=$(pkgutil --payload-files "$STAGE/$COMPONENT" | sed 's|^\./||')
for path in Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver/Contents/MacOS/OpenVirtualSoundcard \
    Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver/Contents/Info.plist \
    "$SUPPORT/bin/ovsc" "$SUPPORT/uninstall.sh" \
    Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist \
    private/etc/newsyslog.d/org.openvirtualsoundcard.conf \
    ${APP:+Applications/OpenVirtualSoundcard.app/Contents/MacOS/OpenVirtualSoundcard} \
    ${NOTICES:+"$SUPPORT/THIRD-PARTY-LICENSES.html"}; do
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

#!/bin/bash
# Installs OpenVirtualSoundcard on this Mac, or upgrades an install in place: the
# daemon as the LaunchDaemon org.openvirtualsoundcard.daemon, the Core Audio driver
# OpenVirtualSoundcard.driver and, when it is built, the OpenVirtualSoundcard app in
# /Applications. It then restarts Core Audio and waits until the driver
# has loaded and connected to the daemon. Running it again is safe, and is
# how an install is upgraded. The package (build-pkg.sh) runs the same
# steps, from the same install-lib.sh.
#
# Build first, from the repository root:
#   cargo build --release --locked -p ovsc
#   packaging/macos/build-driver.sh
#   packaging/macos/build-app.sh
#
# Usage: sudo packaging/macos/install.sh [options]
#   --config FILE           install FILE as the configuration. Without it an
#                           existing configuration is kept, or a default one
#                           written, with the device named after this Mac.
#   --driver PATH           the driver bundle to install
#                           (default: target/macos/OpenVirtualSoundcard.driver)
#   --daemon PATH           the ovsc binary to install
#                           (default: target/release/ovsc)
#   --app PATH              the app bundle to install in /Applications
#                           (default: target/macos/OpenVirtualSoundcard.app, if built)
#   --no-app                leave the installed app alone
#   --no-driver             leave the installed driver, and Core Audio, alone
#   --no-coreaudio-restart  install the driver without restarting Core Audio:
#                           it loads at the next restart of Core Audio
#
# Exits 1 if anything fails, including the driver not connecting to the
# daemon within 30 s, after printing diagnostics.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# shellcheck source=SCRIPTDIR/install-lib.sh
. "$HERE/install-lib.sh"

usage() {
    if [ "$1" = 0 ]; then
        sed -n '/^# Usage:/,/^#$/s/^# \{0,1\}//p' "$0"
    else
        sed -n '/^# Usage:/,/^#$/s/^# \{0,1\}//p' "$0" >&2
    fi
    exit "$1"
}

CONFIG=
DRIVER_SRC=$ROOT/target/macos/OpenVirtualSoundcard.driver
DAEMON_SRC=$ROOT/target/release/ovsc
APP_SRC=$ROOT/target/macos/OpenVirtualSoundcard.app
APP_GIVEN=0
WITH_APP=1
WITH_DRIVER=1
RESTART=1
while [ $# -gt 0 ]; do
    case "$1" in
    --config | --driver | --daemon | --app)
        [ $# -ge 2 ] || usage 2
        case "$1" in
        --config) CONFIG=$2 ;;
        --driver) DRIVER_SRC=$2 ;;
        --daemon) DAEMON_SRC=$2 ;;
        --app)
            APP_SRC=$2
            APP_GIVEN=1
            ;;
        esac
        shift 2
        ;;
    --no-app)
        WITH_APP=0
        shift
        ;;
    --no-driver)
        WITH_DRIVER=0
        shift
        ;;
    --no-coreaudio-restart)
        RESTART=0
        shift
        ;;
    -h | --help) usage 0 ;;
    *)
        ov_err "unknown argument '$1'"
        usage 2
        ;;
    esac
done

[ "$(uname -s)" = Darwin ] || ov_die "OpenVirtualSoundcard's driver is for macOS"
[ "$(id -u)" = 0 ] || ov_die "run as root: sudo $0"
if [ ! -f "$DAEMON_SRC" ] || [ ! -x "$DAEMON_SRC" ]; then
    ov_die "no daemon binary at $DAEMON_SRC (cargo build --release --locked -p ovsc, or --daemon)"
fi
DRIVER_ARG=
if [ "$WITH_DRIVER" = 1 ]; then
    if [ ! -f "$DRIVER_SRC/Contents/MacOS/OpenVirtualSoundcard" ] || [ ! -f "$DRIVER_SRC/Contents/Info.plist" ]; then
        ov_die "no driver bundle at $DRIVER_SRC (packaging/macos/build-driver.sh, or --driver)"
    fi
    DRIVER_ARG=$DRIVER_SRC
fi
if [ -n "$CONFIG" ]; then
    [ -f "$CONFIG" ] || ov_die "no configuration file $CONFIG"
fi
APP_ARG=
if [ "$WITH_APP" = 1 ]; then
    if [ -f "$APP_SRC/Contents/MacOS/OpenVirtualSoundcard" ] && [ -f "$APP_SRC/Contents/Info.plist" ]; then
        APP_ARG=$APP_SRC
    elif [ "$APP_GIVEN" = 1 ]; then
        ov_die "no app bundle at $APP_SRC (packaging/macos/build-app.sh)"
    else
        ov_log "no app at $APP_SRC, so not installing it (packaging/macos/build-app.sh builds it)"
    fi
fi

# OpenDante, the former name, is replaced, driver included.
if ov_legacy_installed && [ "$WITH_DRIVER" = 0 ]; then
    ov_die "OpenDante, this project's former name, is installed: run without --no-driver to replace it"
fi

# 1. Stop the old daemon, so that its files can be replaced, and replace an
# OpenDante install.
ov_bootout || ov_die "could not stop the running daemon"
ov_migrate_legacy || ov_die "could not replace the OpenDante install"
# 2-3. The driver, the binary, the launchd job, the log rotation rule and
# the configuration.
ov_install_files "$DRIVER_ARG" "$DAEMON_SRC" || ov_die "could not install the files"
if [ -n "$CONFIG" ]; then
    ov_log "installing $CONFIG as $OV_CONFIG"
    { install -m 644 "$CONFIG" "$OV_CONFIG" && chown root:wheel "$OV_CONFIG"; } ||
        ov_die "could not install $CONFIG"
else
    ov_default_config "$HERE/ovsc.toml.default" || ov_die "could not write $OV_CONFIG"
fi
if [ -n "$APP_ARG" ]; then
    ov_install_app "$APP_ARG" || ov_die "could not install the app"
fi
# 4. launchd refuses quarantined job files.
ov_unquarantine
# 5. Start the daemon.
ov_log_mark
if ! ov_bootstrap; then
    ov_diagnostics
    ov_die "the daemon did not start"
fi
# 6. Let Dante traffic through the application firewall.
ov_firewall
# 7-8. Have Core Audio load the new driver, and wait for it to connect:
# the driver in a helper process that the new coreaudiod started, since
# the old driver may already have connected to the new daemon. Without a
# restart, the driver Core Audio already runs reconnects by itself.
WAIT=0
if [ "$WITH_DRIVER" = 1 ] && [ "$RESTART" = 1 ]; then
    ov_restart_coreaudiod || ov_die "could not restart coreaudiod"
    WAIT=1
elif ov_helper_running; then
    WAIT=1
else
    ov_log "the driver loads when Core Audio next restarts (sudo killall coreaudiod)"
fi
if [ "$WAIT" = 1 ]; then
    ov_wait_driver || exit 1
fi
ov_log "installed; the daemon's log is $OV_LOG"

#!/bin/bash
# Removes OpenVirtualSoundcard from this Mac: the daemon and its launchd job, the Core
# Audio driver, the OpenVirtualSoundcard app, the command-line link, the log rotation
# rule and the package receipt. It then restarts Core Audio, so that the OpenVirtualSoundcard device goes
# away. The configuration, the remembered state and the logs stay, for a
# later install, unless --purge is given.
#
# Installed as "/Library/Application Support/OpenVirtualSoundcard/uninstall.sh". It
# needs nothing else from the install, so it also works on a damaged one.
#
# Usage: sudo "/Library/Application Support/OpenVirtualSoundcard/uninstall.sh" [--purge]
#   --purge   also remove the configuration, the state and the logs
#
# The script removes itself, so everything runs from main, which bash has
# read in full before it starts.
set -uo pipefail
PATH=/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}

LABEL=org.openvirtualsoundcard.daemon
PKG_ID=org.openvirtualsoundcard.pkg
DRIVER=/Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver
SUPPORT="/Library/Application Support/OpenVirtualSoundcard"
BIN="$SUPPORT/bin/ovsc"
LINK=/usr/local/bin/ovsc
PLIST=/Library/LaunchDaemons/$LABEL.plist
LOG_DIR=/Library/Logs/OpenVirtualSoundcard
NEWSYSLOG=/etc/newsyslog.d/org.openvirtualsoundcard.conf
FIREWALL=/usr/libexec/ApplicationFirewall/socketfilterfw
APP=/Applications/OpenVirtualSoundcard.app
APP_ID=org.openvirtualsoundcard.app
RUN_DIR=/var/run/ovsc

log() { echo "uninstall.sh: $*"; }
warn() { echo "uninstall.sh: warning: $*" >&2; }

# Removes the given paths, recursively, and records any failure.
remove() {
    local p
    for p in "$@"; do
        if [ -e "$p" ] || [ -L "$p" ]; then
            log "removing $p"
            rm -rf "$p" || { warn "could not remove $p"; FAILED=1; }
        fi
    done
}

# Stops the daemon and unloads its job, waiting up to 20 s.
stop_daemon() {
    launchctl print "system/$LABEL" >/dev/null 2>&1 || return 0
    log "stopping the daemon"
    launchctl bootout "system/$LABEL" 2>/dev/null || true
    local end=$((SECONDS + 20))
    while [ "$SECONDS" -lt "$end" ]; do
        launchctl print "system/$LABEL" >/dev/null 2>&1 || return 0
        sleep 0.5
    done
    warn "launchd still has $LABEL loaded"
    FAILED=1
}

# Restarts coreaudiod so that it drops the driver: SIGTERM, then SIGKILL if
# the same coreaudiod is still running 5 s later. Never `launchctl
# kickstart`, which macOS 14.4 and later refuse for coreaudiod.
restart_coreaudiod() {
    local old end pids
    old=$(pgrep -x coreaudiod | head -n 1 || true)
    [ -n "$old" ] || return 0
    log "restarting coreaudiod (pid $old)"
    killall coreaudiod 2>/dev/null || true
    end=$((SECONDS + 5))
    while [ "$SECONDS" -lt "$end" ]; do
        pids=" $(pgrep -x coreaudiod | tr '\n' ' ') "
        case "$pids" in
        *" $old "*) sleep 0.5 ;;
        *) return 0 ;;
        esac
    done
    warn "coreaudiod (pid $old) is still running after 5 s; killing it"
    killall -9 coreaudiod 2>/dev/null || true
}

main() {
    local purge=0
    while [ $# -gt 0 ]; do
        case "$1" in
        --purge) purge=1 ;;
        -h | --help)
            sed -n '/^# Usage:/,/^#$/s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        *)
            echo "uninstall.sh: unknown argument '$1' (--purge)" >&2
            exit 2
            ;;
        esac
        shift
    done
    [ "$(uname -s)" = Darwin ] || { echo "uninstall.sh: this is for macOS" >&2; exit 1; }
    [ "$(id -u)" = 0 ] || { echo "uninstall.sh: run as root: sudo $0" >&2; exit 1; }

    FAILED=0
    stop_daemon
    if [ -x "$FIREWALL" ] && [ -e "$BIN" ]; then
        "$FIREWALL" --remove "$BIN" >/dev/null 2>&1 || true
    fi
    remove "$PLIST" "$DRIVER" "$NEWSYSLOG" "$RUN_DIR"
    # The app only if it is ours.
    if [ -d "$APP" ]; then
        if [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist" 2>/dev/null)" = "$APP_ID" ]; then
            remove "$APP"
        else
            warn "$APP is not the OpenVirtualSoundcard app ($APP_ID); leaving it alone"
        fi
    fi
    # The link only if it is ours.
    if [ -L "$LINK" ] && [ "$(readlink "$LINK")" = "$BIN" ]; then
        remove "$LINK"
    fi
    if [ "$purge" = 1 ]; then
        remove "$SUPPORT" "$LOG_DIR"
    else
        remove "$SUPPORT/bin" "$SUPPORT/uninstall.sh" "$SUPPORT/THIRD-PARTY-LICENSES.html"
        if rmdir "$SUPPORT" 2>/dev/null; then
            log "removed the empty $SUPPORT"
        elif [ -d "$SUPPORT" ]; then
            log "keeping the configuration in $SUPPORT and the logs in $LOG_DIR"
        fi
    fi
    if pkgutil --pkg-info "$PKG_ID" >/dev/null 2>&1; then
        pkgutil --forget "$PKG_ID" >/dev/null || { warn "pkgutil --forget $PKG_ID failed"; FAILED=1; }
    fi
    restart_coreaudiod
    if [ "$FAILED" = 1 ]; then
        echo "uninstall.sh: finished with errors" >&2
        exit 1
    fi
    log "OpenVirtualSoundcard is uninstalled"
}

main "$@"

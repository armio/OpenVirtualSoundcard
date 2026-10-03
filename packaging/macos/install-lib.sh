# shellcheck shell=bash
# The OpenVirtualSoundcard install steps, shared by install.sh and the package scripts
# (scripts/preinstall and scripts/postinstall): installing the files,
# writing a default configuration, starting the daemon and restarting Core
# Audio so that it loads the driver.
#
# Sourced, not run. The functions expect macOS and root. They work with or
# without `set -e`: each one handles its own errors, prints what went wrong
# and returns non-zero, and the caller decides whether that is fatal.
#
# uninstall.sh repeats the paths it removes, so that it keeps working on
# its own.

# The constants are also for the scripts that source this file.
# shellcheck disable=SC2034

OV_LABEL=org.openvirtualsoundcard.daemon
OV_PKG_ID=org.openvirtualsoundcard.pkg
OV_DRIVER=/Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver
OV_SUPPORT="/Library/Application Support/OpenVirtualSoundcard"
OV_BIN="$OV_SUPPORT/bin/ovsc"
OV_CONFIG="$OV_SUPPORT/ovsc.toml"
OV_UNINSTALLER="$OV_SUPPORT/uninstall.sh"
OV_LINK=/usr/local/bin/ovsc
OV_PLIST=/Library/LaunchDaemons/$OV_LABEL.plist
OV_LOG_DIR=/Library/Logs/OpenVirtualSoundcard
OV_LOG=$OV_LOG_DIR/ovsc.log
OV_NEWSYSLOG=/etc/newsyslog.d/org.openvirtualsoundcard.conf
OV_FIREWALL=/usr/libexec/ApplicationFirewall/socketfilterfw
OV_APP=/Applications/OpenVirtualSoundcard.app
OV_APP_ID=org.openvirtualsoundcard.app
# The process Core Audio runs the driver in, as a `pgrep -f` pattern.
OV_HELPER='Core Audio Driver \(OpenVirtualSoundcard\.driver\)'
# The line the daemon logs when the driver has connected to it, followed by
# ": pid <helper process>,".
OV_ATTACHED='hal: plug-in attached'

# For ov_wait_driver: how long the daemon log was (ov_log_mark), and the
# helper processes that ran before the Core Audio restart
# (ov_restart_coreaudiod), space separated with a space at each end.
OV_LOG_MARK=0
OV_OLD_HELPERS=' '

# The install of OpenDante, OpenVirtualSoundcard's former name, which an
# install replaces (ov_migrate_legacy).
OV_LEGACY_LABEL=org.opendante.daemon
OV_LEGACY_PKG_ID=org.opendante.pkg
OV_LEGACY_DRIVER=/Library/Audio/Plug-Ins/HAL/OpenDante.driver
OV_LEGACY_SUPPORT="/Library/Application Support/OpenDante"
OV_LEGACY_PLIST=/Library/LaunchDaemons/$OV_LEGACY_LABEL.plist
OV_LEGACY_APP=/Applications/OpenDante.app
OV_LEGACY_LINK=/usr/local/bin/opendante
OV_LEGACY_NEWSYSLOG=/etc/newsyslog.d/org.opendante.conf
OV_LEGACY_RUN_DIR=/var/run/opendante

# The directory holding this file and the files installed next to it.
OV_SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# System tools first: GNU versions from Homebrew take other options.
PATH=/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}
export PATH

ov_log() { echo "${0##*/}: $*"; }
ov_warn() { echo "${0##*/}: warning: $*" >&2; }
ov_err() { echo "${0##*/}: $*" >&2; }
ov_die() {
    ov_err "$*"
    exit 1
}

# Runs a command with a time limit in seconds (macOS has no timeout(1)).
ov_limit() {
    local secs=$1
    shift
    perl -e 'alarm shift; exec @ARGV' "$secs" "$@"
}

# Whether launchd has the daemon's job loaded.
ov_loaded() {
    launchctl print "system/$OV_LABEL" >/dev/null 2>&1
}

# Whether the driver's helper process is running, that is, whether Core
# Audio has loaded the driver.
ov_helper_running() {
    pgrep -f "$OV_HELPER" >/dev/null 2>&1
}

# The process IDs of the driver's helper processes, space separated with a
# space at each end (one space when there is none).
ov_helper_pids() {
    echo " $(pgrep -f "$OV_HELPER" | tr '\n' ' ')"
}

# The process IDs of coreaudiod, space separated with a space at each end.
ov_coreaudiod_pids() {
    echo " $(pgrep -x coreaudiod | tr '\n' ' ') "
}

# Whether coreaudiod is running.
ov_coreaudiod_running() {
    pgrep -x coreaudiod >/dev/null 2>&1
}

# Stops the daemon and unloads its job, if loaded, then waits up to 20 s
# for launchd to let go of it.
ov_bootout() {
    ov_loaded || return 0
    ov_log "stopping the daemon"
    launchctl bootout "system/$OV_LABEL" 2>/dev/null || true
    local end=$((SECONDS + 20))
    while [ "$SECONDS" -lt "$end" ]; do
        ov_loaded || return 0
        sleep 0.5
    done
    ov_err "launchd still has $OV_LABEL loaded after 20 s"
    return 1
}

# Whether OpenDante, the former name, is installed.
ov_legacy_installed() {
    [ -e "$OV_LEGACY_PLIST" ] || [ -d "$OV_LEGACY_DRIVER" ] || [ -d "$OV_LEGACY_SUPPORT/bin" ]
}

# Replaces an OpenDante install: stops its daemon, carries its configuration
# and saved state over to the new paths (unless a configuration is already
# there) and removes its files. Its logs stay in /Library/Logs/OpenDante.
# Core Audio drops the old driver when it next restarts, which the install
# does.
ov_migrate_legacy() {
    ov_legacy_installed || return 0
    ov_log "replacing OpenDante, this project's former name"
    if launchctl print "system/$OV_LEGACY_LABEL" >/dev/null 2>&1; then
        launchctl bootout "system/$OV_LEGACY_LABEL" 2>/dev/null || true
        local end=$((SECONDS + 20))
        while launchctl print "system/$OV_LEGACY_LABEL" >/dev/null 2>&1; do
            if [ "$SECONDS" -ge "$end" ]; then
                ov_err "the OpenDante daemon ($OV_LEGACY_LABEL) does not stop"
                return 1
            fi
            sleep 0.5
        done
    fi
    mkdir -p "$OV_SUPPORT" || return 1
    if [ ! -e "$OV_CONFIG" ] && [ -f "$OV_LEGACY_SUPPORT/opendante.toml" ]; then
        ov_log "moving the configuration to $OV_CONFIG"
        sed -e 's#Application Support/OpenDante#Application Support/OpenVirtualSoundcard#g' \
            -e 's#org\.opendante\.#org.openvirtualsoundcard.#g' \
            -e 's#/var/run/opendante/#/var/run/ovsc/#g' \
            "$OV_LEGACY_SUPPORT/opendante.toml" >"$OV_CONFIG" || return 1
        chown root:wheel "$OV_CONFIG" && chmod 644 "$OV_CONFIG" || return 1
    fi
    if [ ! -e "$OV_SUPPORT/state.toml" ] && [ -f "$OV_LEGACY_SUPPORT/state.toml" ]; then
        cp -p "$OV_LEGACY_SUPPORT/state.toml" "$OV_SUPPORT/state.toml" || return 1
    fi
    if [ -x "$OV_FIREWALL" ] && [ -e "$OV_LEGACY_SUPPORT/bin/opendante" ]; then
        "$OV_FIREWALL" --remove "$OV_LEGACY_SUPPORT/bin/opendante" >/dev/null 2>&1 || true
    fi
    if [ -L "$OV_LEGACY_LINK" ] &&
        [ "$(readlink "$OV_LEGACY_LINK")" = "$OV_LEGACY_SUPPORT/bin/opendante" ]; then
        rm -f "$OV_LEGACY_LINK"
    fi
    if [ -d "$OV_LEGACY_APP" ] && [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \
        "$OV_LEGACY_APP/Contents/Info.plist" 2>/dev/null)" = org.opendante.app ]; then
        rm -rf "$OV_LEGACY_APP"
    fi
    rm -rf "$OV_LEGACY_PLIST" "$OV_LEGACY_DRIVER" "$OV_LEGACY_SUPPORT" "$OV_LEGACY_NEWSYSLOG" \
        "$OV_LEGACY_RUN_DIR" || return 1
    if pkgutil --pkg-info "$OV_LEGACY_PKG_ID" >/dev/null 2>&1; then
        pkgutil --forget "$OV_LEGACY_PKG_ID" >/dev/null || ov_warn "pkgutil --forget $OV_LEGACY_PKG_ID failed"
    fi
}

# Copies the driver bundle DRIVER (skipped when empty) and the daemon binary
# DAEMON into place, with the launchd job, the log rotation rule and the
# uninstaller from this directory, then runs ov_finish_files. The daemon
# must be stopped first (ov_bootout).
ov_install_files() {
    local driver=$1 daemon=$2
    mkdir -p "$OV_SUPPORT/bin" "${OV_PLIST%/*}" "${OV_NEWSYSLOG%/*}" || return 1
    if [ -n "$driver" ]; then
        ov_log "installing $OV_DRIVER"
        mkdir -p "${OV_DRIVER%/*}" || return 1
        # A clean copy: no files left over from an older bundle.
        rm -rf "$OV_DRIVER" || return 1
        ditto "$driver" "$OV_DRIVER" || return 1
    fi
    ov_log "installing $OV_BIN"
    # install(1) unlinks the old file first. Overwriting a signed binary in
    # place would leave the kernel's code signature cache stale.
    install -m 755 "$daemon" "$OV_BIN" || return 1
    install -m 644 "$OV_SRC/org.openvirtualsoundcard.daemon.plist" "$OV_PLIST" || return 1
    install -m 644 "$OV_SRC/org.openvirtualsoundcard.newsyslog.conf" "$OV_NEWSYSLOG" || return 1
    install -m 755 "$OV_SRC/uninstall.sh" "$OV_UNINSTALLER" || return 1
    ov_finish_files
}

# Whether BUNDLE is the OpenVirtualSoundcard app, by its bundle identifier.
ov_is_our_app() {
    [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$1/Contents/Info.plist" 2>/dev/null)" = "$OV_APP_ID" ]
}

# Copies the app bundle APP to /Applications, replacing an older OpenVirtualSoundcard
# app but nothing else of that name.
ov_install_app() {
    local app=$1
    if [ -e "$OV_APP" ] && ! ov_is_our_app "$OV_APP"; then
        ov_err "$OV_APP is not the OpenVirtualSoundcard app ($OV_APP_ID); leaving it alone"
        return 1
    fi
    ov_log "installing $OV_APP"
    # A clean copy, as for the driver.
    rm -rf "$OV_APP" || return 1
    ditto "$app" "$OV_APP" || return 1
    chown -R root:wheel "$OV_APP" || return 1
    chmod -R go-w "$OV_APP" || return 1
    xattr -dr com.apple.quarantine "$OV_APP" 2>/dev/null || true
}

# Gives the installed files root ownership and standard modes (launchd
# rejects a job file that others can write), creates the log directory and
# links the command-line tool into /usr/local/bin. The package runs this on
# its payload; ov_install_files runs it after copying.
ov_finish_files() {
    local f
    for f in "$OV_BIN" "$OV_PLIST" "$OV_NEWSYSLOG"; do
        if [ ! -f "$f" ]; then
            ov_err "$f is missing"
            return 1
        fi
    done
    mkdir -p "$OV_LOG_DIR" || return 1
    chown root:wheel "$OV_LOG_DIR" "$OV_SUPPORT" "$OV_SUPPORT/bin" "$OV_BIN" \
        "$OV_PLIST" "$OV_NEWSYSLOG" || return 1
    chmod 755 "$OV_LOG_DIR" "$OV_SUPPORT" "$OV_SUPPORT/bin" "$OV_BIN" || return 1
    chmod 644 "$OV_PLIST" "$OV_NEWSYSLOG" || return 1
    if [ -f "$OV_UNINSTALLER" ]; then
        chown root:wheel "$OV_UNINSTALLER" || return 1
        chmod 755 "$OV_UNINSTALLER" || return 1
    fi
    if [ -d "$OV_DRIVER" ]; then
        chown -R root:wheel "$OV_DRIVER" || return 1
        find "$OV_DRIVER" -type d -exec chmod 755 {} + || return 1
        find "$OV_DRIVER" -type f -exec chmod 644 {} + || return 1
        chmod 755 "$OV_DRIVER/Contents/MacOS/OpenVirtualSoundcard" || return 1
    fi
    if [ -e "$OV_LINK" ] && [ ! -L "$OV_LINK" ]; then
        ov_warn "$OV_LINK exists and is not a link; leaving it alone"
    elif ! { mkdir -p "${OV_LINK%/*}" && ln -sfn "$OV_BIN" "$OV_LINK"; }; then
        ov_warn "could not link $OV_LINK"
    fi
    return 0
}

# This Mac's local host name made into a valid Dante device name (1 to 31 of
# A-Z a-z 0-9 and inner hyphens), or "ovsc" if nothing usable is left.
ov_device_name() {
    local raw name
    raw=$(scutil --get LocalHostName 2>/dev/null || true)
    name=$(printf '%s' "$raw" | tr -c 'A-Za-z0-9-' '-' | tr -s '-' |
        sed 's/^-*//' | cut -c 1-31 | sed 's/-*$//')
    echo "${name:-ovsc}"
}

# Writes the configuration from TEMPLATE, with the device named after this
# Mac, unless a configuration exists already: an upgrade keeps the user's.
ov_default_config() {
    local template=$1 name tmp
    if [ -e "$OV_CONFIG" ]; then
        ov_log "keeping $OV_CONFIG"
        return 0
    fi
    name=$(ov_device_name)
    ov_log "writing $OV_CONFIG (device name $name)"
    mkdir -p "$OV_SUPPORT" || return 1
    tmp=$OV_CONFIG.new
    # The first `name = ` line is the device's, in [device].
    awk -v name="$name" '!done && /^name = / { print "name = \"" name "\""; done = 1; next } { print }' \
        "$template" >"$tmp" || return 1
    if ! grep -qx "name = \"$name\"" "$tmp"; then
        ov_err "$template has no device name line"
        rm -f "$tmp"
        return 1
    fi
    chown root:wheel "$tmp" && chmod 644 "$tmp" && mv "$tmp" "$OV_CONFIG"
}

# Removes the quarantine attribute that a download leaves on the files:
# launchd refuses quarantined job files, and Gatekeeper would assess the
# driver and the daemon.
ov_unquarantine() {
    local p
    for p in "$OV_PLIST" "$OV_DRIVER" "$OV_SUPPORT"; do
        if [ -e "$p" ]; then
            xattr -dr com.apple.quarantine "$p" >/dev/null 2>&1 || true
        fi
    done
    return 0
}

# Loads and starts the daemon's job, booting out a loaded one first so that
# a new binary or job file takes effect, and waits up to 10 s for it to run.
ov_bootstrap() {
    ov_bootout || return 1
    mkdir -p "$OV_LOG_DIR" || return 1
    # A job disabled with `launchctl disable` would not bootstrap.
    launchctl enable "system/$OV_LABEL" 2>/dev/null || true
    ov_log "starting the daemon"
    local tries=0
    # Right after a bootout launchd can still refuse for a moment.
    until launchctl bootstrap system "$OV_PLIST"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 5 ]; then
            ov_err "launchctl bootstrap system $OV_PLIST failed"
            return 1
        fi
        sleep 1
    done
    local end=$((SECONDS + 10)) state
    while [ "$SECONDS" -lt "$end" ]; do
        state=$(launchctl print "system/$OV_LABEL" 2>/dev/null || true)
        case "$state" in
        *"state = running"*) return 0 ;;
        esac
        sleep 0.5
    done
    ov_err "the daemon is loaded but not running"
    return 1
}

# When the application firewall is on, lets the daemon accept incoming
# connections (Dante control and audio), as Dante Virtual Soundcard's
# installer does.
ov_firewall() {
    [ -x "$OV_FIREWALL" ] || return 0
    local state
    state=$("$OV_FIREWALL" --getglobalstate 2>/dev/null || true)
    case "$state" in
    *"is enabled"* | *"State = 1"* | *"State = 2"*) ;;
    *) return 0 ;;
    esac
    ov_log "allowing $OV_BIN through the application firewall"
    "$OV_FIREWALL" --add "$OV_BIN" >/dev/null 2>&1 || ov_warn "socketfilterfw --add failed"
    "$OV_FIREWALL" --unblockapp "$OV_BIN" >/dev/null 2>&1 ||
        ov_warn "socketfilterfw --unblockapp failed"
    return 0
}

# Restarts coreaudiod so that it loads the installed driver: SIGTERM, then
# SIGKILL if the same coreaudiod is still running 5 s later. launchd starts
# a new one when a client asks for it. Never `launchctl kickstart`: macOS
# 14.4 and later refuse it for coreaudiod.
#
# Records the driver's helper processes in OV_OLD_HELPERS first. They run
# the driver that was loaded before: right after ov_bootstrap it can
# connect to the new daemon, and the old helpers can outlive the restart
# for a while. ov_wait_driver therefore waits for a helper not among them.
ov_restart_coreaudiod() {
    local old end
    OV_OLD_HELPERS=$(ov_helper_pids)
    old=$(pgrep -x coreaudiod | head -n 1 || true)
    if [ -z "$old" ]; then
        ov_log "coreaudiod is not running; it loads the driver when it starts"
        return 0
    fi
    ov_log "restarting coreaudiod (pid $old)"
    killall coreaudiod 2>/dev/null || true
    end=$((SECONDS + 5))
    while [ "$SECONDS" -lt "$end" ]; do
        case "$(ov_coreaudiod_pids)" in
        *" $old "*) sleep 0.5 ;;
        *) return 0 ;;
        esac
    done
    ov_warn "coreaudiod (pid $old) is still running after 5 s; killing it"
    killall -9 coreaudiod 2>/dev/null || true
    sleep 1
    case "$(ov_coreaudiod_pids)" in
    *" $old "*)
        ov_err "could not stop coreaudiod (pid $old)"
        return 1
        ;;
    esac
    return 0
}

# Remembers how long the daemon log is now: ov_wait_driver looks only at
# what the daemon logs after this. Call it before ov_bootstrap.
ov_log_mark() {
    OV_LOG_MARK=0
    if [ -f "$OV_LOG" ]; then
        OV_LOG_MARK=$(wc -c <"$OV_LOG" | tr -d ' ')
    fi
    return 0
}

# Waits up to 30 s for Core Audio to load the driver and for the driver to
# connect to the daemon: a running helper process that is not one of
# OV_OLD_HELPERS (any helper, without a Core Audio restart), for which the
# daemon has logged "hal: plug-in attached: pid <helper>," since
# ov_log_mark. Prints diagnostics and returns 1 if there is none.
ov_wait_driver() {
    local p helpers=' ' attached size poked=no end=$((SECONDS + 30))
    ov_log "waiting for the driver to load and connect to the daemon"
    while [ "$SECONDS" -lt "$end" ]; do
        helpers=' '
        for p in $(ov_helper_pids); do
            case "$OV_OLD_HELPERS" in
            *" $p "*) ;;
            *) helpers="$helpers$p " ;;
            esac
        done
        if [ "$helpers" = ' ' ]; then
            if [ "$poked" = no ] && ! ov_coreaudiod_running; then
                # launchd starts coreaudiod on demand: ask it something.
                poked=yes
                ov_limit 20 system_profiler SPAudioDataType >/dev/null 2>&1 || true
                continue
            fi
        elif [ -f "$OV_LOG" ]; then
            size=$(wc -c <"$OV_LOG" | tr -d ' ')
            # Rotated or truncated since the mark: all of it is new.
            if [ "$size" -lt "$OV_LOG_MARK" ]; then
                OV_LOG_MARK=0
            fi
            # The helpers the daemon has logged as attached since the mark.
            # sed reads to the end: with pipefail, a reader that exits early
            # could kill tail with SIGPIPE and fail the pipeline.
            attached=" $(tail -c "+$((OV_LOG_MARK + 1))" "$OV_LOG" |
                sed -n "s/.*$OV_ATTACHED: pid \([0-9][0-9]*\),.*/\1/p" | tr '\n' ' ') "
            for p in $helpers; do
                case "$attached" in
                *" $p "*)
                    ov_log "the driver (helper pid $p) is loaded and connected to the daemon"
                    return 0
                    ;;
                esac
            done
        fi
        sleep 0.5
    done
    if [ "$helpers" = ' ' ] && [ "$OV_OLD_HELPERS" = ' ' ]; then
        ov_err "after 30 s: Core Audio runs no helper process for the driver"
    elif [ "$helpers" = ' ' ]; then
        ov_err "after 30 s: Core Audio has started no new helper process for the driver since the restart (before it: pid${OV_OLD_HELPERS% })"
    else
        ov_err "after 30 s: the daemon has not logged '$OV_ATTACHED' for the driver's helper process (pid${helpers% })"
    fi
    ov_diagnostics
    return 1
}

# Prints what helps find out why the driver or the daemon is not running.
ov_diagnostics() {
    echo "--- launchctl print system/$OV_LABEL"
    launchctl print "system/$OV_LABEL" 2>&1 | head -n 40 || true
    echo "--- $OV_LOG (last 30 lines)"
    tail -n 30 "$OV_LOG" 2>&1 || true
    echo "--- processes"
    # Full ps lines (user, age, arguments) say more than pgrep.
    # shellcheck disable=SC2009
    ps axo pid,user,etime,command | grep -E 'coreaudiod|Core Audio Driver|ovsc' |
        grep -v grep || true
    echo "--- unified log, last 2 minutes"
    ov_limit 30 log show --last 2m --style compact --predicate \
        'subsystem == "org.openvirtualsoundcard" OR process CONTAINS "Core-Audio-Driver" OR (process == "coreaudiod" AND eventMessage CONTAINS[c] "OpenVirtualSoundcard")' \
        2>&1 | tail -n 40 || true
}

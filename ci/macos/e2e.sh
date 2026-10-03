#!/bin/bash
# End-to-end test of OpenVirtualSoundcard on macOS (docs/MACOS.md, "Testing"), for a
# GitHub-hosted macOS runner: passwordless sudo, no other Dante devices. On
# your own Mac, run it through ci/macos/e2e-local.sh.
#
# It builds everything, starts a development PTP master on 127.0.0.1 at
# +50 ppm, installs the daemon and the driver with packaging/macos/install.sh
# using ci/macos/e2e.toml (a device whose receive channels are subscribed to
# its own transmit channels), and then runs scenarios S1-S11 against the
# Core Audio device with tools/coreaudio-check.
#
# Usage: ci/macos/e2e.sh [S1 S2 ...]    (default: every scenario)
# Each scenario prints PASS or FAIL; the last line is an E2E-SUMMARY. The
# script exits 1 if any scenario failed.
# Scenario functions are called indirectly, by name.
# shellcheck disable=SC2317
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT" || exit 1

DEV=org.openvirtualsoundcard.vsc
APP_SUPPORT="/Library/Application Support/OpenVirtualSoundcard"
CONFIG="$APP_SUPPORT/ovsc.toml"
LOG=/Library/Logs/OpenVirtualSoundcard/ovsc.log
LABEL=org.openvirtualsoundcard.daemon
PLIST=/Library/LaunchDaemons/$LABEL.plist
CHECK=$ROOT/tools/coreaudio-check/target/release/coreaudio-check
OD=$ROOT/target/release/ovsc
SELFTEST=$ROOT/target/release/ovsc-hal-selftest
WORK=${E2E_WORK:-$ROOT/target/e2e}
RATE_PPM=50
# The macOS runners are small VMs whose scheduling stalls now and then
# outlast the 4 ms network latency or a Core Audio cycle: the daemon then
# logs late packets, the driver late_out, and Core Audio skips IO. Where a
# scenario restarts the daemon or runs tiny buffers, it may lose this many
# frames per channel to that (10 ms at 48 kHz). S4's steady state may lose
# none, and audio that does arrive must always be in its place.
STALL_FRAMES=480
mkdir -p "$WORK"

SCENARIOS=("$@")
if [ ${#SCENARIOS[@]} -eq 0 ]; then
    SCENARIOS=(S1 S2 S3 S4 S5 S6 S7 S8 S9 S10 S11)
fi
RESULTS=()
FAILED=0

group() { echo "::group::$1"; }
endgroup() { echo "::endgroup::"; }
# Runs a command with a time limit in seconds; coreaudiod has no watchdog,
# so a hung client would otherwise hang the job.
limit() { local secs=$1; shift; perl -e 'alarm shift; exec @ARGV' "$secs" "$@"; }
# A key from a "k=v k=v" status line.
kv() { awk -v k="$2" '{ for (i = 1; i <= NF; i++) { n = index($i, "="); if (substr($i, 1, n - 1) == k) print substr($i, n + 1) } }' <<<"$1"; }
ovst() { limit 10 "$CHECK" prop "$DEV" ovst --type cfstring 2>/dev/null; }
now() { date '+%Y-%m-%d %H:%M:%S'; }
coreaudiod_pid() { pgrep -x coreaudiod | head -1; }
kickstart() { sudo launchctl kickstart -k "system/$LABEL"; }
use_config() { sudo cp "$1" "$CONFIG"; }

# coreaudiod log lines that mean the HAL rejected our zero timestamps.
reanchors_since() {
    sudo log show --start "$1" --style compact --predicate 'process == "coreaudiod"' 2>/dev/null |
        grep -cE 'TimeStampOutOfLine|Re-anchoring|not consecutive|clockResetReason'
}

# Waits up to SECS for the device's rate (ovst ppm) to stay within TOL ppm
# of the test master's for 8 readings a second apart. The daemon's PTP servo
# reports Locked on phase; for about 20 s after that its frequency fit keeps
# converging and its proportional term follows the timestamp noise, so the
# rate swings by tens of ppm (see crates/ovsc-clock/src/ptp/servo.rs).
# The device follows it.
wait_rate_settled() {
    local secs=$1 tol=$2 line p run=0
    for _ in $(seq 1 "$secs"); do
        line=$(ovst)
        p=$(kv "$line" ppm)
        if [ -n "$p" ] &&
            awk -v p="$p" -v w="$RATE_PPM" -v t="$tol" 'BEGIN { d = p - w; exit !(d <= t && d >= -t) }'; then
            run=$((run + 1))
            if [ "$run" -ge 8 ]; then
                echo "device rate settled at $p ppm"
                return 0
            fi
        else
            run=0
        fi
        sleep 1
    done
    echo "device rate not within $tol ppm of +$RATE_PPM for 8 s within $secs s: $line"
    return 1
}

# The coreaudio-check snapshot lines of a run log, by label.
snapshot_line() { sed -n "s/^snapshot $2 'ovst': //p" "$1" | tail -1; }

# Fails if any of the counters moved between two status lines.
same_counters() {
    local a=$1 b=$2 ok=0 k
    for k in absorbs seed late_out early_in far_out far_in tx_underruns; do
        if [ "$(kv "$a" "$k")" != "$(kv "$b" "$k")" ]; then
            echo "counter $k changed: $(kv "$a" "$k") -> $(kv "$b" "$k")"
            ok=1
        fi
    done
    return "$ok"
}

# loopback NAME ARGS...: runs coreaudio-check loopback, logs to $WORK/NAME.log.
loopback() {
    local name=$1; shift
    limit 180 "$CHECK" loopback "$DEV" "$@" >"$WORK/$name.log" 2>&1
    local rc=$?
    grep -vE '^(cycle [0-9]|input stream|output stream)' "$WORK/$name.log" | tail -40
    return "$rc"
}

setup() {
    group "Setup: system"
    sw_vers; uname -m; csrutil status
    echo "coreaudiod pid $(coreaudiod_pid)"
    endgroup

    group "Setup: build"
    cargo build --release --locked -p ovsc -p ovsc-hal --bins || return 1
    packaging/macos/build-driver.sh || return 1
    (cd tools/coreaudio-check && cargo build --release) || return 1
    "$CHECK" list
    endgroup

    group "Setup: PTP master at +$RATE_PPM ppm"
    sudo pkill -f "ovsc ptp-master" 2>/dev/null
    # The log stays the runner's own file.
    # shellcheck disable=SC2024
    sudo "$OD" ptp-master -i 127.0.0.1 --event-port 10319 --general-port 10320 \
        --rate-ppm "$RATE_PPM" >"$WORK/ptp-master.log" 2>&1 &
    sleep 1
    tail -5 "$WORK/ptp-master.log"
    endgroup

    group "Setup: install"
    sudo packaging/macos/install.sh --config ci/macos/e2e.toml || return 1
    endgroup
}

s1() {
    limit 70 "$CHECK" wait "$DEV" 60 || return 1
    pgrep -fl 'Core Audio Driver \(OpenVirtualSoundcard.driver\)' || { echo "no driver helper process"; return 1; }
    for _ in $(seq 1 30); do
        sudo grep -q 'hal: plug-in attached' "$LOG" && { echo "daemon: plug-in attached"; return 0; }
        sleep 1
    done
    echo "the daemon never logged 'hal: plug-in attached'"
    return 1
}

s2() {
    local names
    names=$(seq -f '%02g' -s, 1 8)
    limit 30 "$CHECK" check "$DEV" --rate 48000 --inputs 8 --outputs 8 --zts-period 16384 \
        --input-safety 216 --output-safety 55 --output-latency 192 --clock-algorithm raww \
        --element-names "$names" || return 1
    limit 60 "$CHECK" walk "$DEV"
}

s3() {
    # shellcheck disable=SC2024
    sudo "$SELFTEST" --clock-timeout 60 >"$WORK/selftest.log" 2>&1
    local rc=$?
    cat "$WORK/selftest.log"
    [ "$rc" -eq 0 ] && grep -q 'SELFTEST-SUMMARY.*PASS' "$WORK/selftest.log"
}

s4() {
    limit 70 "$CHECK" wait-status "$DEV" clock=following 60 || return 1
    wait_rate_settled 120 2 || return 1
    local t0 rc a b
    t0=$(now)
    loopback s4 --seconds 20 --channels 8 --rate 48000 --buffer 512 --expect-device-delay 0 \
        --expect-rate-ppm "$RATE_PPM" --rate-tol-ppm 3 --snapshot-prop ovst
    rc=$?
    a=$(snapshot_line "$WORK/s4.log" 'at 25%')
    b=$(snapshot_line "$WORK/s4.log" 'at 75%')
    echo "ovst at 25%: $a"
    echo "ovst at 75%: $b"
    same_counters "$a" "$b" || rc=1
    local n
    n=$(reanchors_since "$t0")
    echo "coreaudiod re-anchor lines during the run: $n"
    [ "$n" = 0 ] || rc=1
    echo "--- daemon io_trace and status ---"
    sudo grep -E 'io_trace|hal:' "$LOG" | tail -80
    return "$rc"
}

s5() {
    local rc=0 n jumps bad slips
    for n in 32 128 1024 4096; do
        jumps=3 bad=0 slips=0
        [ "$n" = 32 ] && jumps=10
        # Cycles of 0.7 and 2.7 ms leave the least room for stalls.
        if [ "$n" -le 128 ]; then bad=$((5 * STALL_FRAMES)) slips=20; fi
        echo "--- buffer $n ---"
        loopback "s5-$n" --seconds 5 --channels 8 --buffer "$n" --expect-device-delay 0 \
            --max-jumps "$jumps" --max-bad "$bad" --max-slips "$slips" || rc=1
        echo "ovst: $(ovst)"
    done
    return "$rc"
}

s6() {
    local before after rc t0 n
    before=$(ovst)
    echo "before: $before"
    t0=$(now)
    # A restart can starve the runner's Core Audio for a cycle (the HAL then
    # skips ahead, which the jump allowance covers). The device's own
    # timeline must not jump: no re-anchors, and the seed stays the same.
    limit 120 "$CHECK" loopback "$DEV" --seconds 40 --channels 8 --buffer 512 --allow-outage 15 \
        --expect-device-delay 0 --max-jumps 2 --max-bad $((5 * STALL_FRAMES)) --max-slips 4 \
        >"$WORK/s6.log" 2>&1 &
    local pid=$!
    sleep 10
    echo "restarting the daemon at $(now)"
    kickstart
    wait "$pid"
    rc=$?
    grep -vE '^(cycle [0-9]|input stream|output stream)' "$WORK/s6.log" | tail -30
    after=$(ovst)
    echo "after: $after"
    if [ "$(kv "$before" seed)" != "$(kv "$after" seed)" ]; then
        echo "the seed changed across a daemon restart"
        rc=1
    fi
    n=$(reanchors_since "$t0")
    echo "coreaudiod re-anchor lines during the run: $n"
    [ "$n" = 0 ] || rc=1
    if [ "$(kv "$after" attach)" != "$(($(kv "$before" attach) + 1))" ]; then
        echo "attach count $(kv "$before" attach) -> $(kv "$after" attach), want +1"
        rc=1
    fi
    return "$rc"
}

s7() {
    local attached old
    attached=$(sudo grep -c 'hal: plug-in attached' "$LOG")
    old=$(coreaudiod_pid)
    sudo killall coreaudiod
    sleep 2
    echo "coreaudiod $old -> $(coreaudiod_pid)"
    limit 70 "$CHECK" wait "$DEV" 60 || return 1
    for _ in $(seq 1 30); do
        [ "$(sudo grep -c 'hal: plug-in attached' "$LOG")" -gt "$attached" ] && break
        sleep 1
    done
    [ "$(sudo grep -c 'hal: plug-in attached' "$LOG")" -gt "$attached" ] ||
        { echo "the new driver instance never attached"; return 1; }
    limit 70 "$CHECK" wait-status "$DEV" daemon=attached 60 || return 1
    loopback s7 --seconds 10 --channels 8 --expect-device-delay 0 --max-bad "$STALL_FRAMES" ||
        return 1
    limit 30 "$CHECK" check "$DEV" --rate 48000
}

# Waits until `check ARGS` passes, for up to SECS.
wait_check() {
    local secs=$1; shift
    for _ in $(seq 1 "$secs"); do
        limit 30 "$CHECK" check "$DEV" "$@" >"$WORK/wait-check.log" 2>&1 && { cat "$WORK/wait-check.log"; return 0; }
        sleep 1
    done
    cat "$WORK/wait-check.log"
    return 1
}

s8() {
    local pid
    pid=$(coreaudiod_pid)
    use_config ci/macos/e2e-4ch.toml
    kickstart
    wait_check 30 --inputs 4 --outputs 4 || return 1
    [ "$(coreaudiod_pid)" = "$pid" ] || { echo "coreaudiod restarted"; return 1; }
    limit 70 "$CHECK" wait-status "$DEV" clock=following 60 || return 1
    loopback s8 --seconds 5 --channels 4 --expect-device-delay 0 --max-bad "$STALL_FRAMES"
}

s9() {
    use_config ci/macos/e2e-96k.toml
    kickstart
    limit 40 "$CHECK" wait-rate "$DEV" 96000 30 || return 1
    limit 70 "$CHECK" wait-status "$DEV" clock=following 60 || return 1
    wait_rate_settled 120 2 || return 1
    local rc=0
    loopback s9 --seconds 5 --channels 8 --expect-device-delay 0 --max-bad $((2 * STALL_FRAMES)) \
        --expect-rate-ppm "$RATE_PPM" --rate-tol-ppm 3 || rc=1
    use_config ci/macos/e2e.toml
    kickstart
    limit 40 "$CHECK" wait-rate "$DEV" 48000 30 || rc=1
    return "$rc"
}

s10() {
    # Holdover keeps the rate the device last followed: make sure it has
    # one, settled (the last scenario may have just restarted the daemon).
    limit 70 "$CHECK" wait-status "$DEV" clock=following 60 || return 1
    wait_rate_settled 120 2 || return 1
    sudo launchctl bootout "system/$LABEL"
    sleep 2
    local alive
    alive=$(limit 10 "$CHECK" prop "$DEV" livn --type u32)
    echo "alive with the daemon gone: $alive"
    [ "$alive" = 1 ] || return 1
    loopback s10-absent --seconds 10 --channels 8 --expect-silent-input \
        --expect-rate-ppm "$RATE_PPM" --rate-tol-ppm 5 || return 1
    sudo launchctl bootstrap system "$PLIST" || return 1
    limit 40 "$CHECK" wait-status "$DEV" daemon=attached 30 || return 1
    limit 70 "$CHECK" wait-status "$DEV" clock=following 60 || return 1
    loopback s10-back --seconds 5 --channels 8 --expect-device-delay 0 --max-bad "$STALL_FRAMES"
}

s11() {
    packaging/macos/build-pkg.sh || return 1
    local pkgs=(dist/OpenVirtualSoundcard-*.pkg) pkg
    pkg=${pkgs[${#pkgs[@]}-1]}
    echo "package: $pkg"
    sudo installer -pkg "$pkg" -target / || return 1
    limit 70 "$CHECK" wait "$DEV" 60 || return 1
    pkgutil --files org.openvirtualsoundcard.pkg 2>/dev/null | head -40
    # --purge removes the daemon's log; keep it for diagnostics.sh.
    sudo cp "$LOG" "$WORK/ovsc.log" 2>/dev/null
    sudo "$APP_SUPPORT/uninstall.sh" --purge || return 1
    sleep 3
    if limit 15 "$CHECK" wait "$DEV" 10 >/dev/null 2>&1; then
        echo "the device is still there after uninstall"
        return 1
    fi
    if pkgutil --pkgs | grep -q '^org.openvirtualsoundcard.pkg$'; then
        echo "pkgutil still knows org.openvirtualsoundcard.pkg"
        return 1
    fi
    local p
    for p in "$PLIST" /Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver \
        /Applications/OpenVirtualSoundcard.app "$APP_SUPPORT"; do
        [ -e "$p" ] && { echo "left behind: $p"; return 1; }
    done
    return 0
}

run() {
    local id=$1 fn
    fn=$(tr '[:upper:]' '[:lower:]' <<<"$id")
    group "$id"
    local start=$SECONDS
    if "$fn"; then
        echo "$id: PASS ($((SECONDS - start)) s)"
        RESULTS+=("$id=PASS")
    else
        echo "$id: FAIL ($((SECONDS - start)) s)"
        RESULTS+=("$id=FAIL")
        FAILED=1
    fi
    endgroup
}

T0=$(now)
echo "$T0" >"$WORK/t0"
if ! setup; then
    endgroup
    echo "E2E-SUMMARY os=$(sw_vers -productVersion) arch=$(uname -m) setup=FAIL"
    exit 1
fi
for s in "${SCENARIOS[@]}"; do
    run "$s"
done
sudo pkill -f "ovsc ptp-master" 2>/dev/null
echo "E2E-SUMMARY os=$(sw_vers -productVersion) arch=$(uname -m) ${RESULTS[*]}"
exit $FAILED

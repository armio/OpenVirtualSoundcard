#!/bin/bash
# Prints everything useful after an OpenVirtualSoundcard macOS end-to-end run, pass or
# fail, into the job log (artifacts are not uploaded).
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CHECK=$ROOT/tools/coreaudio-check/target/release/coreaudio-check
DEV=org.openvirtualsoundcard.vsc
LOG=/Library/Logs/OpenVirtualSoundcard/ovsc.log
DRIVER=/Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver
T0=$(cat "$ROOT/target/e2e/t0" 2>/dev/null || date -v-30M '+%Y-%m-%d %H:%M:%S')

group() { echo "::group::$1"; }
endgroup() { echo "::endgroup::"; }
limit() { local secs=$1; shift; perl -e 'alarm shift; exec @ARGV' "$secs" "$@"; }

group "Daemon log (last 400 lines)"
# S11 uninstalls with --purge after copying the log to target/e2e.
if sudo test -f "$LOG"; then
    sudo tail -400 "$LOG"
elif [ -f "$ROOT/target/e2e/ovsc.log" ]; then
    echo "(the copy e2e.sh kept before uninstalling)"
    tail -400 "$ROOT/target/e2e/ovsc.log"
else
    echo "no $LOG"
fi
endgroup

group "Device"
if [ -x "$CHECK" ]; then
    limit 20 "$CHECK" list
    limit 20 "$CHECK" info "$DEV"
    echo "ovst: $(limit 10 "$CHECK" prop "$DEV" ovst --type cfstring 2>&1)"
fi
endgroup

group "launchd"
sudo launchctl print system/org.openvirtualsoundcard.daemon 2>&1 | head -60
endgroup

group "Processes"
# Full ps lines (user, parent, age) say more than pgrep.
# shellcheck disable=SC2009
ps axo pid,user,ppid,etime,comm | grep -iE 'audio|ovsc' | grep -v grep
endgroup

group "Installed driver"
if [ -d "$DRIVER" ]; then
    ls -laR "$DRIVER"
    codesign -dvvv "$DRIVER" 2>&1
    plutil -p "$DRIVER/Contents/Info.plist"
    nm -gU "$DRIVER/Contents/MacOS/OpenVirtualSoundcard"
    otool -L "$DRIVER/Contents/MacOS/OpenVirtualSoundcard"
else
    echo "not installed"
fi
endgroup

group "Unified log since $T0 (last 3000 lines)"
sudo log show --start "$T0" --style compact --info --debug --predicate \
    'subsystem == "org.openvirtualsoundcard" OR process CONTAINS "Core-Audio-Driver" OR (process == "coreaudiod" AND (eventMessage CONTAINS[c] "OpenVirtualSoundcard" OR eventMessage CONTAINS "clockReset" OR eventMessage CONTAINS "TimeStampOutOfLine")) OR process == "amfid"' \
    2>/dev/null | tail -n 3000
endgroup

group "Crash reports"
for d in /Library/Logs/DiagnosticReports "$HOME/Library/Logs/DiagnosticReports"; do
    sudo find "$d" -newermt "$T0" \( -iname '*coreaudiod*' -o -iname '*Core-Audio-Driver*' -o -iname '*Core Audio Driver*' -o -iname '*ovsc*' \) 2>/dev/null |
        while read -r f; do
            echo "=== $f"
            sudo head -150 "$f"
        done
done
endgroup

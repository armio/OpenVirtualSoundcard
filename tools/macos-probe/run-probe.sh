#!/bin/bash
# Installs the probe (and the BlackHole canary, if built) on this Mac,
# restarts Core Audio, waits for the plug-in's report and prints everything
# needed to read the result from a CI log. Needs passwordless sudo.
set -uo pipefail
cd "$(dirname "$0")"

HAL=/Library/Audio/Plug-Ins/HAL
T0=$(date '+%Y-%m-%d %H:%M:%S')
LOGS=${LOGS:-logs}
mkdir -p "$LOGS"
group() { echo "::group::$1"; }
endgroup() { echo "::endgroup::"; }
# Some steps can hang if Core Audio misbehaves; bound them.
limit() { local secs=$1; shift; perl -e 'alarm shift; exec @ARGV' "$secs" "$@"; }

group "System"
sw_vers; uname -m; csrutil status
pgrep -lx coreaudiod || echo "coreaudiod not running"
ls -la "$HAL"
endgroup

group "Install the daemon"
sudo mkdir -p /usr/local/libexec
sudo install -o root -g wheel -m 755 build/ovprobe-daemon /usr/local/libexec/ovprobe-daemon
sudo install -o root -g wheel -m 644 build/org.openvirtualsoundcard.probe.plist /Library/LaunchDaemons/org.openvirtualsoundcard.probe.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/org.openvirtualsoundcard.probe.plist
sleep 2
sudo launchctl print system/org.openvirtualsoundcard.probe | head -60
endgroup

group "Install the drivers"
sudo rm -f /var/log/ovprobe-report.txt /tmp/ovprobe-plugin-report.txt
sudo cp -R build/OvscProbe.driver "$HAL/"
CANARY=0
if [ -d canary/out/BlackHole.driver ]; then
    sudo cp -R canary/out/BlackHole.driver "$HAL/"
    CANARY=1
fi
sudo chown -R root:wheel "$HAL"/*.driver
for d in "$HAL"/OvscProbe.driver "$HAL"/BlackHole.driver; do
    [ -d "$d" ] || continue
    codesign -dvvv "$d" 2>&1 | grep -E "^(Identifier|Format|Signature|CodeDirectory|TeamIdentifier|flags)" || true
    file "$d"/Contents/MacOS/*
done
sudo killall coreaudiod || true
sleep 3
limit 60 system_profiler SPAudioDataType > "$LOGS/system_profiler.txt" 2>&1 || echo "system_profiler failed or timed out"
endgroup

group "Wait for the plug-in report"
for i in $(seq 1 60); do
    [ -s /var/log/ovprobe-report.txt ] && break
    [ -s /tmp/ovprobe-plugin-report.txt ] && [ "$i" -gt 20 ] && break
    sleep 1
done
ps axo pid,user,ppid,comm | grep -iE "audio|ovprobe" | grep -v grep
endgroup

echo "::group::Plug-in report"
if [ -s /var/log/ovprobe-report.txt ]; then
    echo "(received by the daemon over XPC)"
    sudo cat /var/log/ovprobe-report.txt | tee "$LOGS/report.txt"
elif [ -s /tmp/ovprobe-plugin-report.txt ]; then
    echo "(written by the plug-in to /tmp; XPC report missing)"
    cat /tmp/ovprobe-plugin-report.txt | tee "$LOGS/report.txt"
else
    echo "NO REPORT: the plug-in did not run or could neither use XPC nor write /tmp"
fi
endgroup

group "Daemon log"
sudo cat /var/log/ovprobe-daemon.log | tee "$LOGS/daemon.log" | tail -200
endgroup

group "Audio devices"
grep -E "^\s{4}[^ ].*:$|Manufacturer|Transport" "$LOGS/system_profiler.txt" || cat "$LOGS/system_profiler.txt"
endgroup

show() {
    local name=$1 predicate=$2
    sudo log show --start "$T0" --style compact --info --debug --predicate "$predicate" \
        > "$LOGS/$name.log" 2>&1
    group "log: $name ($(wc -l < "$LOGS/$name.log") lines, last 150)"
    tail -150 "$LOGS/$name.log"
    endgroup
}
show probe 'eventMessage CONTAINS "ovprobe"'
show coreaudiod 'process == "coreaudiod" AND (eventMessage CONTAINS[c] "OvscProbe" OR eventMessage CONTAINS[c] "BlackHole" OR eventMessage CONTAINS[c] "plug-in" OR eventMessage CONTAINS[c] "plugin")'
show driver-service 'process CONTAINS[c] "Core-Audio-Driver" OR processImagePath CONTAINS[c] "Core-Audio-Driver"'
show sandbox '(sender == "Sandbox" OR eventMessage CONTAINS "Sandbox:") AND (eventMessage CONTAINS[c] "audio" OR eventMessage CONTAINS "ovprobe")'
show amfi 'process == "amfid" OR eventMessage CONTAINS "AppleMobileFileIntegrity" OR eventMessage CONTAINS "AMFI"'

echo "::group::Summary"
OS="$(sw_vers -productVersion) $(uname -m)"
if [ -s "$LOGS/report.txt" ]; then
    echo "probe plug-in loaded on $OS"
    grep -E "^(PASS|FAIL)" "$LOGS/report.txt"
elif grep -q "factory called" "$LOGS/probe.log"; then
    echo "probe plug-in factory ran on $OS but produced no report"
else
    echo "probe plug-in NOT loaded on $OS"
fi
if [ "$CANARY" = 1 ]; then
    if grep -qi "BlackHole" "$LOGS/system_profiler.txt"; then
        echo "canary: BlackHole (ad-hoc) enumerated"
    else
        echo "canary: BlackHole (ad-hoc) NOT enumerated"
    fi
fi
endgroup

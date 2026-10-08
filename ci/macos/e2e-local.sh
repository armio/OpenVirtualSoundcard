#!/bin/bash
# Runs the macOS end-to-end test (ci/macos/e2e.sh) on your own Mac instead of
# a GitHub runner, and saves its output and the diagnostics to one log file
# under target/e2e. See docs/MACOS.md, "Running the end-to-end test on your
# Mac".
#
# Usage: ci/macos/e2e-local.sh [--yes] [S1 S2 ...]    (default: every scenario)
#   --yes   do not ask before changing the system
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT" || exit 1
APP_SUPPORT="/Library/Application Support/OpenVirtualSoundcard"
WORK=$ROOT/target/e2e

YES=0
if [ "${1:-}" = --yes ]; then
    YES=1
    shift
fi

if [ "$(uname -s)" != Darwin ]; then
    echo "This test runs on macOS only."
    exit 1
fi
if ! xcode-select -p >/dev/null 2>&1; then
    echo "Install the Xcode command-line tools first: xcode-select --install"
    exit 1
fi
# rustup installed without touching the shell's profile leaves cargo off the
# PATH; its env file puts it back.
if ! command -v cargo >/dev/null && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null; then
    echo "Install Rust first: https://rustup.rs"
    exit 1
fi

cat <<EOF
This test changes your Mac while it runs (about 10 to 15 minutes, more the
first time while it builds):

  * it installs the OpenVirtualSoundcard driver, daemon and app with sudo, using a
    test configuration that talks only to this Mac (127.0.0.1);
  * it restarts Core Audio several times: all sound stops for a few seconds
    each time, so quit your DAW, music, video and calls first;
  * it runs a test PTP clock master on 127.0.0.1;
  * at the end it installs the .pkg it builds, then uninstalls OpenVirtualSoundcard
    completely, with its configuration, state and logs.

macOS may ask whether your terminal app may use the microphone: allow it,
since the test records from the OpenVirtualSoundcard device. If you missed the
question, allow it in System Settings > Privacy & Security > Microphone and
run the test again.
EOF
if [ -e "$APP_SUPPORT/ovsc.toml" ]; then
    echo
    echo "OpenVirtualSoundcard is already installed here: its configuration will be replaced,"
    echo "then removed. A copy goes to target/e2e/ovsc.toml.before."
fi
if [ "$YES" = 0 ]; then
    echo
    read -r -p "Go ahead? [y/N] " answer
    case $answer in
        y | Y | yes) ;;
        *) exit 1 ;;
    esac
fi

# e2e.sh calls sudo many times over several minutes, some of them in the
# background: ask for the password once, then keep it fresh.
sudo -v || exit 1
(while kill -0 $$ 2>/dev/null; do
    sudo -n true 2>/dev/null
    sleep 50
done) &

mkdir -p "$WORK"
if [ -e "$APP_SUPPORT/ovsc.toml" ]; then
    sudo cp "$APP_SUPPORT/ovsc.toml" "$WORK/ovsc.toml.before"
    sudo chown "$(id -u)" "$WORK/ovsc.toml.before"
fi

RUN_LOG=$WORK/run-$(date '+%Y%m%d-%H%M%S').log
ci/macos/e2e.sh "$@" 2>&1 | tee "$RUN_LOG"
rc=${PIPESTATUS[0]}
echo "Collecting diagnostics..."
ci/macos/diagnostics.sh >>"$RUN_LOG" 2>&1

echo
grep -E '^E2E-SUMMARY' "$RUN_LOG" | tail -1
echo "Full log, with diagnostics: $RUN_LOG"
if [ -e /Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver ]; then
    echo "OpenVirtualSoundcard is still installed (S11 did not run or did not finish). To remove it:"
    echo "  sudo \"$APP_SUPPORT/uninstall.sh\" --purge"
fi
exit "$rc"

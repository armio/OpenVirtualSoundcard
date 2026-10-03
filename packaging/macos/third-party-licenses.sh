#!/bin/bash
# Writes target/macos/THIRD-PARTY-LICENSES.html: the licences of the
# crates compiled into the macOS package (the daemon and the driver, then
# the app), which binary releases must ship. build-pkg.sh packages it.
#
# Needs cargo-about (cargo install cargo-about --locked --features cli).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/packaging/macos
cd "$ROOT"
OUT=${OUT:-$ROOT/target/macos}

command -v cargo-about >/dev/null ||
    { echo "third-party-licenses.sh: install cargo-about (cargo install cargo-about --locked --features cli)" >&2; exit 1; }

notices() {
    cargo about generate --locked --manifest-path "$1" --config "$HERE/about.toml" "$HERE/about.hbs"
}

mkdir -p "$OUT"
DAEMON=$(notices Cargo.toml)
APP=$(notices apps/macos/Cargo.toml)
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
cat >"$OUT/THIRD-PARTY-LICENSES.html" <<HTML
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>OpenVirtualSoundcard $VERSION: third-party licences</title>
<style>
body { font-family: -apple-system, sans-serif; max-width: 60em; margin: 2em auto; padding: 0 1em; }
pre { white-space: pre-wrap; background: #f4f4f4; padding: 1em; font-size: 0.85em; }
.used-by { font-size: 0.9em; }
</style>
</head>
<body>
<h1>OpenVirtualSoundcard $VERSION: third-party licences</h1>
<p>OpenVirtualSoundcard is licensed under the GNU General Public License,
version 3 or later. It includes the following open-source software, under
the licences below.</p>
<h2>The daemon and the Core Audio driver</h2>
$DAEMON
<h2>The OpenVirtualSoundcard app</h2>
$APP
</body>
</html>
HTML
echo "wrote $OUT/THIRD-PARTY-LICENSES.html"

#!/bin/bash
# Writes target/macos/THIRD-PARTY-LICENSES.html: the licences of the
# crates compiled into the macOS package (the daemon and the driver, then
# the app) and of the Rust standard library linked into all three, which
# binary releases must ship. build-pkg.sh packages it.
#
# Needs cargo-about (cargo install cargo-about --locked --features cli) and
# the Rust toolchain that builds the package: the standard library's
# notices come from it.
#
# Environment:
#   OUT  output directory (default: target/macos)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/packaging/macos
cd "$ROOT"
OUT=${OUT:-$ROOT/target/macos}

command -v cargo-about >/dev/null ||
    { echo "third-party-licenses.sh: install cargo-about (cargo install cargo-about --locked --features cli)" >&2; exit 1; }
STD_NOTICES=$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html
[ -f "$STD_NOTICES" ] ||
    { echo "third-party-licenses.sh: no $STD_NOTICES (it comes with rustc)" >&2; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cat "$HERE/about.toml" "$HERE/about-app.toml" >"$TMP/about-app.toml"

# The notices of the workspace at MANIFEST, with cargo-about's settings
# CONFIG. --fail: a crate whose licence cannot be worked out fails the
# build instead of being left out. Each part of the page gets its own
# anchor prefix, PREFIX: licence names repeat across the parts.
notices() {
    cargo about generate --fail --locked --manifest-path "$1" --config "$2" "$HERE/about.hbs" |
        sed -e "s/ id=\"/ id=\"$3-/" -e "s/ href=\"#/ href=\"#$3-/"
}

mkdir -p "$OUT"
DAEMON=$(notices Cargo.toml "$HERE/about.toml" daemon)
APP=$(notices apps/macos/Cargo.toml "$TMP/about-app.toml" app)
# The body of the toolchain's notices, a level down under this page's
# heading for them, with their anchors prefixed like the others.
STD=$(sed -e '1,/<body>/d' -e '/<\/body>/,$d' -e '/<h1>/d' "$STD_NOTICES" |
    sed -e 's/<h3/<h4/g' -e 's/<\/h3>/<\/h4>/g' -e 's/<h2/<h3/g' -e 's/<\/h2>/<\/h3>/g' \
        -e 's/ id="/ id="rust-std-/g' -e 's/ href="#/ href="#rust-std-/g')
[ -n "$STD" ] || { echo "third-party-licenses.sh: no notices in $STD_NOTICES" >&2; exit 1; }
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
RUST=$(rustc --version)
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
<ul>
<li><a href="#daemon">The daemon and the Core Audio driver</a></li>
<li><a href="#app">The OpenVirtualSoundcard app</a></li>
<li><a href="#rust-std">The Rust standard library</a></li>
</ul>
<h2 id="daemon">The daemon and the Core Audio driver</h2>
$DAEMON
<h2 id="app">The OpenVirtualSoundcard app</h2>
$APP
<h2 id="rust-std">The Rust standard library</h2>
<p>The daemon, the driver and the app all include the Rust standard library
($RUST), under these notices.</p>
$STD
</body>
</html>
HTML
echo "wrote $OUT/THIRD-PARTY-LICENSES.html"

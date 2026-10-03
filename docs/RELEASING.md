# Releasing

A release is a tag. The [Release workflow](../.github/workflows/release.yml)
builds the macOS installer package for it, universal (Apple silicon and
Intel), and attaches it to a draft GitHub release.

## Making a release

1. Set the version in `Cargo.toml` (`[workspace.package] version`) and in
   `apps/macos/Cargo.toml`, run `cargo build` and `(cd apps/macos && cargo
   build)` to update both lockfiles, and move the `[Unreleased]` notes in
   `CHANGELOG.md` under the new version.
2. Merge that to `main`, then tag it:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

   The workflow refuses a tag that differs from the version in either
   `Cargo.toml`.
3. When it finishes, review the draft release under Releases: its notes
   and `OpenVirtualSoundcard-<version>.pkg`. Install the package on a Mac
   (see [MACOS.md](MACOS.md#installer-package)), then publish the release.

Started by hand (Actions, Release, Run workflow), the workflow builds the
package as an artifact of the run, without a release.

## Signing and notarization

Without signing secrets the package is unsigned: it installs and works,
but macOS warns that Apple could not verify it is free of malware, and
users have to allow it in System Settings
([MACOS.md](MACOS.md#installer-package)). To sign and notarize it, the project needs an
Apple Developer Program membership and these repository secrets (Settings,
Secrets and variables, Actions):

| Secret | What |
|---|---|
| `MACOS_SIGNING_CERTIFICATES` | A `.p12` export, base64-encoded, holding both the **Developer ID Application** and the **Developer ID Installer** certificates with their private keys (Keychain Access: select both, Export Items; then `base64 -i certificates.p12 \| pbcopy`). |
| `MACOS_SIGNING_PASSWORD` | The password of that `.p12`. |
| `MACOS_CODESIGN_IDENTITY` | The Application identity's name, e.g. `Developer ID Application: Jane Doe (TEAM123456)`. |
| `MACOS_INSTALLER_IDENTITY` | The Installer identity's name, e.g. `Developer ID Installer: Jane Doe (TEAM123456)`. |
| `APPLE_ID` | The Apple ID of the developer account, for notarization. |
| `APPLE_TEAM_ID` | Its team ID. |
| `APPLE_APP_PASSWORD` | An app-specific password for that Apple ID (appleid.apple.com, Sign-In and Security). |

With them, `build-pkg.sh` signs the driver, the daemon and the app with
the hardened runtime and a secure timestamp, signs the package, submits it
to Apple's notary service and staples the ticket.

## What the package contains

The driver, the daemon and its launchd job, the log rotation rule, the
uninstaller, the app, and `THIRD-PARTY-LICENSES.html`: the licences of every
crate compiled into them, written by `packaging/macos/third-party-licenses.sh`
with [cargo-about](https://github.com/EmbarkStudios/cargo-about). Binary
releases must carry those notices; the accepted licences are the ones
`deny.toml` allows.

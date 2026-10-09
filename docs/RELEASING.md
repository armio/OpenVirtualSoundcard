# Releasing

A release is a tag. The [Release workflow](../.github/workflows/release.yml)
builds the macOS installer package for it, universal (Apple silicon and
Intel), and attaches it to a draft GitHub release.

## Making a release

1. Set the new version in three places, run `cargo build` and `(cd
   apps/macos && cargo build)` to update both lockfiles, and move the
   `[Unreleased]` notes in `CHANGELOG.md` under the new version:
   * `Cargo.toml`, `[workspace.package] version`;
   * `Cargo.toml`, the `version` of each `ovsc-*` crate in
     `[workspace.dependencies]` (Cargo refuses a crate whose version does
     not match it);
   * `apps/macos/Cargo.toml`, `[package] version`.
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

Re-running the workflow for a tag (GitHub allows it for 30 days) replaces
the package of its draft release, but never touches a published one: the
run fails instead. Started
by hand (Actions, Release, Run workflow), from a branch or a tag, the
workflow only builds the package, as an artifact of the run.

The workflow has three jobs, so that the third-party code a build runs
(crates' build scripts and procedural macros, cargo-about) never runs where
the signing keys or a token that can write to the repository are:

| Job | Does | Has |
|---|---|---|
| `build` | Builds the driver, the daemon, the app and the licence notices, signed ad hoc (`BUILD=1 UNIVERSAL=1 build-pkg.sh`) | No secrets; a read-only token |
| `package` | Signs and packages what `build` made (`build-pkg.sh` with `DRIVER`, `DAEMON`, `APP` and `NOTICES`), and runs no cargo | The signing secrets, when set |
| `release` | Attaches the package to a draft release, on a tag push only | A token that can write releases |

## Signing and notarization

Without signing secrets the package is unsigned: it installs and works,
but macOS warns that Apple could not verify it is free of malware, and
users have to allow it in System Settings
([MACOS.md](MACOS.md#installer-package)). To sign and notarize it, the
project needs an Apple Developer Program membership and these repository
secrets (Settings, Secrets and variables, Actions):

| Secret | What |
|---|---|
| `MACOS_SIGNING_CERTIFICATES` | A `.p12` export, base64-encoded, holding both the **Developer ID Application** and the **Developer ID Installer** certificates with their private keys (Keychain Access: select both, Export Items; then `base64 -i certificates.p12 \| pbcopy`). |
| `MACOS_SIGNING_PASSWORD` | The password of that `.p12`. |
| `MACOS_CODESIGN_IDENTITY` | The Application identity's name, e.g. `Developer ID Application: Jane Doe (TEAM123456)`. |
| `MACOS_INSTALLER_IDENTITY` | The Installer identity's name, e.g. `Developer ID Installer: Jane Doe (TEAM123456)`. |
| `APPLE_NOTARY_KEY` | An App Store Connect API key for the notary service: the contents of its `.p8` file (App Store Connect, Users and Access, Integrations, Team Keys; the Developer role is enough). An API key reaches only App Store Connect, unlike an app-specific password, which also opens the Apple Account's iCloud data, and it can be revoked on its own. |
| `APPLE_NOTARY_KEY_ID` | That key's ID. |
| `APPLE_NOTARY_ISSUER` | The issuer ID shown above the list of keys. |

With them, the `package` job signs the driver, the daemon and the app with
the hardened runtime and a secure timestamp, signs the package, submits it
to Apple's notary service and staples the ticket. Its keychain is deleted
when the job ends.

## What the package contains

The driver, the daemon and its launchd job, the log rotation rule, the
uninstaller, the app, and `THIRD-PARTY-LICENSES.html`: the licences of the
crates compiled into them, written by
`packaging/macos/third-party-licenses.sh` with
[cargo-about](https://github.com/EmbarkStudios/cargo-about), and the Rust
standard library's notices from the toolchain that built them. Binary
releases must carry those notices. The accepted licences are the ones
`deny.toml` allows; `packaging/macos/about-app.toml` adds the notices that
the app's font and `dpi` crates leave out of their licence fields.

# Contributing to OpenVirtualSoundcard

Thanks for helping! The most valuable contribution right now is **testing
against real Dante equipment** and sending packet captures.

## Building and testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

On Linux the sound-card bridge needs ALSA headers (`libasound2-dev`,
`alsa-lib-devel` or `alsa-lib`).

## Reporting interoperability results

Please include:

* the OpenVirtualSoundcard commit, OS, and how you ran it (`ovsc run …`, config);
* the Dante devices involved (model, firmware) and the Dante Controller version;
* a Wireshark capture filtered to
  `udp.port in {4440 4455 5353 8700 8702 8708 8800 319 320}`, covering the
  action that failed;
* logs from `ovsc run --log debug`.

Captures from real devices are turned into test vectors (see
`crates/ovsc-proto/src/arc.rs`). Only share captures from networks you
are allowed to share, and strip anything sensitive.

## Clean-room rules

OpenVirtualSoundcard interoperates with an undocumented, proprietary protocol. To stay on
the right side of copyright and licence terms:

* Don't paste code from projects with incompatible licences. Describe formats
  in your own words. `docs/PROTOCOL.md` is the reference.
* Never use or share Audinate's SDKs, firmware or confidential documents, and
  don't disassemble Audinate software. Many of their licences forbid it.
  Observing network traffic between devices you own is fine.
* Don't add anything that circumvents access control (e.g. Dante Domain
  Manager).

## Code style

* Protocol knowledge goes in `ovsc-proto` as pure functions with tests.
  Every unknown field is named `unknown…` with the observed value documented.
* Real-time paths (transmit thread, ring access, audio callbacks) must not
  allocate, lock or log.
* Comments explain *why*. Cite the source of protocol facts (capture, project,
  section of `docs/PROTOCOL.md`).

By contributing you agree that your contributions are licensed under the
GNU GPL v3 or later.

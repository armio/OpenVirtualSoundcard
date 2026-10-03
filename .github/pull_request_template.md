## What and why

<!-- What this changes, and why. Link the issue it closes, if any. -->

## How it was tested

<!-- The checks you ran (cargo test, clippy, the macOS end-to-end test),
     and anything tested on real Dante equipment: devices and firmware. -->

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] Protocol changes are described in `docs/PROTOCOL.md`, with their source
- [ ] The [clean-room rules](../CONTRIBUTING.md#clean-room-rules) are respected: no code or documents from incompatible or confidential sources
- [ ] Packet captures added as test vectors contain nothing private

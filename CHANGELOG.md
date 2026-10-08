# Changelog

All notable changes to OpenVirtualSoundcard are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

The first public version, not released yet.

### Added

- A Dante-compatible device engine in Rust: mDNS discovery, the ARC, CMC,
  conmon and flow-control protocols, unicast audio flows both ways, and an
  in-process PTPv1 clock follower with kernel receive timestamps on macOS.
- Backends: a sound-card bridge with drift-compensating resampling, WAV
  recording, test tones and loopback.
- The `ovsc` command: `run`, `discover`, `route`, `unroute`, `info`,
  `soundcards`, `ptp-master` and `example-config`.
- macOS: a native Core Audio device, **Open Virtual Soundcard**, clocked by
  the Dante network without resampling. A Core Audio driver and a
  LaunchDaemon share memory over XPC. Installed with `install.sh` or a
  package, removed with `uninstall.sh`.
- macOS: the OpenVirtualSoundcard app, for status and settings over the
  daemon's control socket.
- macOS: an installer package for Apple silicon and Intel Macs, with the
  app and the third-party licence notices, built by the release workflow
  for each tag (signed and notarized once the signing secrets are set).
- Settings from Dante Controller: device and channel names, subscriptions,
  latency, sample rate and encoding.
- Monitoring in Dante Controller: clock state, signal meters, receive
  latency and late packets, and interface traffic.
- Documentation: an independent protocol reference (`docs/PROTOCOL.md`),
  the architecture, the macOS device, and the roadmap.

[Unreleased]: https://github.com/armio/OpenVirtualSoundcard/commits/main

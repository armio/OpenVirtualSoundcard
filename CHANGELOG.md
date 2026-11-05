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
- Settings from Dante Controller: device and channel names, subscriptions,
  latency, sample rate and encoding.
- Monitoring in Dante Controller: clock state, signal meters, receive
  latency and late packets, and interface traffic.
- Documentation: an independent protocol reference (`docs/PROTOCOL.md`),
  the architecture, the macOS device, and the roadmap.
- A guide to live use (`docs/LIVE.md`): the latency budget, recommended
  settings, the network, a pre-show check, and what happens when something
  fails.
- `[device] tx_latency_ms`: the latency the transmit channels ask receivers
  for, separate from the receive latency. Unset, it is the configured
  `latency_ms`, as before. The device reports it to Core Audio as its output
  latency.
- The show lock (`[device] lock_remote_settings`, or the app): controllers
  cannot change the latency, sample rate, encoding or names while it is on.
  The app lists the changes controllers made or tried.
- The device probes its name at start and takes `<name>-2` if another device
  has it; a device using its name later is logged and reported. The default
  name comes from the interface's MAC address and survives an address change.
- The daemon follows its network interface and restarts the network engine
  when the interface returns or its address changes; it restarts itself
  after any panic; non-macOS backends retry until the network is up and
  reopen a sound card that stopped.
- Audio packets are marked DSCP EF and PTP packets CS7.
- Counters for receive (lost, early and rejected packets, per-flow health),
  transmit (send errors, lateness, skipped packets, resyncs, expired flows,
  real-time scheduling) and the clock (unlocks, steps, leader changes,
  holdover), logged as problems happen.
- Health in the app and the log: the Audio light is green **Flowing** only
  when nothing audible went wrong in the last 30 s, amber **Dropouts** when
  something did, and red **Silent** when the driver has failed (with the
  command that recovers it). Warnings in plain words cover what rose in the
  last 30 s, with the urgent ones first; they are logged as `health:`
  lines, and the status line says `healthy` or lists them.
  Controllers' changes move to an **Earlier** list after 10 minutes.

### Changed

- A new receive latency, from the app or Dante Controller (ARC 0x1101),
  restarts only the receive side instead of the whole network engine: the
  receive flows are requested again at the new latency, the backends read
  the inputs at it, and the Core Audio driver gets its new input safety
  offset while the engine runs on (one configuration change in Core Audio).
  The transmit flows, such as the returns to a console, keep their handles
  and keep sending, and the mDNS records and the clock carry on. The control
  socket answers `"restart":"receive"` for it, and the app says what it
  does. The transmit latency no longer follows a receive latency set from
  the app or a controller: unset, it stays the configured `latency_ms`.
- A Dante leader change, a lost leader (30 s of holdover) and a clock that
  unlocks under congestion no longer interrupt audio when the new leader
  agrees with the running clock. A leader whose time differs by more than
  250 µs is stepped to once.
- Receive flows play at the larger of the device's latency and the latency
  the transmitter asks for, as Dante receivers do.
- Received packets give their frame count by their length (up to 256), and
  are accepted only from the transmitter's addresses and the port its audio
  comes from. A computer on Wi-Fi and Ethernet in the Dante subnet may send
  from either of its addresses.
- Receive recovery never waits on the network: a transmitter that does not
  answer no longer delays the others, and failures show as Dante statuses
  (`NO_CONNECTION`, `RX_FAIL`, `CHANNEL_FORMAT`, `NO_RX`, `CHANNEL_LATENCY`).
- Latencies below 1 ms are refused from the app and controllers, and raised
  to the minimum, with a warning, in the configuration: packets leave 0.5 ms
  after their time.
- A device restarted in-process (an address change), or by a new daemon on
  the same Mac (after a crash, at a reboot), keeps the name it announced
  instead of probing it again, so a name conflict can no longer rename it
  mid-show. The state file keeps the name with the interface's MAC address;
  a copy on another Mac (a cloned disk) probes as before. The daemon offers
  Core Audio the saved latency and names at start, so its restart does not
  reconfigure the device twice.
- While the show lock is on, controllers see the sample rate and encoding as
  fixed.
- Transmit flows accept 2 to 32 frames per packet and 1 to 8 channels, as
  advertised. A flow requested again with another packet size is replaced.
- The macOS driver fades over 5 ms when its gate opens or closes, and sends
  silence for NaN or infinite output.
- The state file is written durably with a backup, and a damaged one is kept
  aside rather than overwritten. The idle-sleep assertion is held from
  daemon start to shutdown. The daemon log keeps rotating.
- The sound-card bridge runs its Linux callbacks with real-time priority and
  recovers from device gaps with short fades instead of long mutes.

### Fixed

- The transmit thread could panic on a malformed flow request, replayed up
  to 100 ms of stale packets after a late wake-up before the one a receiver
  could still play (now at most 40 ms, then it resyncs), and logged on its
  real-time thread.
- An expired transmit flow could remove a flow that had just been requested
  again.
- Receive threads took a lock per packet.
- Stale audio could replay from a ring 2^32 samples (about 25 h at 48 kHz)
  after it was written.
- Two devices with the default name got each other's subscriptions, and a
  self-subscription could loop back when another device used the name.
- A channel added to an existing receive flow stayed silent after the media
  clock stepped back, for as long as the step.
- The transmit thread allocated, freed sockets and woke a task when flows
  came and went.
- Settings changed in the app while the engine restarted were lost when it
  came back.
- The app's Apply stayed disabled while the engine waited for the network
  with an empty device name.
- The mDNS cache kept only the last address of a host that announces one per
  interface, such as a computer on Wi-Fi and Ethernet: a record with the
  cache-flush bit removed the others at once, even those just received. It
  now follows RFC 6762 §10.2.
- A computer on Wi-Fi and Ethernet was taken for two transmitters when
  lookups gave its addresses in another order: a channel found at its other
  address got a new flow instead of a free slot in the existing one,
  spending one more of the computer's flows. Its channels now share flows,
  and requests to it go one at a time.

[Unreleased]: https://github.com/armio/OpenVirtualSoundcard/commits/main

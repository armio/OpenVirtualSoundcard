# OpenVirtualSoundcard

[![CI](https://github.com/armio/OpenVirtualSoundcard/actions/workflows/ci.yml/badge.svg)](https://github.com/armio/OpenVirtualSoundcard/actions/workflows/ci.yml)
[![License: GPL v3+](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)

An open-source virtual soundcard that speaks the Dante® audio-over-IP
protocol family. Written in Rust. Licensed under the GPL v3 or later.
Unofficial: not affiliated with Audinate ([Legal](#legal)).

> ⚠️ **Experimental.** Every protocol format was cross-checked against
> published captures from real Dante devices, and OpenVirtualSoundcard instances
> interoperate with each other. On macOS it has run on one real Dante
> network: Dante Controller found it, routed a Dante AVIO's channels to it
> and showed its clock locked to the AVIO. Wider testing has **not been done
> yet**. Please help: see [CONTRIBUTING.md](CONTRIBUTING.md). Don't use it on
> a show-critical network.

## What it does

OpenVirtualSoundcard runs as a device on a Dante network:

* advertises itself and its channels over mDNS, so Dante Controller and other
  devices can see it;
* answers the control protocols (ARC, CMC, conmon), so controllers can list
  channels, route subscriptions and rename things;
* **sends** audio: receivers request unicast flows from it;
* **receives** audio: subscribe its inputs to any transmitter, from a
  controller or from the command line;
* follows the network's **PTPv1** clock in-process, so it needs no extra
  daemon.

macOS is the platform in development and testing for now. The engine also
runs on Linux, where the unit tests run; Windows is not tested.

Audio reaches your applications through a *backend*:

| Backend | Use |
|---|---|
| `soundcard` | Bridge to a sound card or virtual cable (BlackHole, VB-Cable, ALSA loopback) with drift-compensating resampling |
| `record` | Record all receive channels to a WAV file |
| `tone` | Test tones on every transmit channel |
| `loopback` | Echo receive channel *n* to transmit channel *n* |

On macOS, OpenVirtualSoundcard also installs as a native Core Audio device that any
application can select, running on the network clock without resampling
([macOS quick start](#macos-quick-start)). Native drivers for Windows
(WASAPI/ASIO) and Linux (PipeWire) are on the [roadmap](docs/ROADMAP.md).

## Quick start

```sh
cargo build --release
./target/release/ovsc example-config > ovsc.toml
```

Edit `ovsc.toml`, then:

```sh
sudo ./target/release/ovsc run -c ovsc.toml
```

`sudo` is needed on Linux because PTP uses ports 319/320 (alternatively grant
`CAP_NET_BIND_SERVICE` and `CAP_SYS_NICE`). Quit Dante Virtual Soundcard
first: both use the same ports.

On macOS, use the native device ([below](#macos-quick-start)). Elsewhere, to
use it from a DAW, bridge it to a virtual audio device (VB-Cable on Windows,
`snd-aloop` on Linux, or BlackHole on macOS): set `backend = "soundcard"` and
the device names in the config. `ovsc soundcards` lists devices, and the
[`ovsc-soundcard` docs](crates/ovsc-soundcard/src/lib.rs) explain
each platform's setup.

Without a config file, flags work too:

```sh
ovsc run --name studio-mac --tx 8 --rx 8 --backend record --record-path take1.wav
```

Route audio with Dante Controller, or from the command line:

| Command | Does |
|---|---|
| `ovsc discover --channels` | lists the devices and channels on the network |
| `ovsc route studio-mac 1 01@stagebox` | feeds receive channel 1 of studio-mac from channel 01 of stagebox |
| `ovsc info studio-mac` | shows channels, subscriptions and status |
| `ovsc unroute studio-mac 1` | clears receive channel 1 |
| `ovsc soundcards` | lists the audio devices for the soundcard backend |

### macOS quick start

The Core Audio device needs the Xcode command-line tools. From the repository
root:

```sh
cargo build --release --locked -p ovsc
packaging/macos/build-driver.sh
packaging/macos/build-app.sh
sudo packaging/macos/install.sh
```

`build-driver.sh` builds `target/macos/OpenVirtualSoundcard.driver` and `build-app.sh`
the OpenVirtualSoundcard app. To install with a configuration of your own, add
`--config my.toml` to the last line.

This installs the driver, the daemon (a LaunchDaemon) and
`/Applications/OpenVirtualSoundcard.app`, restarts Core Audio and waits until the
driver has connected to the daemon. Applications then see a device called
**Open Virtual Soundcard**. The app shows the clock, the driver and the subscriptions,
and changes the device name, the network interface, the sample rate, the
channel counts and the latency; Dante Controller sets the name and the
latency too. The configuration is
`/Library/Application Support/OpenVirtualSoundcard/ovsc.toml`; set `interface` to
the Dante network interface unless it carries the default route. To edit
the file yourself, use `sudo`, then restart the daemon:

```sh
sudo launchctl kickstart -k system/org.openvirtualsoundcard.daemon
```

To uninstall (add `--purge` to also remove the configuration, the remembered
state and the logs):

```sh
sudo "/Library/Application Support/OpenVirtualSoundcard/uninstall.sh"
```

[`docs/MACOS.md`](docs/MACOS.md) covers the app, latency, status,
troubleshooting and how the driver works.

### Trying it without Dante hardware

Run `ovsc ptp-master` on a network with no Dante devices to give
OpenVirtualSoundcard instances a PTP clock, or use the system clock for two instances on
one machine:

```sh
ovsc run --name tx-dev --clock system --backend tone
# second terminal: a config with [device.ports] arc = 4441, cmc = 8801,
# flow_control = 4456, settings = 8701 and process_id = 1
ovsc run -c rx.toml --clock system --backend record
ovsc route rx-dev 1 01@tx-dev
```

## How it works

```
 Dante network ──► mDNS · ARC · CMC · conmon ─► control plane ─► subscriptions
       ▲   │                                                        │
       │   └──── media flows ─► RX tasks ─► rx rings ─┐             ▼
       │                                              ├─► backend / driver
       └──── TX thread (real-time) ◄─ tx rings ◄──────┘
                     ▲
              PTPv1 media clock
```

* [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): crates, threads, time model, data path.
* [`docs/PROTOCOL.md`](docs/PROTOCOL.md): byte-level protocol reference, sourced, with confidence markers and open questions.
* [`docs/MACOS.md`](docs/MACOS.md): the macOS Core Audio device, for users and developers.
* [`docs/ROADMAP.md`](docs/ROADMAP.md): what's next.

| Crate | Purpose |
|---|---|
| [`ovsc-proto`](crates/ovsc-proto) | Pure codecs for every wire format |
| [`ovsc-clock`](crates/ovsc-clock) | Media clock and in-process PTPv1 follower |
| [`ovsc-core`](crates/ovsc-core) | The device engine |
| [`ovsc-soundcard`](crates/ovsc-soundcard) | Sound-card bridge with drift compensation |
| [`ovsc-shm`](crates/ovsc-shm) | Shared-memory layout between the daemon and the macOS driver |
| [`ovsc-ipc`](crates/ovsc-ipc) | Daemon–driver protocol and XPC transport |
| [`ovsc-hal`](crates/ovsc-hal) | The macOS Core Audio driver |
| [`ovsc-hal-server`](crates/ovsc-hal-server) | The daemon's side of the macOS driver |
| [`ovsc`](crates/ovsc) | The `ovsc` command |

## Legal

**Dante is a registered trademark of Audinate Pty Ltd.** OpenVirtualSoundcard is an
independent project. It is not affiliated with, endorsed by, sponsored by or
approved by Audinate. "Dante" is used here only to describe what OpenVirtualSoundcard is
compatible with.

The implementation is based on public reverse-engineering work and on
observing network traffic. No Audinate software, SDK, firmware or
confidential documentation was used. Audinate holds patents on technology
used by Dante. Consult a lawyer before using OpenVirtualSoundcard commercially or
distributing binaries where software patents apply.

Please don't use OpenVirtualSoundcard to make devices that pretend to be Audinate
products. Always describe it as an unofficial implementation.

## Credits

OpenVirtualSoundcard stands on the shoulders of:

* [Inferno](https://github.com/teodly/inferno) by Teodor Woźniak: the first
  open-source Dante-compatible device, and the source of much of our protocol
  knowledge;
* [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller)
  ("netaudio") by Chris Ritsen and contributors: the controller side, and real
  device captures we use as test vectors;
* [Statime](https://github.com/pendulum-project/statime) and its PTPv1 fork.

## Contributing

Reports from real Dante networks help most: see
[CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test and send
interoperability results, and for the clean-room rules every contribution
follows. Changes are listed in [CHANGELOG.md](CHANGELOG.md).

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
Report security problems privately, as [SECURITY.md](SECURITY.md) explains.

## License

Copyright © 2026 the OpenVirtualSoundcard contributors.

OpenVirtualSoundcard is free software: you can redistribute it and/or
modify it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or (at your
option) any later version. It is distributed in the hope that it will be
useful, but without any warranty; see the [license](LICENSE) for details.

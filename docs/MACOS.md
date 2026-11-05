# OpenVirtualSoundcard on macOS

On macOS, OpenVirtualSoundcard installs a native Core Audio device called **Open
Virtual Soundcard**.
Any application can select it like a sound card. Its inputs are the Dante
receive channels and its outputs the Dante transmit channels. Core Audio runs
the device on the Dante network's PTP clock, so nothing is resampled.

Two pieces make the device:

* `OpenVirtualSoundcard.driver`, a Core Audio server plug-in (AudioServerPlugIn). Core
  Audio runs it in a helper process. It only copies samples and reports time.
* The `ovsc` daemon, a LaunchDaemon. It does all the networking: PTP,
  discovery, control and audio flows.

They talk over the XPC Mach service `org.openvirtualsoundcard.audio` and share one
memory region that holds the clock and the audio rings.

> ⚠️ **Experimental.** It has run on one real Dante network: an M1 Mac with
> macOS 26.6 and SIP on, a Dante AVIO-DAI2 as clock leader, and Dante
> Controller on the same Mac, which discovered the device, routed the
> AVIO's channels to it and showed its clock locked. The end-to-end tests run on a Mac
> with OpenVirtualSoundcard talking to itself ([Testing](#testing)).

Contents:

* For users: [Installing](#installing), [What gets installed](#what-gets-installed),
  [Configuration](#configuration), [The OpenVirtualSoundcard app](#the-openvirtualsoundcard-app),
  [What applications see](#what-applications-see),
  [Clock states and silence](#clock-states-and-silence), [Status](#status),
  [Uninstalling](#uninstalling), [Troubleshooting](#troubleshooting).
* For developers: [Architecture](#architecture), [Shared region](#shared-region),
  [Device timeline](#device-timeline), [Retire protocol](#retire-protocol),
  [Testing](#testing) (and [running it on your
  Mac](#running-the-end-to-end-test-on-your-mac)), [Measured IO
  timing](#measured-io-timing).

## Installing

You need macOS 11 or later, Rust and the Xcode command-line tools. From the
repository root:

```sh
cargo build --release --locked -p ovsc
packaging/macos/build-driver.sh
packaging/macos/build-app.sh
sudo packaging/macos/install.sh
```

`build-driver.sh` builds `target/macos/OpenVirtualSoundcard.driver` and signs it ad hoc
(or with `CODESIGN_IDENTITY`). `build-app.sh` builds the [OpenVirtualSoundcard
app](#the-openvirtualsoundcard-app), `target/macos/OpenVirtualSoundcard.app`; it is optional, and
`install.sh` installs it only if it is there. `install.sh` takes these
options:

| Option | Effect |
|---|---|
| `--config FILE` | Install `FILE` as the configuration. Without it, an existing configuration is kept, or the default one is written with the device named after this Mac (`scutil --get LocalHostName`, cut to a valid Dante name). |
| `--driver PATH` | The driver bundle to install (default `target/macos/OpenVirtualSoundcard.driver`). |
| `--daemon PATH` | The `ovsc` binary to install (default `target/release/ovsc`). |
| `--app PATH` | The app bundle to install as `/Applications/OpenVirtualSoundcard.app` (default `target/macos/OpenVirtualSoundcard.app`, if it was built). |
| `--no-app` | Leave the installed app alone. |
| `--no-driver` | Leave the installed driver, and Core Audio, alone. |
| `--no-coreaudio-restart` | Install the driver without restarting Core Audio; it loads at the next restart of `coreaudiod`. |

`install.sh` is idempotent: running it again upgrades an install. It:

1. stops the running daemon (`launchctl bootout system/org.openvirtualsoundcard.daemon`);
2. copies the driver, the daemon, the launchd job, the log rotation rule,
   the uninstaller and the app into place, owned by `root:wheel`, and writes
   the configuration;
3. removes the `com.apple.quarantine` attribute from the launchd job, the
   driver and `/Library/Application Support/OpenVirtualSoundcard` (launchd refuses
   quarantined job files);
4. starts the daemon (`launchctl bootstrap system /Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist`);
5. lets the daemon through the application firewall, if the firewall is on
   ([Firewall](#firewall));
6. restarts Core Audio with `killall coreaudiod`, and `killall -9 coreaudiod`
   if the same process still runs 5 s later. It never uses
   `launchctl kickstart` on `coreaudiod`, which macOS 14.4 and later refuse;
7. waits up to 30 s for Core Audio to start the driver's helper process and
   for the daemon to log `hal: plug-in attached` for it. Otherwise it prints
   diagnostics and exits 1.

### Upgrading from OpenDante

OpenVirtualSoundcard was called OpenDante while it was developed. Installing
it, with `install.sh` or the package, replaces an OpenDante install: it stops
the `org.opendante.daemon` job, moves `opendante.toml` and `state.toml` from
`/Library/Application Support/OpenDante` to the new folder (rewriting the old
paths and service names in them), and removes the old daemon, driver, app,
command link and log rotation rule. The old logs stay in
`/Library/Logs/OpenDante`. The new driver replaces the old one at the same
time, so `install.sh` refuses `--no-driver` then.

### Installer package

`packaging/macos/build-pkg.sh` builds `dist/OpenVirtualSoundcard-<version>-<arch>.pkg`
(`dist/OpenVirtualSoundcard-<version>.pkg` when the driver and the daemon are universal
binaries) from the same files; `BUILD=1` builds both first, and
`BUILD=1 UNIVERSAL=1` builds them for arm64 and x86_64 (first
`rustup target add aarch64-apple-darwin x86_64-apple-darwin`). Its
postinstall script runs the same steps from the same `install-lib.sh`.
`build-pkg.sh` names the package on its last line (`built <path> ...`), and
older packages stay in `dist`. The package does not include the OpenVirtualSoundcard
app yet: build and install it with `build-app.sh` and `install.sh`. Install that one, for example:

```sh
sudo installer -pkg dist/OpenVirtualSoundcard-<version>-arm64.pkg -target /
```

Do not hand out an unsigned package for download: Gatekeeper blocks it, and
macOS 15 dropped the Control-click way around that. `build-pkg.sh` signs with
a Developer ID when `CODESIGN_IDENTITY` and `INSTALLER_IDENTITY` are set, and
notarizes with `NOTARY_PROFILE`.

## What gets installed

| Path | What |
|---|---|
| `/Library/Audio/Plug-Ins/HAL/OpenVirtualSoundcard.driver` | The Core Audio driver. |
| `/Library/Application Support/OpenVirtualSoundcard/bin/ovsc` | The daemon. |
| `/usr/local/bin/ovsc` | A link to the daemon, unless a file that is not a link is already there. |
| `/Library/Application Support/OpenVirtualSoundcard/ovsc.toml` | The configuration. |
| `/Library/Application Support/OpenVirtualSoundcard/state.toml` | Renames, subscriptions and the latency set from a controller or the app (`state_file` in the default configuration). |
| `/Applications/OpenVirtualSoundcard.app` | The [OpenVirtualSoundcard app](#the-openvirtualsoundcard-app), if it was built. |
| `/var/run/ovsc/control.sock` | The daemon's [control socket](#the-control-socket), while it runs. |
| `/Library/Application Support/OpenVirtualSoundcard/uninstall.sh` | The uninstaller. |
| `/Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist` | The launchd job `org.openvirtualsoundcard.daemon`. |
| `/Library/Logs/OpenVirtualSoundcard/ovsc.log` | The daemon's log. |
| `/etc/newsyslog.d/org.openvirtualsoundcard.conf` | Rotates the log at 1 MiB, keeping 5 old files. The daemon notices within 5 s and writes to the new `ovsc.log`. |

The launchd job runs `ovsc run --config "/Library/Application Support/OpenVirtualSoundcard/ovsc.toml"
--log-file /Library/Logs/OpenVirtualSoundcard/ovsc.log` as root, at boot and
again whenever it exits (`RunAtLoad`, `KeepAlive`, `ThrottleInterval` 5 s):
it exits on purpose, with an error, when a part of it failed ([The daemon
restarted by itself](#the-daemon-restarted-by-itself)). It holds the Mach service `org.openvirtualsoundcard.audio`
(`MachServices`), which only launchd can hand out, so the driver can connect
only to a daemon that launchd started. `ProcessType` `Interactive` keeps
launchd from throttling it and coalescing its timers.

## Configuration

The installed configuration starts from
[`packaging/macos/ovsc.toml.default`](../packaging/macos/ovsc.toml.default),
which `ovsc example-config --macos` prints. The [OpenVirtualSoundcard
app](#the-openvirtualsoundcard-app) changes the main settings. The file belongs to
root, so edit it with `sudo`, then restart the daemon:

```sh
sudo launchctl kickstart -k system/org.openvirtualsoundcard.daemon
```

The `[audio]` backend must stay `"coreaudio"`. The daemon refuses to start
with a configuration the driver cannot take, and says why in its log.

### Choosing the Dante network interface

`[device] interface` is an interface name (`en0`) or one of its IPv4
addresses. Empty means the interface of the default route, which may be
Wi-Fi: name the Dante interface if so. If the interface is missing or has no
address, the daemon keeps running and retries every few seconds (2 s, backing
off to 5 s), logging `cannot start the engine: ...; retrying until it
starts`. The device stays present and silent meanwhile.

### The device name

Dante Controller lists devices by name, and subscriptions find their
transmitters by name (`<channel>@<device>`), so every device on the network
needs a name of its own.

* The installer names the device after the Mac (`scutil --get
  LocalHostName`, which macOS keeps unique on the network). With
  `name = ""` the device is called `ovsc-` and the last six hex digits of
  the Dante interface's MAC address, such as `ovsc-5279b6` (with
  `-<process_id>` for a second device on one Mac). Neither changes when the
  IP address does. A name set from Dante Controller or the app is saved in
  `state.toml` and wins over the configuration.
* The device's service records point at the host name `<default
  name>.local` (`ovsc-5279b6.local`), as Dante hardware points at its
  factory name, so a device named after the Mac does not clash with the
  Mac's own `<name>.local`.
* When the device starts, and when it is renamed, it first asks the network
  whether another device has the name (mDNS probing, RFC 6762 §8.1); this
  takes about a second before the device appears. If one answers, the
  device takes `<name>-2` (or `-3`, ...), saves it like a rename, and logs
  an error: `the device at <address> already uses the name <name>: this
  device takes the name <name>-2`. The device that had the name keeps it,
  and neither changes name at its next start. Receivers' subscriptions to
  `<name>` reach the device that kept it: re-patch the ones meant for this
  one before the show.
* If another device starts announcing the name while this one runs (one
  that does not probe, or two devices started at the same instant), this
  device keeps its name, since renaming it mid-show would break every
  subscription to it. It logs `the device at <address> also announces
  <name>...: Rename one of them.`, announces its records again, and keeps
  reporting the conflict until the other device withdraws the name or its
  records expire. Meanwhile a subscription on this device to
  `<channel>@<name>` that is made (or that reconnects) goes to the other
  device if that one has the channel, rather than looping back to this one,
  and logs a warning. Subscriptions already receiving stay where they are.
* A Mac cloned from another one's disk has the same `ovsc.toml` and
  `state.toml`, so the same device name. Rename one of the two (Dante
  Controller or the app) before putting both on one network.

### Ports and other Dante software

Audinate's ConMon service (`conmon_cm`), installed with Dante Controller,
Dante Virtual Soundcard and Dante Via, runs all the time and listens on UDP
8800 (CMC) and 8700 (settings) on every interface. Like Dante Virtual
Soundcard, the default configuration therefore moves the device's CMC port to
38800, which it advertises over mDNS, and its settings port to 38700, which
it reports in its CMC replies:

```toml
[device.ports]
cmc = 38800
settings = 38700
```

ARC (4440) and flow control (4455) keep Dante's ports. Dante Controller
reaches a device on its own Mac over 127.0.0.1 for ARC, so the daemon also
answers ARC there. Dante Virtual Soundcard and Dante Via are Dante devices on
those ports too: stop them while OpenVirtualSoundcard runs. A configuration written by an earlier version lacks
`[device.ports]`; the log then says `cannot bind UDP port 8800: Address
already in use`. Add the section and restart the daemon.

### Channels, sample rate and latency

| Key | Values |
|---|---|
| `[device] rx_channels` | 1 to 128: the device's inputs. A count (named `01`, `02`, ...) or a list of names. |
| `[device] tx_channels` | 1 to 128: the device's outputs. |
| `[device] sample_rate` | 44100, 48000, 88200, 96000, 176400 or 192000. |
| `[device] bits_per_sample` | 16, 24 or 32, for the transmitted audio. |
| `[device] latency_ms` | The receive latency, 1 to 40 ms: the device's inputs are read this long after their timestamps. |
| `[device] tx_latency_ms` | The transmit latency, 1 to 40 ms: the least latency the device asks of receivers of its outputs (the console); see [Latency and safety offsets](#latency-and-safety-offsets). Unset: `latency_ms`. |
| `[clock] source` | `"ptp"` follows the network's PTP master; `"free"` runs on this Mac's own clock, for a network without a master. `"system"` is refused. |
| `[device] lock_remote_settings` | `false` (default) or `true`: the [show lock](#locking-the-settings-for-a-show). The app's choice replaces it. |

Both latencies must also be at least `tx_guard_us` (500 µs by default) plus
0.5 ms: the device sends a packet `tx_guard_us` after its first frame's
timestamp, so a latency of `tx_guard_us` or less would make every packet
late, and the 0.5 ms leaves room for the send's wake-up and the network.
With the default guard, 1 ms is the least.

A new rate, channel count or latency reaches Core Audio as a device
configuration change: applications that use the device see it reconfigure.
Core Audio is not restarted. Channel names alone (from the configuration, or
renames from Dante Controller) update without a reconfiguration.
`device.ring_capacity` must stay at its default, 32768.

#### Settings from Dante Controller

In Device View, under Device Config, Dante Controller sets the latency (see
below), the sample rate and the encoding (the bit depth). A new sample rate
or encoding is written into `ovsc.toml`, as from the [OpenVirtualSoundcard
app](#the-openvirtualsoundcard-app), and the daemon restarts with it: audio stops for
10 to 20 seconds in both directions, the feeds other devices receive from
OpenVirtualSoundcard included, and Dante Controller gives no warning first.
Remember that devices only exchange audio at the same sample rate.

Dante Controller also renames the device and its channels. Receivers
subscribe by `channel@device`, so a new device name leaves every
subscription to OpenVirtualSoundcard's outputs unresolved (the console's
returns go silent) until someone patches them again. A transmit channel
keeps answering to its factory name (`01`, `02`, ...), so only subscriptions
made to a previous custom name break.

Any of these changes, made by mistake mid-show from any computer running
Dante Controller, interrupts the show; [lock the
settings](#locking-the-settings-for-a-show) to refuse them.

#### Locking the settings for a show

With the show lock on, the daemon refuses controllers' changes of the
latency, the sample rate, the encoding, the device name and the channel
names. Dante Controller gets an error for a latency write or a rename (the
error result OpenVirtualSoundcard also gives for a value out of range), and
the device reports the sample rate and encoding as not writable. The log says
`refusing ... from the controller at ...: remote settings are locked`.
Subscriptions stay open, so the console can still patch to and from
OpenVirtualSoundcard, and the app's own changes still apply. A write that
changes nothing (the latency it already has, say) is not refused.

Turn it on in the app (Settings, **Show lock**), or from a script with
`{"op":"apply","change":{"lock_remote_settings":true}}`. The choice is saved
in the device's state file and survives restarts, like a rename; it replaces
`[device] lock_remote_settings` (default `false`), which sets it until the
app has.

Whether locked or not, the app's Status lists the last few changes
controllers made, or tried to make while locked, with the controller's
address and how long ago: the explanation for a dropout that a colleague's
Dante Controller caused. The list survives the network engine restarting,
but not the daemon.

#### Setting the latency from Dante Controller or the app

Dante Controller sets the receive latency in Device View, under Device Config
> Latency (1 to 40 ms there); the [OpenVirtualSoundcard app](#the-openvirtualsoundcard-app) sets it
too. Its Latency tab shows how late the packets of each receive flow arrive,
against that setting. Like a rename, the new latency is saved in the device's state file
(`state.toml`, as `latency_ns`) and from then on replaces
`latency_ms`, also after a reboot. Deleting `latency_ns` from the state file
(then restarting the daemon) goes back to `latency_ms`. Only the receive
latency is set this way; while `tx_latency_ms` is unset, the transmit
latency follows it. A saved latency below the least the device allows is
ignored at start.

To apply it, the daemon restarts its network engine while the clock keeps
running and the driver stays attached. Although only the receive side
changes, the restart stops audio in both directions:

1. Each receive flow is stopped, then the device withdraws all its mDNS
   records (goodbyes for the device and every transmit channel).
2. Every transmit flow closes, so the feeds other devices receive from
   OpenVirtualSoundcard (the processed returns to a console, say) stop.
3. The engine starts again with the new latency, announces itself, and
   requests its receive flows again: its inputs come back after about a
   second.
4. Each device that received from it must notice and request its flow
   again. When that happens depends on that device, not on
   OpenVirtualSoundcard; expect several seconds, and test it with your
   console before relying on it.

Core Audio also sees a configuration change with the new latency and safety
offsets, and some applications restart their audio engine on it. The log
says `restarting the network engine for a receive latency of ... ms`, and
warns when transmit flows were open. Change the latency before the show, or
[lock the settings](#locking-the-settings-for-a-show) so that nobody does it
from Dante Controller during one.

The `[coreaudio]` section tunes the device:

| Key | Default | Meaning |
|---|---|---|
| `control_socket` | `"/var/run/ovsc/control.sock"` | The [control socket](#the-control-socket) for the app; empty for none. |
| `allowed_uids` | `[202]` | Effective user IDs allowed to connect; 202 is `_coreaudiod`, Core Audio's driver helper. |
| `input_margin_us` | `500` | Added to the input safety offset; see [Choosing the margins](#choosing-the-margins). |
| `output_margin_us` | `1000` | Added to the output safety offset; see [Choosing the margins](#choosing-the-margins). |
| `output_latency_ms` | unset | Output latency reported to applications; unset means the transmit latency (`tx_latency_ms`, else `latency_ms`). Set it to the console's latency when that is longer. |
| `input_latency_mode` | `"safety"` | `"safety"` or `"latency"`: see [Latency and safety offsets](#latency-and-safety-offsets). |
| `clock_algorithm` | `"raw"` | Core Audio's time stamp smoothing: `"raw"` (none) or `"iir"`. |
| `prevent_idle_sleep` | `true` | Keep the Mac from idle-sleeping while the daemon runs, from its start and across engine restarts. A restart of the daemon itself still releases it for a moment: on a show machine, also turn system sleep off (see [The Mac must not sleep](#the-mac-must-not-sleep)). |
| `status_log_interval_s` | `30` | Seconds between status lines in the log; 0 disables them. |
| `service` | `"org.openvirtualsoundcard.audio"` | The Mach service the daemon serves. Leave it as is: the driver always connects to `org.openvirtualsoundcard.audio`, which is compiled in and listed in its Info.plist, and the launchd job holds that name. |
| `debug_zts_jitter_ns` | `0` | Jitters the time stamps on purpose, for tests only. |

## The OpenVirtualSoundcard app

`/Applications/OpenVirtualSoundcard.app` shows what the daemon is doing and changes its
settings, like Dante Virtual Soundcard's control panel. It talks to the
daemon over its [control socket](#the-control-socket), so it works only for
an administrator of the Mac, and does not need to stay open: the daemon runs
without it.

* The header shows the device name and three lights: **Clock** (green when
  locked to the Dante network, amber while locking, red when unlocked),
  **Driver** (whether Core Audio has loaded the OpenVirtualSoundcard driver) and
  **Audio** (whether an application is playing or recording through it).
* **Status** lists the device (interface, address, sample rate, bit depth,
  latency, whether settings from the network are locked), the clock (its
  leader, offset, network delay and rate), the receive channels with what
  each one is subscribed to, the transmit flows and the packet counters.
  Warnings from the daemon come first: a clock that does not lock, and
  the changes controllers made or tried to make, newest first. The header
  says `settings locked` while the show lock is on.
* **Settings** changes the device name, the network interface, the sample
  rate, the bit depth, the receive and transmit channel counts, the
  latency and the [show lock](#locking-the-settings-for-a-show). **Apply**
  sends only what changed, and asks first for anything that interrupts
  audio or other devices' subscriptions, saying what it does:
  * A new latency restarts the network engine: audio stops for a second or
    two in both directions, and devices receiving from OpenVirtualSoundcard,
    such as a console taking its returns, must request their flows again
    (see [above](#setting-the-latency-from-dante-controller-or-the-app)).
  * A new interface, rate, bit depth or channel count is written into
    `ovsc.toml`, keeping its comments, and restarts the whole daemon:
    audio stops for 10 to 20 seconds in both directions while the clock
    locks again.
  * A new name applies at once, but subscriptions other devices made to
    OpenVirtualSoundcard's outputs name the old one: they stop resolving
    until patched again in Dante Controller. The question lists the devices
    receiving from it now.
  * The show lock applies at once and interrupts nothing.

Build it with `packaging/macos/build-app.sh`, which needs nothing beyond
Rust and the command-line tools. It is signed ad hoc; built on the Mac that
runs it, it carries no quarantine, so Gatekeeper lets it open. Its icon is
drawn in [`apps/macos/icon/OpenVirtualSoundcard.svg`](../apps/macos/icon/OpenVirtualSoundcard.svg);
`apps/macos/icon/make-icon.sh` renders it into the `.icns` the bundle carries
and the PNG the app shows in the Dock.

### The control socket

The daemon listens on the Unix socket `/var/run/ovsc/control.sock`,
owned by root and the `admin` group with mode 0660 (`[coreaudio]
control_socket`; empty turns it off). Each request is one line of JSON and
gets one line back; the types are in
[`crates/ovsc-control`](../crates/ovsc-control/src/lib.rs). From a
script:

```sh
echo '{"op":"status"}' | nc -U /var/run/ovsc/control.sock
echo '{"op":"apply","change":{"latency_ms":2}}' | nc -U /var/run/ovsc/control.sock
```

| Request | Answer |
|---|---|
| `{"op":"status"}` | `"result":"status"`: the device, the clock, the driver and warnings. |
| `{"op":"settings"}` | `"result":"settings"`: `name`, `interface`, `sample_rate`, `bits_per_sample`, `rx_channels`, `tx_channels`, `latency_ms`, `lock_remote_settings`. |
| `{"op":"interfaces"}` | `"result":"interfaces"`: the interfaces that are up and have an IPv4 address. |
| `{"op":"apply","change":{...}}` | `"result":"applied"` with `"restart"`: `"none"`, `"device"` (the network engine restarts) or `"daemon"` (the daemon exits for launchd to start it again); or `"result":"error"` with a `message`, and nothing changed. `change` holds any of the settings' fields. |

## What applications see

| Property | Value |
|---|---|
| Name | `Open Virtual Soundcard` (streams: `Open Virtual Soundcard Input`, `… Output`) |
| Manufacturer | `OpenVirtualSoundcard` |
| UID | `org.openvirtualsoundcard.vsc` (model UID `org.openvirtualsoundcard.vsc.model`) |
| Transport type | virtual |
| Alive | always, also while the daemon is down |
| Default device | may be the default input or output; never the system (alert sound) device |
| Sample rates | only the configured one. Setting it to the current rate succeeds, any other rate fails. |
| Format | interleaved 32-bit float, one stream per direction |
| Channel names | the Dante channel names, per input and output element |
| Zero time stamp period | 16384 frames |
| Clock algorithm | `raww` or `iirf`, from `clock_algorithm`; clock is stable; clock domain 0 |
| Latency, safety offset | see below; stream latency 0 |

Core Audio remembers the last configuration the driver applied, so after a
reboot the device shows its channels and names before the daemon is up.

### Latency and safety offsets

Core Audio defines `kAudioDevicePropertySafetyOffset` as how many frames
behind (input) or ahead of (output) the current position IO is safe, and
`kAudioDevicePropertyLatency` as frames of latency in the device. The daemon
computes both (`compute_offsets` in
[`crates/ovsc-ipc/src/protocol.rs`](../crates/ovsc-ipc/src/protocol.rs)),
with:

* `L = round(latency_ms × fs / 1000)`, the receive latency;
* `T = round(tx_latency_ms × fs / 1000)`, the transmit latency (`T = L`
  while `tx_latency_ms` is unset);
* `guard = floor(tx_guard_us × fs / 1e6)`: a packet is sent this long after
  its timestamp (`tx_guard_us` is 500 µs by default);
* `FPP_MAX = 32`, the most frames per transmitted packet;
* `margin(us) = ceil(us × fs / 1e6)`.

| Property | `"safety"` mode (default) | `"latency"` mode |
|---|---|---|
| Input safety offset | `L + margin(input_margin_us)` | `margin(input_margin_us)` |
| Input latency | 0 | `L` |
| Output safety offset | `max(FPP_MAX − 1 − guard, 0) + margin(output_margin_us)` | same |
| Output latency | `T`, or `output_latency_ms` | same |

In safety mode the network latency is part of the input safety offset, so
the input time stamps an application gets are the senders' capture times:
recorded audio lines up with the network's timestamps. In latency mode the
driver reads `L` frames further back and reports `L` as input latency, for
applications that compensate input latency themselves. A host that adds both
values sees the same input total in either mode.

The output safety offset covers the transmit thread: a packet carries up to
`FPP_MAX` frames and leaves `guard` after its first frame's timestamp, so a
frame must be in the ring up to `FPP_MAX − 1 − guard` frames before its own
time. The output margin adds room for the driver writing late ([Choosing
the margins](#choosing-the-margins)).

A receiver plays an output frame at its timestamp plus the larger of its own
receive latency and the latency the device asks for in its transmit
channels' advertisement, `T` (TXT `latency_ns`, [PROTOCOL.md](PROTOCOL.md)
sections 2.2 and 8.4). So the console plays the device's outputs
`max(console latency, T)` after their time, and the device reports `T` as
its output latency. That is right while the console's latency is at most
`T`; if it is longer, set `output_latency_ms` to it, or hosts under-report
the output path by the difference. While `tx_latency_ms` is unset, `T` is
the receive latency: raising `latency_ms` (here, from the app or from Dante
Controller) then delays the console's playout of the outputs as well as the
inputs. For live use, set `tx_latency_ms = 1.0`: the console then plays the
returns at its own latency (at least 1 ms), and `latency_ms` sets only the
input side.

With the default margins, 4 ms latency (`tx_latency_ms` unset) and the
default guard:

| Rate | `L` | Input safety | Output safety | Output latency |
|---|---|---|---|---|
| 44100 | 176 | 199 | 54 | 176 |
| 48000 | 192 | 216 | 55 | 192 |
| 88200 | 353 | 398 | 89 | 353 |
| 96000 | 384 | 432 | 96 | 384 |
| 176400 | 706 | 795 | 177 | 706 |
| 192000 | 768 | 864 | 192 | 768 |

The test `offsets_match_the_design_table` in
[`crates/ovsc-ipc/tests/protocol.rs`](../crates/ovsc-ipc/tests/protocol.rs)
checks this table against `compute_offsets`, and the end-to-end test checks
the 48 kHz row on a real device (scenario S2).

#### Round trip

The HAL reads input one buffer plus the input safety offset behind its IO
cycle and writes output one buffer plus the output safety offset ahead of
it ([Measured IO timing](#measured-io-timing)). With a buffer of `B` frames,
a frame captured at a stagebox is therefore played by the console

```text
2·B + input safety + output safety + max(console latency, T)
  = 2·B + L + margin(input_margin_us)
        + max(FPP_MAX − 1 − guard, 0) + margin(output_margin_us)
        + max(console latency, T)
```

after its capture, plus the plug-ins' own latency and the stagebox's and
console's converters and processing. A host that adds both buffers and the
device's latencies and safety offsets reports the same figure while the
output latency is right (see above). At 48 kHz with the default margins and
guard, and a console at 1 ms:

| `latency_ms` | `tx_latency_ms` | B = 32 | B = 64 | B = 128 |
|---|---|---|---|---|
| 4 | unset | 11.0 ms | 12.3 ms | 15.0 ms |
| 4 | 1 | 8.0 ms | 9.3 ms | 12.0 ms |
| 2 | 1 | 6.0 ms | 7.3 ms | 10.0 ms |
| 1 | 1 | 5.0 ms | 6.3 ms | 9.0 ms |

Each millisecond of `latency_ms` adds a millisecond, and so does each one of
`T` above the console's latency.

#### Choosing the margins

The input margin covers the driver's timeline and the daemon's clock
disagreeing (by up to 250 µs while audio flows, see [Clock states and
silence](#clock-states-and-silence)) and packets a little later than `L`.
The output margin covers the same disagreement, HAL wake-ups 1 to 3 frames
late and IO cycles that finish late, so that output reaches the rings
before the transmit thread reads it; a transmit thread that runs late needs
none, since it then reads later still. Neither should be below the 250 µs
clock bound plus a few frames, about 300 µs. The defaults (500 µs in,
1000 µs out) leave room for a busy Mac; each 100 µs less shortens the round
trip by 100 µs.

To lower them, run the show's load (the session, its plug-ins and buffer
size) for at least an hour, then read `out_margin` and `in_margin` in the
[status](#status) (the log's `min_out_margin` and `min_in_margin`), and
`late_out`, `early_in` and `tx_underruns`, which must stay 0. The smallest
margin of each, less 12 frames (250 µs at 48 kHz) and a few more, is how far
that margin may come down; repeat the test after changing it. On the input
side, a receive latency `L` that packets keep to (the app's Latency tab, the
`rx_late` counter) matters more than the margin.

## Clock states and silence

The daemon's clock is in one of these states:

| State | Meaning |
|---|---|
| Unlocked | No master heard yet, or the master went silent. After losing a master the clock keeps running on its frequency estimate for 30 s (holdover), then becomes invalid. |
| Locking | A master was found and the servo is converging; or a locked clock is slewing back after its error stayed above 250 µs (an unlock), or checking its time against a new master. |
| Locked | Following the master. |
| FreeRunning | `clock.source = "free"`. |

The driver keeps its own continuous timeline for Core Audio
([Device timeline](#device-timeline)). It starts following a daemon clock
once that clock is Locked or FreeRunning, and from then on keeps following
it in any state for as long as its `step_gen` stays the same: the daemon
bumps `step_gen` on every jump of its clock, so a clock with the same
`step_gen` is continuous with what the driver plays. The status shows the
driver's side as `clock=synthetic` (it has not followed a daemon clock yet),
`clock=following` or `clock=holdover` (it lost the daemon's clock and keeps
going at the last rate).

Audio flows only while the driver's **gate** is open (`gate=1`). The gate is
open when all of these hold:

* the driver has the daemon's region (`daemon=attached`);
* the daemon's engine runs at the device's sample rate, and its heartbeat is
  less than 1 s old;
* the daemon's clock is valid and within 1000 ppm of nominal;
* the driver's timeline follows this clock, or continues it in holdover: a
  new daemon's clock, or the same clock after a step, is shut out until it
  has locked and the timeline follows it (so never while
  `clock=synthetic`);
* the daemon's clock agrees with the driver's timeline to within 250 µs.

Otherwise the inputs read silence and the outputs are dropped. So:

* before the daemon's clock first locks, the device is silent;
* when Syncs are delayed for a few seconds (a network without
  strict-priority QoS for PTP, a loaded 100 Mbit link), the daemon's clock
  may unlock: it keeps its frequency estimate and slews back gently, by at
  most 100 ppm, and the driver keeps following it. Audio keeps flowing and
  nothing slips;
* when the master goes away (no Sync for 5 s), the daemon's clock holds over
  for 30 s on its frequency estimate, and the driver follows it. When the
  same or a new master is heard, the daemon checks it against its running
  clock: within 250 µs it slews on and audio keeps flowing; beyond, it steps
  (see the next point). If the holdover runs out first, the device is silent
  until the daemon's clock locks again;
* a new leader that was synchronised to the old one, the normal Dante leader
  change (the leader reboots, Preferred Leader is moved, a better device
  joins), agrees with the running clock to within microseconds: the clock
  stays continuous, `step_gen` does not change, and audio keeps flowing.
  If the old leader just went silent, the new one takes over once the old
  one has been silent for 1 s (four of its Sync intervals if it advertised
  slower Syncs than one every 250 ms, and at most the 5 s master timeout)
  and the new one has sent two Syncs, while the clock runs on;
* when the clock steps (the master's time jumped by more than 2 ms, or a new
  master's time differs from the running clock by more than 250 µs), the
  device is silent until the daemon's clock has locked again, then the
  driver realigns once: the audio slips once, but the timeline Core Audio
  sees stays continuous. For a new master 10 ms off, the replay test
  measures 1.6 s of silence at the median and 5.5 s at worst;
* when the daemon restarts, audio stops until the new daemon's clock locks
  (3.6 s at the median and about 7.6 s at worst in the servo simulation).
  Core Audio sees no change: no reconfiguration, no new timeline;
* when the daemon is stopped, the device stays present and silent.

The daemon's clock status counts unlocks, steps (with the size of the last
one), master changes and master losses, and reports the masters heard
besides the one followed, the time of the last Sync and when a holdover
runs out, so that it can warn before a problem is heard.

## Status

The device publishes its state as a custom string property, `'ovst'`. Read it
with `coreaudio-check`, the test client in
[`tools/coreaudio-check`](../tools/coreaudio-check/README.md):

```sh
(cd tools/coreaudio-check && cargo build --release)
tools/coreaudio-check/target/release/coreaudio-check prop org.openvirtualsoundcard.vsc ovst --type cfstring
```

The line is a list of `key=value` fields:

| Field | Meaning |
|---|---|
| `daemon` | `absent` (not started, or the service does not exist), `connecting`, `attached` followed by `gen=<generation>`, or `incompatible(<reason>)` with reason `proto`, `layout`, `region`, `map`, `clock`, `generation` or `reply`. |
| `clock` | The driver's timeline: `synthetic`, `following` or `holdover`. |
| `ppm` | The rate of the driver's timeline against its nominal rate, in ppm of this Mac's clock. |
| `rate`, `in`, `out` | The sample rate and channel counts the device publishes. |
| `gate` | 1 while audio flows. |
| `absorbs` | Clock discontinuities absorbed so far (each one slips the audio once). |
| `seed` | Core Audio's timeline seed; it changes only with a new timeline. |
| `late_out`, `early_in` | IO cycles whose output came too late for the daemon, or whose input was read too early. |
| `far_out`, `far_in` | IO cycles whose output or input margin was at least half a zero time stamp period (8192 frames): the sign of a host that reads the time stamps a period off. |
| `out_margin`, `in_margin` | The smallest and largest output and input margins since IO last started, as `min..max` frames, or `-` while none has been measured. The output margin is how many frames are left before the daemon sends the first frame written; the input margin, how long the last frame read had been in the ring. A negative margin is a late output or an early input. |
| `silenced` | IO operations the closed gate silenced. |
| `in_missing` | Input channel-frames that had no sample. |
| `tx_underruns` | Packets the daemon sent with a channel the driver had not filled. |
| `io`, `zts` | Core Audio clients doing IO; zero time stamp calls. |
| `attach` | Daemon regions attached since the driver loaded. |
| `faulted` | 1 if the driver caught a panic; its IO is then silent. |
| `v` | The driver's version. |

Other `coreaudio-check` commands: `list` (every device), `info
org.openvirtualsoundcard.vsc` (the properties applications see) and `wait-status
org.openvirtualsoundcard.vsc clock=following 60` (waits for a status substring).

The daemon logs to `/Library/Logs/OpenVirtualSoundcard/ovsc.log`:

```sh
sudo tail -400 /Library/Logs/OpenVirtualSoundcard/ovsc.log
sudo launchctl print system/org.openvirtualsoundcard.daemon
```

The daemon opens this file itself (`--log-file`) and looks every 5 s whether
it is still the file it writes to. When newsyslog has rotated it (at 1 MiB,
keeping `ovsc.log.0` to `ovsc.log.4`), the daemon logs `log file ... reopened
after a rotation` at the top of the new `ovsc.log`; the last few seconds
before that are at the end of `ovsc.log.0`. An older daemon (before
`--log-file`) went on writing to `ovsc.log.0`, which then grew without
bound: if `ovsc.log` stays empty, reinstall, or restart the daemon.

Lines worth knowing:

* `hal: serving org.openvirtualsoundcard.audio (region generation <hex>, 67174400 bytes)`
  at start;
* `hal: plug-in attached: pid <helper>, ...` when a driver instance connects;
* `clock locked to ...` and `clock lost`;
* `the network interface <name> is gone`, `... lost its IPv4 address`,
  `... has its address <ip> back`, and `<why>: restarting the network
  engine` ([The network changed](#the-network-changed));
* `<what> panicked: ...` and `...: restarting the daemon`
  ([The daemon restarted by itself](#the-daemon-restarted-by-itself));
* `the saved state ... could not be read` ([The saved subscriptions are
  gone](#the-saved-subscriptions-are-gone));
* every `status_log_interval_s`: `clock <state>; receiving <n>/<m>
  subscribed channels; <k> transmit flows; packets: tx <n> (<u> underruns),
  rx <n> (<l> late)[; <interface problem>]; hal: peers=... io=... gate=... regime=... absorbs=...
  seed=... late_out=... early_in=... silenced=... in_missing=...
  tx_underruns=... min_out_margin=... min_in_margin=...`, the margins being
  the smallest seen since IO started, in frames. A late packet arrived after
  its timestamp plus the latency: its audio may have been read already;
* once per IO session, `hal: io_trace <session> <i>: ...`: the first 64 IO
  operations, with the sample times Core Audio passed
  ([Measured IO timing](#measured-io-timing)).

The driver logs to the unified log, subsystem `org.openvirtualsoundcard`, category
`driver`:

```sh
log show --last 2m --style compact --info --debug --predicate 'subsystem == "org.openvirtualsoundcard" OR process CONTAINS "Core-Audio-Driver" OR (process == "coreaudiod" AND eventMessage CONTAINS[c] "OpenVirtualSoundcard")'
```

[`ci/macos/diagnostics.sh`](../ci/macos/diagnostics.sh) prints all of the
above, plus the installed bundle's signature and any crash reports. It only
reads.

## Uninstalling

```sh
sudo "/Library/Application Support/OpenVirtualSoundcard/uninstall.sh"
```

It stops the daemon, removes the launchd job, the driver, the daemon, the
`/usr/local/bin/ovsc` link, the log rotation rule, the firewall entry and
the package receipt, then restarts `coreaudiod` so that the device goes away.
The configuration, the state and the logs stay for a later install; add
`--purge` to remove them too.

## Troubleshooting

### The device does not appear

1. Is the driver loaded? `tools/coreaudio-check/target/release/coreaudio-check list`
   (built as in [Status](#status)) should show `org.openvirtualsoundcard.vsc`, and Core
   Audio should run the driver's helper process:

   ```sh
   pgrep -fl 'Core Audio Driver \(OpenVirtualSoundcard.driver\)'
   ```

2. If not, restart Core Audio, as `install.sh` does:

   ```sh
   sudo killall coreaudiod
   ```

   launchd starts a new `coreaudiod` when an application asks for audio.
   Do not use `launchctl kickstart` on `coreaudiod`; macOS 14.4 and later
   refuse it. If the device still does not appear, restart the Mac, which is
   what the installer package advises when the driver has not connected.
3. Check the unified log (above) for the driver's messages.
   `ci/macos/diagnostics.sh` also shows what `amfid` logged about the bundle
   ([Gatekeeper and signing](#gatekeeper-and-signing)).

### The device is silent

Read the status ([Status](#status)):

* `daemon=absent`: launchd has no OpenVirtualSoundcard job loaded. Start it with
  `sudo launchctl bootstrap system /Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist`.
* `daemon=connecting` for more than a few seconds: the job is loaded but the
  daemon does not answer, usually because it exits at start and launchd
  keeps restarting it. `sudo launchctl print system/org.openvirtualsoundcard.daemon`
  and the log say why (often the configuration). Fix that, then
  `sudo launchctl kickstart -k system/org.openvirtualsoundcard.daemon`.
* `daemon=incompatible(proto)` or `incompatible(layout)`: the daemon and the
  driver come from different versions. Install both from the same build
  (`install.sh` does). `reply` means the driver could not decode the
  daemon's answer to hello. Other reasons mean the region was unusable. The
  driver's log says why.
* `clock=synthetic`, or `gate=0`: the clock is not locked, or the engine is
  not running. The log says which: `engine not running: ...`, `cannot start
  the engine: ...` (often the interface), or the clock state in the status
  line. Is there a PTP master (a Dante device) on the interface the daemon
  uses? See [The clock does not lock](#the-clock-does-not-lock).
* `gate=1` but silence: check the subscriptions (`ovsc info <name>`) and
  that the senders reach this Mac.

### The clock does not lock

The log keeps saying `clock Unlocked` and never `following PTP master`.
Watch the Dante interface for the leader's PTP packets, which go to
224.0.1.129 on UDP 319 and 320 several times a second:

```sh
sudo tcpdump -n -i en7 -c 6 'udp port 319 or udp port 320'
```

with the Dante interface in place of `en7`.

* Nothing arrives, although Dante Controller lists the devices: something
  between the leader and this Mac drops the PTP group but passes the
  224.0.0.x traffic that discovery uses, typically a router's built-in
  switch or a switch doing IGMP snooping without a querier. Connect the Mac
  to the same switch as the Dante devices, and on a managed switch enable an
  IGMP querier or turn IGMP snooping off. `netstat -gn | grep 224.0.1.129`
  shows that the daemon has joined the group on the right interface.
* Packets arrive, but tcpdump marks them `truncated-ip - 12 bytes missing!`
  and `netstat -s -p ip` counts them under `with data size < data length`:
  the Ethernet adapter's driver cuts the end off PTP packets, and macOS
  drops them before any program sees them. This was seen with a CalDigit
  Thunderbolt Ethernet adapter (an Intel chip, which timestamps PTP in
  hardware). Use another Ethernet adapter for the Dante network: a plain
  USB-C adapter worked.

### The network changed

The daemon looks at its network interface every second. It restarts the
whole network engine (clock, device, advertisement; the driver stays
attached and reads silence meanwhile) when:

* the interface came back after it was gone: a USB-C or Thunderbolt
  Ethernet adapter or a dock that re-enumerated after a knock or a power
  blip. Its multicast memberships (PTP, mDNS) went with it, so without a
  restart the clock would be lost some 35 s later, for good;
* the interface was attached again under a new index;
* the interface no longer has the engine's address: a DHCP lease replacing
  a self-assigned 169.254 address, or a router handing out a new lease;
* the interface got its address back after a cable pull, but the clock has
  not locked 15 s later.

A restart needs two looks in a row (2 s). While the interface is gone or
has no address the engine waits for it; after a cable pull that brings the
same address back, nothing restarts and the clock locks again by itself.
Audio returns once the clock has locked again, typically a few seconds
after the restart. The log says what happened (`the network interface en7
is gone`, `... is back: restarting the network engine`), and the status
line ends with the problem while it lasts.

Name the Dante interface in `[device] interface` for a show: with the
default route (an empty name), a restart picks whatever carries the default
route at that moment, which may be Wi-Fi.

### The daemon restarted by itself

A part of the daemon failed: a panic in any thread (logged at error level
as `... panicked: ...`), a device task or the transmit thread that stopped,
or the driver heartbeat task. Rather than run on half dead (silent with a
green status), the daemon says goodbye to the driver and exits with an
error; launchd starts a new one within `ThrottleInterval` (5 s), and audio
returns once its clock locks. `sudo launchctl print
system/org.openvirtualsoundcard.daemon` shows the last exit status. Please
report the panic message with the log.

### The saved subscriptions are gone

The renames, subscriptions and latency set from Dante Controller or the app
are saved in `state.toml` ([What gets installed](#what-gets-installed)). Each
save is flushed to the disk before it replaces the file, and the previous
version is kept as `state.toml.bak`. If the file cannot be read at start
(empty or damaged, after a power cut during a save on an older version, or
a disk fault), the daemon logs `the saved state ... could not be read` as
an error, starts from `state.toml.bak`, and the app shows a warning. Once
the engine runs, the daemon saves the state it runs with, keeping the
damaged file as `state.toml.corrupt-<unix time>` first. Check the
subscriptions in Dante Controller then. The damaged file is never
overwritten, so it can still be repaired by hand: stop the daemon, copy the
repaired file over `state.toml`, and start it again.

### The Mac must not sleep

With `prevent_idle_sleep = true` (the default) the daemon keeps the Mac
from idle-sleeping from the moment it starts, across engine restarts. A
restart of the daemon itself (a settings change in the app, a crash)
releases it for a few seconds, and nothing keeps a display or a lid from
sleeping. On a show machine, also turn sleep off:

```sh
sudo pmset -a sleep 0 disablesleep 1
pmset -g assertions | grep -i openvirtualsoundcard
```

### Firewall

When the application firewall is on, `install.sh` lets the daemon accept
incoming connections, as Dante Virtual Soundcard's installer does. If the
firewall was turned on later, run `install.sh` again, or:

```sh
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add "/Library/Application Support/OpenVirtualSoundcard/bin/ovsc"
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp "/Library/Application Support/OpenVirtualSoundcard/bin/ovsc"
```

### Gatekeeper and signing

`build-driver.sh` signs the driver ad hoc unless `CODESIGN_IDENTITY` is set,
and `install.sh` removes the quarantine attribute from the installed files.
The ad-hoc signed driver, built on the Mac itself, loads with SIP enabled: a
local end-to-end run on an M1 Mac with macOS 26.6.2 passed every scenario
but S6, whose failure was in the test client. On GitHub's runners, which run
with SIP disabled, it loads too; `amfid` logs
`AppleMobileFileIntegrityError -423 ("adhoc signed")`, but that does not stop
it ([`tools/macos-probe/README.md`](../tools/macos-probe/README.md#results)).
Packages for download need a Developer ID and notarization ([Installer
package](#installer-package)), since a downloaded file is quarantined.

## Architecture

```
 Dante network (PTP, ARC/CMC/conmon, audio)                 applications
        │ UDP                                                    │ Core Audio
┌───────┴────── ovsc run (LaunchDaemon, root) ──────┐   coreaudiod
│ PTP follower ─► MediaClock ─► ShmClockMirror ─┐        │       │
│ device engine: RX threads ─► RX rings ────────┤        │   helper "Core Audio Driver
│                TX thread  ◄─ TX rings ◄───────┤        │   (OpenVirtualSoundcard.driver)", _coreaudiod
│ ovsc-hal-server: XPC listener,           │        │       │
│   heartbeat, configuration, status log        │        │   OpenVirtualSoundcard.driver (ovsc-hal)
└──────────────────────┬────────────────────────┼────────┘   GetZeroTimeStamp ◄─ clock block
                       │ XPC org.openvirtualsoundcard.audio│            ReadInput ◄─ RX rings
                       │ (control only)         ▼            WriteMix  ─► TX rings
                       └──────────────► one xpc_shmem region ◄──────┘
                                        (ovsc-shm layout)
```

| Crate | Role |
|---|---|
| [`ovsc-shm`](../crates/ovsc-shm) | `no_std`, no dependencies: the region layout, the rings, the clock block seqlock, the status blocks, sample conversion and the device timeline. Both sides compile the same definitions. |
| [`ovsc-ipc`](../crates/ovsc-ipc) | The messages, the driver configuration and its offsets, the XPC transports over a small C shim ([`c/ovshim.c`](../crates/ovsc-ipc/c/ovshim.c)), an in-memory transport for tests, and the region's memory. |
| [`ovsc-hal`](../crates/ovsc-hal) | The driver: a static library linked into the bundle, exporting only `OpenVirtualSoundcard_Factory`, and an rlib so that it runs under `cargo test` on every OS. Also builds `ovsc-hal-selftest`. |
| [`ovsc-hal-server`](../crates/ovsc-hal-server) | The daemon's side: the region, the clock mirror, the service, the driver configuration, the status task and the power assertion. |
| [`ovsc`](../crates/ovsc) | The `coreaudio` backend of `ovsc run` ([`src/coreaudio.rs`](../crates/ovsc/src/coreaudio.rs)). |

**Daemon.** `ovsc run` with `backend = "coreaudio"` creates the region,
starts the service, then starts the engine on the region's rings with its
clock mirrored into the region, retrying until it starts. The engine is the
same as on other systems; only its rings live in the region. On macOS the RX
and TX threads run with the time-constraint scheduling policy.

**Driver.** The driver creates no threads. Its real-time paths
(GetZeroTimeStamp, WillDoIOOperation, DoIOOperation) never allocate, lock,
log or call the host; everything else runs on one serial queue,
`org.openvirtualsoundcard.ipc`. Every entry point catches panics. ReadInput copies RX
ring slots `t + off − read_delay` into Core Audio's buffer, WriteMix stores
the mix at TX ring slots `t + off`, where `t` is Core Audio's sample time and
`off` the timeline's ring offset. A ring slot holds the sample and the low 32
bits of its timestamp, so a stale slot reads as silence. Every 24-bit sample
survives the trip through Float32 bit for bit.

**IPC.** The driver's Info.plist lists `org.openvirtualsoundcard.audio` under
`AudioServerPlugIn_MachServices`. Messages are XPC dictionaries with an `op`:

| Op | Direction | Purpose |
|---|---|---|
| `hello` | driver → daemon, with reply | Protocol and layout versions, layout hash, what the driver has applied. |
| `welcome` | reply | The region (an `xpc_shmem` under the key `region`), its size and generation, and the configuration. |
| `reject` | reply | Reason `proto` or `layout`; the device stays present but inert, and says hello again every 30 s. |
| `config` | daemon → driver | A new configuration. |
| `config_applied` | driver → daemon | What the driver now publishes. |
| `bye` | daemon → driver | The daemon is shutting down. |

The daemon accepts a connection only from an effective user ID in
`allowed_uids`. A hello waits 2 s for its reply. After losing the daemon the
driver says hello at 0, 0.25, 0.5, 1, 2 and 5 s, then every 5 s; it keeps the
old region meanwhile and releases it after 30 s without a welcome. A welcome
of the same generation keeps the mapping; a new generation maps the new
region and retires the old one.

**Configuration changes.** The configuration travels in every welcome, and
the daemon pushes a new one to attached drivers when it changes, for example
when a controller renames a channel. A new rate or channel count comes with a
daemon restart, in the new daemon's welcome. Names alone are published at once,
with a property-changed notification on the element names. Anything else
(rate, channel counts, offsets, latencies, clock algorithm) is structural:
the driver asks Core Audio for a configuration change
(RequestDeviceConfigurationChange) and applies it when Core Audio calls
PerformDeviceConfigurationChange. It sends no request in the first 2 s after
starting, keeps at most one outstanding, and retries 1 s after an abort. What
it applies is kept in Core Audio's storage under `org.openvirtualsoundcard.config.v1`.

## Shared region

The daemon creates one region per process with `mmap(MAP_ANON|MAP_SHARED)`
and wraps it in an `xpc_shmem` at once. The driver maps it from the welcome.
The layout is in
[`crates/ovsc-shm/src/layout.rs`](../crates/ovsc-shm/src/layout.rs):

| Offset | Size | Block | Writer → reader | Constant |
|---|---|---|---|---|
| `0x0000` | 256 B | Header: magic, version, sizes, offsets, layout hash, daemon generation, pid, arch, timebase, version | daemon, once → driver | `HEADER_OFFSET` |
| `0x0400` | 128 B | ClockBlock: seqlock over host and media reference times, rate, state, step generation, grandmaster | daemon clock mirror → driver | `CLOCK_OFFSET` |
| `0x0800` | 256 B | DaemonStatus: heartbeat, rate, channels, latency, TX guard, flags, packet counters, peers | daemon → driver | `DAEMON_STATUS_OFFSET` |
| `0x0C00` | 512 B | PluginStatus: IO counters and margins, timeline state, applied configuration | driver → daemon | `PLUGIN_STATUS_OFFSET` |
| `0x1000` | 128 B | IoTraceHeader: session, entry count | driver → daemon | `IO_TRACE_OFFSET` |
| `0x1080` | 64 × 64 B | IoTraceEntry: the first 64 IO operations of a session | driver → daemon | `IO_TRACE_ENTRIES_OFFSET` |
| `0x0001_0000` | 128 × 256 KiB | RX rings, ring `c` at `RX_OFFSET + c × RING_BYTES` | daemon RX threads → driver ReadInput | `RX_OFFSET` |
| `0x0201_0000` | 128 × 256 KiB | TX rings, ring `c` at `TX_OFFSET + c × RING_BYTES` | driver WriteMix → daemon TX thread | `TX_OFFSET` |
| `0x0401_0000` | | end | | `REGION_SIZE` |

| Constant | Value | Meaning |
|---|---|---|
| `MAX_CHANNELS` | `128` | Rings per direction. |
| `RING_FRAMES` | `32768` | Slots per ring: 682 ms at 48 kHz, 170 ms at 192 kHz. The same as `ovsc-core`'s default ring capacity. |
| `RING_BYTES` | `0x0004_0000` | 256 KiB per ring: `RING_FRAMES` 8-byte slots. |
| `REGION_SIZE` | `0x0401_0000` | 67,174,400 bytes, 64 MiB + 64 KiB of address space. Only the pages of active channels are touched, so only those are committed. |
| `IO_TRACE_ENTRIES` | `64` | IO operations traced per session. |
| `IO_FRAMES_CAP` | `16384` | Frames per IO operation the driver handles; more are zero-filled or dropped, and counted. |
| `LAYOUT_VERSION` | `1` | Bumped on an incompatible change. |

Every field is a fixed-size integer, every shared one an `AtomicU64` with a
single writer, so the layout is the same on arm64 and x86_64. The driver
never trusts what it reads: it compares every header field with its compiled
constants, including `LAYOUT_HASH`, an FNV-1a hash over every constant, block
size and field offset, pinned in
[`tests/layout_golden.rs`](../crates/ovsc-shm/tests/layout_golden.rs).
It also rejects a region whose daemon heartbeat is more than 10 s from its
own clock: the two processes then do not share a clock base. Times in the
region are host nanoseconds of `CLOCK_UPTIME_RAW`; the driver converts to
Mach ticks only when it answers Core Audio.

[`crates/ovsc-shm/tests/docs_constants.rs`](../crates/ovsc-shm/tests/docs_constants.rs)
checks the two tables above and the service name against the code.

The clock block is a seqlock written by the daemon's clock mirror
synchronously on every clock change. The driver reads it with a bounded
number of tries and keeps its last good record, so a daemon that dies in the
middle of a write cannot make the IO thread spin. The mirror counts
discontinuities in `step_gen`: a jump of the media time by more than 1 µs, the
clock becoming valid, or a change into FreeRunning. A change of master or of
lock state is not one by itself, and neither is losing the master: the
mapping says whether the time moved.

## Device timeline

Core Audio extrapolates a device's sample clock from the zero time stamps the
driver returns, one every 16384 frames, and re-anchors, audibly, when they
disagree. The daemon's clock can jump (lock, step, new master, restart), so
the driver does not report it directly. It runs its own continuous model
([`crates/ovsc-shm/src/timeline.rs`](../crates/ovsc-shm/src/timeline.rs)):

```
T(h) = t_whole + t_frac + (h − h_a) × rho        device frame t lives at ring index t + off
```

* `rho` moves towards the daemon's rate by at most 200 ppm per second, plus a
  phase correction of at most 20 ppm with a 2 s time constant.
* A discontinuity, seen as a new (daemon generation, `step_gen`) pair, leaves
  `T` alone and moves the integer offset `off` by the whole frames of the
  error, if that is at least half a frame: the audio slips once, the
  timeline does not move.
* Only a rate jump beyond 200 ppm, a reset (a new sample rate) or an IO stall
  of more than 4 periods gives Core Audio a new timeline (a new seed).
* The timeline follows the daemon's clock only while it is valid and within
  1000 ppm, the daemon's engine runs at the device's rate, and its heartbeat
  is under 1 s old; and only once it is Locked or FreeRunning, unless it is
  the clock (daemon generation and `step_gen`) the timeline already
  follows, which it follows in any state. Otherwise it holds over at its
  current rate.

The audio passes the network unchanged: the driver writes output frame `t` at
ring index `t + off`, the daemon sends it stamped `t + off`, and a
self-subscribed receive channel writes it back at `t + off`, where ReadInput
at input time `t` reads it. The round trip in device time is 0 frames, which
the end-to-end test checks bit for bit.

## Retire protocol

When a daemon restarts, the driver must replace the region under running IO
without the IO thread ever touching unmapped memory
([`crates/ovsc-hal/src/io/attach.rs`](../crates/ovsc-hal/src/io/attach.rs),
[`crates/ovsc-hal/src/link/retire.rs`](../crates/ovsc-hal/src/link/retire.rs)).

Real-time readers count themselves in before loading the current attachment:

```
READERS.fetch_add(1, SeqCst); a = CURRENT.load(SeqCst); ...use a...; READERS.fetch_sub(1, Release)
```

The serial queue replaces it:

1. swaps the new attachment (or none) into `CURRENT`;
2. at once maps an anonymous private region over the old range with
   `mmap(MAP_FIXED|MAP_ANON|MAP_PRIVATE)` ("neutralize"). A straggling reader
   now reads zeros, which are stale tags and therefore silence, and writes to
   private pages. If this fails, the old mapping stays;
3. every 10 ms checks the reader count, and frees (unmaps) the old
   attachment once a check after the swap has seen no reader *and* at least
   1 s has passed since the swap.

A reader that still holds the old attachment incremented the count before
its load, and its load came before the swap, so a count of 0 seen after the
swap proves it has finished. The placeholder mapping and the grace period are
defence in depth. `attach_stress.rs` runs 100,000 swaps against an IO thread;
on macOS, `xpc_in_process.rs` neutralizes a real `xpc_shmem` mapping.

## Testing

| Tier | Where | What |
|---|---|---|
| Unit and integration tests | `cargo test --workspace` on Linux (`ci.yml`) and macOS (`macos.yml`, job `check`); Windows is not tested for now | Everything but Apple's code. Among them: the layout, seqlock, sample and timeline tests of `ovsc-shm`; the real servo replayed into the timeline (`ovsc-clock/tests/timeline_replay.rs`); the codec, transports and a forked cross-process test of `ovsc-ipc`; the driver through its C vtable with a fake host (`ovsc-hal/tests`); and `ovsc-hal-server/tests/fullstack.rs`, a real engine on 127.0.0.1 with the driver rlib driven by a simulated HAL. |
| Lint | `ci.yml` on Linux | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `ovsc-shm` built for `thumbv7em-none-eabihf` and `aarch64-unknown-none`, `shellcheck` on the macOS scripts, the plists parsed. |
| macOS cross-build | a Linux machine with the macOS SDK, before pushing | Clippy for `aarch64-apple-darwin` and `build-driver.sh` in cross mode ([Cross-building](#cross-building-from-linux)). Nothing runs. |
| macOS check | `macos.yml`, job `check`, `macos-latest`, on every pull request | Clippy and `cargo test --workspace` on macOS, which adds the XPC tests; `abi_check.c` of the driver and of `coreaudio-check` compiled against the SDK; `build-driver.sh` with its structural checks. |
| macOS end to end | Your own Mac, with [`ci/macos/e2e-local.sh`](../ci/macos/e2e-local.sh) ([below](#running-the-end-to-end-test-on-your-mac)); or `macos.yml`, job `e2e`, run by hand: `macos-latest` and `macos-15` gate, `macos-15-intel` does not | [`ci/macos/e2e.sh`](../ci/macos/e2e.sh), scenarios S1 to S11 below, then [`ci/macos/diagnostics.sh`](../ci/macos/diagnostics.sh). |

Not automated: Dante hardware and Dante Controller.

### Local checks before pushing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Cross-building from Linux

The driver and the daemon cross-check from Linux with a macOS SDK, a clang
wrapper and `rustup target add aarch64-apple-darwin`. The wrapper runs
`clang -target arm64-apple-macos11 -isysroot <SDK> -fuse-ld=lld` with its
arguments. Set:

| Variable | Value |
|---|---|
| `SDKROOT` | the SDK directory, such as `MacOSX14.5.sdk` |
| `CC_aarch64_apple_darwin` | the wrapper |
| `AR_aarch64_apple_darwin` | an `llvm-ar`, such as `llvm-ar-18` |
| `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER` | the wrapper |
| `MACOSX_DEPLOYMENT_TARGET` | `11.0` |

Then:

```sh
cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings
TARGET=aarch64-apple-darwin packaging/macos/build-driver.sh
```

`build-driver.sh` in cross mode compiles `crates/ovsc-hal/abi_check.c`
against the SDK, links the bundle, checks it with `llvm-nm` and
`llvm-objdump`, and skips signing. The binaries cannot run on Linux.
`tools/coreaudio-check` is a separate workspace.

### End-to-end scenarios

`ci/macos/e2e.sh [S1 S2 ...]` runs on a GitHub macOS runner (passwordless
`sudo`, no Dante devices), or on your own Mac through `e2e-local.sh`
([below](#running-the-end-to-end-test-on-your-mac)). It builds everything,
starts a development PTP master at +50 ppm:

```sh
sudo target/release/ovsc ptp-master -i 127.0.0.1 --event-port 10319 --general-port 10320 --rate-ppm 50
```

and installs with `sudo packaging/macos/install.sh --config ci/macos/e2e.toml`.
[`e2e.toml`](../ci/macos/e2e.toml) is a device `odci` on 127.0.0.1, on Dante's
ports plus 20000 (ARC 24440), discovery off, 8 × 8 channels at 48 kHz, 4 ms,
whose receive channel *n* subscribes to its own transmit channel `0n`, so
audio played into the device comes back through the network engine. It allows uids 202 and 0 (for the self-test) and
logs status every 5 s. Every client call in the scenarios has a time limit,
and the device is always selected by its UID.

| ID | Scenario | Pass criteria |
|---|---|---|
| S1 | Load | `coreaudio-check wait org.openvirtualsoundcard.vsc 60`; the helper process runs; the daemon logged `hal: plug-in attached`. |
| S2 | Properties | `check --rate 48000 --inputs 8 --outputs 8 --zts-period 16384 --input-safety 216 --output-safety 55 --output-latency 192 --clock-algorithm raww --element-names 01,02,03,04,05,06,07,08`, then `walk` finds every property consistent. |
| S3 | Self-test | `sudo target/release/ovsc-hal-selftest --clock-timeout 60` passes: welcome, region size and header, heartbeat, clock, engine, and a TX pattern back on RX through the daemon alone, without Core Audio. |
| S4 | Network clock, bit-exact loop | `wait-status ... clock=following 60`; the status's `ppm` within 2 of +50 for 8 s running, within 120 s; then a 20 s `loopback` on 8 channels with a 512-frame buffer: every channel bit-exact at device delay 0, rate +50 ± 3 ppm; `absorbs`, `seed`, `late_out`, `early_in`, `far_out`, `far_in` and `tx_underruns` unchanged between the 25 % and 75 % snapshots; no `coreaudiod` re-anchoring lines. Prints the io_trace. |
| S5 | Buffer sizes | 5 s loopbacks at 32, 128, 1024 and 4096 frames, device delay 0. At 32 and 128 frames, up to 2400 frames and 20 slips per channel may be lost to the runner's scheduling stalls. |
| S6 | Daemon restart under IO | 40 s loopback; `sudo launchctl kickstart -k system/org.openvirtualsoundcard.daemon` at 10 s. One outage of at most 15 s, then bit-exact again at device delay 0, allowing two Core Audio overload skips and 2400 more frames lost to stalls; no timeline jump: `seed` unchanged and no `coreaudiod` re-anchoring lines; `attach` + 1. |
| S7 | Core Audio restart | `sudo killall coreaudiod`; the device returns within 60 s, a new driver instance attaches, a 10 s loopback passes (480 frames may be lost to stalls), the rate is still 48000. |
| S8 | Channel change | Install `e2e-4ch.toml` and restart the daemon: 4 × 4 within 30 s without a `coreaudiod` restart; 5 s loopback (480 frames may be lost to stalls). |
| S9 | Rate change | Install `e2e-96k.toml` and restart the daemon: `wait-rate org.openvirtualsoundcard.vsc 96000 30`; once `ppm` has stayed within 2 of +50 for 8 s, a 5 s loopback at +50 ± 3 ppm (960 frames may be lost to stalls); then back to 48 kHz. |
| S10 | Daemon absent | Once `ppm` has stayed within 2 of +50 for 8 s, `sudo launchctl bootout system/org.openvirtualsoundcard.daemon`: the device stays alive, a 10 s loopback reads exact silence at +50 ± 5 ppm (holdover); after `launchctl bootstrap` it attaches within 30 s and a 5 s loopback passes (480 frames may be lost to stalls). |
| S11 | Package and uninstall | `build-pkg.sh`; `sudo installer -pkg <package> -target /` over the install; the device is present; `uninstall.sh --purge` removes it and the package receipt, and leaves no launchd job file, driver bundle or `/Library/Application Support/OpenVirtualSoundcard`. |

The GitHub macOS runners are small VMs whose scheduling stalls sometimes
outlast the 4 ms network latency or a Core Audio cycle; the daemon then logs
late packets and the driver `late_out`, or Core Audio skips IO. The loss
allowances above cover only that, around daemon and Core Audio restarts and
at tiny buffers. Audio that arrives must always be at device delay 0; the one
exception is data exactly a zero time stamp period old while Core Audio skips
IO, which is its own IO buffer left from an earlier lap, not a misplaced
ring. On real hardware none of this should happen.

Each scenario prints PASS or FAIL in its own `::group::`, and the last line is
`E2E-SUMMARY os=<version> arch=<arch> S1=PASS ...`. The `e2e` job runs only
when the workflow is started by hand (Actions > macOS > Run workflow, or
`workflow_dispatch`), since Mac runner minutes are billed at ten times
Linux's; its `scenarios` input runs a subset.

### Running the end-to-end test on your Mac

You need the Xcode command-line tools (`xcode-select --install`), Rust
([rustup.rs](https://rustup.rs)) and an administrator account. Quit anything
playing or recording audio. Dante software (Dante Virtual Soundcard, Dante
Via, Dante Controller) can keep running: the test device uses Dante's ports
plus 20000. From the repository root:

```sh
ci/macos/e2e-local.sh
```

That runs every scenario; to run some, name them: `ci/macos/e2e-local.sh S1 S2 S4`.

It says what it will change and asks before going ahead (`--yes` skips the
question), then asks for your password once and keeps `sudo` fresh while
`e2e.sh` runs. While it runs:

* it installs the driver and the daemon with the test configuration, which
  talks only to this Mac (127.0.0.1), and a test PTP master on 127.0.0.1;
* Core Audio restarts several times, and all sound stops for a few seconds
  each time;
* the first time, macOS asks whether your terminal app may use the
  microphone. Allow it: without that permission macOS records silence, and
  the loopback scenarios fail with `every input channel was silent`. If you
  missed the question, turn it on in System Settings > Privacy & Security >
  Microphone and run again;
* S11 ends by uninstalling OpenVirtualSoundcard with `--purge`. If OpenVirtualSoundcard was
  installed before, its configuration is copied to
  `target/e2e/ovsc.toml.before` first.

The output and the diagnostics go to `target/e2e/run-<date>.log`; the last
lines print the `E2E-SUMMARY` and the log's path. If the run stops before
S11, OpenVirtualSoundcard stays installed, and the script prints how to uninstall it.

A Mac of your own is not a GitHub runner: SIP is normally on, which the
driver handles ([Gatekeeper and signing](#gatekeeper-and-signing)), and
scheduling stalls are rarer than on the runners' small VMs. On an M1 Mac
every loopback was bit-exact, at every buffer size from 32 to 4096 frames.

### Reading CI logs

Nothing is uploaded: everything is in the job logs. Besides the Actions page,
the GitHub MCP server's `get_job_logs` reads them, by `job_id` for one job or
by `run_id` with `failed_only` for every failed job of a run; `return_content`
returns the text and `tail_lines` (500 by default) how much of the end.
`actions_list` with `list_workflow_jobs` gives the job ids. Search for
`E2E-SUMMARY`, `: FAIL (`, `CA-RESULT` (the last line of each loopback),
`SELFTEST-SUMMARY` and `hal: io_trace`.

### Measured IO timing

The IO trace records, for the first 64 IO operations of each session, the
sample times Core Audio passes the driver: the current time and the input
and output times. Their differences show where the HAL places input and
output relative to the current time. The table takes them from the io_trace
lines that S4 prints (48 kHz, 512-frame buffer, input safety offset 216,
output safety offset 55).

<!-- io_trace: filled from the e2e S-scenario logs -->
| Mac | current − input (frames) | output − current (frames) |
|---|---|---|
| M1, macOS 26.6.2, SIP on (local run) | 729 to 731 | 564 to 566 |
| `macos-latest` (arm64) | pending | pending |
| `macos-15` (arm64) | pending | pending |
| `macos-15-intel` | pending | pending |

So the HAL reads input one buffer plus the input safety offset behind the
cycle boundary, and writes output one buffer plus the output safety offset
ahead of it (512 + 216 = 728 and 512 + 55 = 567), the current time it passes
being 1 to 3 frames after that boundary: the safety offsets ([Latency and
safety offsets](#latency-and-safety-offsets)) are honoured as declared.

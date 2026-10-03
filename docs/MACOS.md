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
| `/etc/newsyslog.d/org.openvirtualsoundcard.conf` | Rotates the log at 1 MiB, keeping 5 old files. The running daemon keeps writing to the rotated `ovsc.log.0` until it restarts. |

The launchd job runs `ovsc run --config "/Library/Application Support/OpenVirtualSoundcard/ovsc.toml"`
as root, at boot and again whenever it exits (`RunAtLoad`, `KeepAlive`,
`ThrottleInterval` 5 s). It holds the Mach service `org.openvirtualsoundcard.audio`
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
| `[device] latency_ms` | The receive latency, 0.25 to 40 ms. |
| `[clock] source` | `"ptp"` follows the network's PTP master; `"free"` runs on this Mac's own clock, for a network without a master. `"system"` is refused. |

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
10 to 20 seconds. Remember that devices only exchange audio at the same
sample rate.

#### Setting the latency from Dante Controller or the app

Dante Controller sets the receive latency in Device View, under Device Config
> Latency (1 to 40 ms there); the [OpenVirtualSoundcard app](#the-openvirtualsoundcard-app) sets it
too. Its Latency tab shows how late the packets of each receive flow arrive,
against that setting. Like a rename, the new latency is saved in the device's state file
(`state.toml`, as `latency_ns`) and from then on replaces
`latency_ms`, also after a reboot. Deleting `latency_ns` from the state file
(then restarting the daemon) goes back to `latency_ms`.

To apply it, the daemon restarts its network engine while the clock keeps
running and the driver stays attached: subscriptions drop for about a second
and come back, and Core Audio sees a configuration change with the new
latency and safety offsets. The log says `restarting the network engine for a
receive latency of ... ms`.

The `[coreaudio]` section tunes the device:

| Key | Default | Meaning |
|---|---|---|
| `control_socket` | `"/var/run/ovsc/control.sock"` | The [control socket](#the-control-socket) for the app; empty for none. |
| `allowed_uids` | `[202]` | Effective user IDs allowed to connect; 202 is `_coreaudiod`, Core Audio's driver helper. |
| `input_margin_us` | `500` | Added to the input safety offset. |
| `output_margin_us` | `1000` | Added to the output safety offset. |
| `output_latency_ms` | unset | Output latency reported to applications; unset means `latency_ms`. |
| `input_latency_mode` | `"safety"` | `"safety"` or `"latency"`: see [Latency and safety offsets](#latency-and-safety-offsets). |
| `clock_algorithm` | `"raw"` | Core Audio's time stamp smoothing: `"raw"` (none) or `"iir"`. |
| `prevent_idle_sleep` | `true` | Keep the Mac from idle-sleeping while the engine runs. |
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
  latency), the clock (its leader, offset, network delay and rate), the
  receive channels with what each one is subscribed to, the transmit flows
  and the packet counters. Warnings from the daemon, such as a clock that
  does not lock, come first.
* **Settings** changes the device name, the network interface, the sample
  rate, the bit depth, the receive and transmit channel counts and the
  latency. **Apply** sends only what changed. A new name applies at once,
  and a new latency restarts the network engine (audio stops for a second
  or two, see
  [above](#setting-the-latency-from-dante-controller-or-the-app)). A new
  interface, rate, bit depth or channel count is written into
  `ovsc.toml`, keeping its comments, and restarts the whole daemon:
  the app asks first, since audio then stops for 10 to 20 seconds while the
  clock locks again.

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
| `{"op":"settings"}` | `"result":"settings"`: `name`, `interface`, `sample_rate`, `bits_per_sample`, `rx_channels`, `tx_channels`, `latency_ms`. |
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
* `guard = floor(tx_guard_us × fs / 1e6)`: a packet is sent this long after
  its timestamp (`tx_guard_us` is 500 µs by default);
* `FPP_MAX = 32`, the most frames per transmitted packet;
* `margin(us) = ceil(us × fs / 1e6)`.

| Property | `"safety"` mode (default) | `"latency"` mode |
|---|---|---|
| Input safety offset | `L + margin(input_margin_us)` | `margin(input_margin_us)` |
| Input latency | 0 | `L` |
| Output safety offset | `max(FPP_MAX − 1 − guard, 0) + margin(output_margin_us)` | same |
| Output latency | `L`, or `output_latency_ms` | same |

In safety mode the network latency is part of the input safety offset, so
the input time stamps an application gets are the senders' capture times:
recorded audio lines up with the network's timestamps. In latency mode the
driver reads `L` frames further back and reports `L` as input latency, for
applications that compensate input latency themselves. A host that adds both
values sees the same input total in either mode.

The output safety offset covers the transmit thread: a packet carries up to
`FPP_MAX` frames and leaves `guard` after its first frame's timestamp, so a
frame must be in the ring up to `FPP_MAX − 1 − guard` frames before its own
time. Dante receivers play a frame at its timestamp plus their own receive
latency; the device reports `latency_ms` as its output latency unless
`output_latency_ms` says otherwise.

With the default margins, 4 ms latency and the default guard:

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

## Clock states and silence

The daemon's clock is in one of these states:

| State | Meaning |
|---|---|
| Unlocked | No master heard yet, or the master went silent. After losing a master the clock keeps running on its last frequency for 30 s (holdover), then becomes invalid. |
| Locking | A master was found; the servo is converging. |
| Locked | Following the master. |
| FreeRunning | `clock.source = "free"`. |

The driver keeps its own continuous timeline for Core Audio and follows the
daemon's clock when it is Locked or FreeRunning ([Device timeline](#device-timeline)).
The status shows the driver's side as `clock=synthetic` (it has not followed
a daemon clock yet), `clock=following` or `clock=holdover` (it lost the
daemon's clock and keeps going at the last rate).

Audio flows only while the driver's **gate** is open (`gate=1`). The gate is
open when all of these hold:

* the driver has the daemon's region (`daemon=attached`);
* the daemon's engine runs at the device's sample rate, and its heartbeat is
  less than 1 s old;
* the daemon's clock is valid and within 1000 ppm of nominal;
* the driver's timeline follows this clock, or continues it in holdover: a
  new daemon's clock, or the same clock after a step, is shut out until the
  timeline follows it (so never while `clock=synthetic`);
* the daemon's clock agrees with the driver's timeline to within 250 µs.

Otherwise the inputs read silence and the outputs are dropped. So:

* before the daemon's clock first locks, the device is silent;
* when the master goes away (no Sync for 5 s), the daemon's clock holds over
  for 30 s on its last mapping, and audio keeps flowing on it while the two
  clocks agree. When a master is heard again the daemon locks to it and the
  driver realigns once; if the holdover runs out first, the device is silent
  until then;
* when the clock steps, the driver realigns once: the audio slips once, but
  the timeline Core Audio sees stays continuous. A new master silences the
  device until the daemon's clock has locked to it, then the driver realigns
  the same way;
* when the daemon restarts, audio stops until the new daemon's clock locks.
  Core Audio sees no change: no reconfiguration, no new timeline;
* when the daemon is stopped, the device stays present and silent.

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

launchd opens this file when it starts the daemon, and the daemon never
reopens it. After newsyslog rotates the log at 1 MiB, the running daemon
keeps writing to `ovsc.log.0` until it restarts. If `ovsc.log` is
empty or stale, read `ovsc.log.0`, or restart the daemon with
`sudo launchctl kickstart -k system/org.openvirtualsoundcard.daemon`.

Lines worth knowing:

* `hal: serving org.openvirtualsoundcard.audio (region generation <hex>, 67174400 bytes)`
  at start;
* `hal: plug-in attached: pid <helper>, ...` when a driver instance connects;
* `clock locked to ...` and `clock lost`;
* every `status_log_interval_s`: `clock <state>; receiving <n>/<m>
  subscribed channels; <k> transmit flows; packets: tx <n> (<u> underruns),
  rx <n> (<l> late); hal: peers=... io=... gate=... regime=... absorbs=...
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
clock becoming valid, a grandmaster other than the last one, or a change into
Locked or FreeRunning. Losing the master is not one: the clock holds over on
the same mapping.

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
* The timeline follows the daemon's clock only while it is valid, Locked or
  FreeRunning and within 1000 ppm, the daemon's engine runs at the device's
  rate, and its heartbeat is under 1 s old. Otherwise it holds over at its
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

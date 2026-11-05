# Live use: vocal processing through OpenVirtualSoundcard

This guide is for one job: a console or stagebox sends vocal channels over
Dante to a Mac, a plug-in host processes them at a small buffer, and the
processed vocals go back over Dante to the console for in-ears and PA. It
covers the settings, the network, a pre-show check, and what happens when
something fails. [`MACOS.md`](MACOS.md) has the full reference.

> ⚠️ **Read this first.** OpenVirtualSoundcard has run on one real Dante
> network so far. Everything in this guide is backed by tests on Linux and,
> for the Core Audio driver, by tests with a simulated host. None of the
> behaviour below has yet been confirmed with your console, your switches or
> a show-length run on a Mac. Before you trust it with a show:
>
> * do a full dress rehearsal on the show's network, with the show's
>   session, for at least as long as the show;
> * work through the [pre-show check](#pre-show-check) on that rig;
> * keep a fallback that does not go through the Mac: the console's own
>   vocal processing, or the dry channels on a fader you can bring up.
>
> The things that have **not** been verified on hardware are listed under
> [What is not verified yet](#what-is-not-verified-yet).

## The signal path

```text
mic → stagebox ─Dante→ console ─Dante (direct outs)→ OVSC inputs → plug-in host
                                                                       │
console (IEM mixes, PA) ←Dante── OVSC outputs ←────────────────────────┘
```

The Mac runs two parts: the daemon, which is the Dante device on the network
and follows the network's PTP clock, and a Core Audio driver that shows the
device to applications as **Open Virtual Soundcard**. Core Audio runs on the
network's clock, so nothing is resampled.

## Latency

### The budget

A frame captured at the stagebox reaches the console's outputs

```text
2·B + L + margin_in + 7 + margin_out + max(console latency, T)
```

later, in frames at 48 kHz, plus the plug-ins' own latency and the
converters and processing in the stagebox and console. Here:

| Term | What it is | Default |
|---|---|---|
| `B` | The plug-in host's buffer | (your choice) |
| `L` | OVSC's receive latency, `latency_ms` | 4 ms |
| `margin_in` | `input_margin_us` | 500 µs (24 frames) |
| `7` | The transmit thread's packet lead: 32 frames per packet, sent 0.5 ms (24 frames) after their time | fixed |
| `margin_out` | `output_margin_us` | 1000 µs (48 frames) |
| `T` | OVSC's transmit latency, `tx_latency_ms` | `latency_ms` |
| console latency | The receive latency the console uses for OVSC's returns | (console setting) |

At 48 kHz with the default margins and a console at 1 ms:

| `latency_ms` | `tx_latency_ms` | B = 32 | B = 64 | B = 128 |
|---|---|---|---|---|
| 4 (default) | unset (default) | 11.0 ms | 12.3 ms | 15.0 ms |
| 4 | 1 | 8.0 ms | 9.3 ms | 12.0 ms |
| 2 | 1 | 6.0 ms | 7.3 ms | 10.0 ms |
| **1** | **1** | **5.0 ms** | **6.3 ms** | **9.0 ms** |

Each millisecond of `latency_ms` adds a millisecond. So does each
millisecond of `T` above the console's latency, and each 100 µs of margin
adds 100 µs.

**Set `tx_latency_ms = 1.0` for live use.** Without it, OVSC asks the
console to play its returns at `latency_ms`, and with the default of 4 ms the
returns wait 3 ms longer than they need to. The packaged configuration
leaves it unset to keep the behaviour of earlier versions.

### What the host is told

The device reports the input side (`L` plus the input margin) as the input
safety offset and `T` as the output latency, so a host that adds up its
buffers and the device's figures shows the round trip above. If the console
plays OVSC's returns at more than `T` (its receive latency for them is
higher), set `output_latency_ms` to the console's latency, or the host
under-reports by the difference. [MACOS.md, "Latency and safety
offsets"](MACOS.md#latency-and-safety-offsets) has the details.

### Choosing `latency_ms`

`latency_ms` is how long OVSC waits for the console's packets. Too short and
packets arrive after their time; that is a dropout on that channel.

* Start at **2 ms**. Run the show's session for at least an hour and watch
  the receive health: late and lost packets must stay at 0.
* Then try **1 ms** the same way. Keep it only if it stays clean on the
  show's network, with the show's traffic.
* A flow from a transmitter that asks for more latency than OVSC's setting
  (Dante Virtual Soundcard, Dante Via, or a console set higher) plays at the
  transmitter's latency, as Dante receivers do. Its channels then arrive
  later than the others, and the log says so for each flow. The host is not
  told: its input figures still assume `latency_ms`, so it under-reports
  those channels by the difference. `ovsc discover --channels` lists the
  latency each transmit channel asks for. **Set `latency_ms` to at least the
  latency the console's channels ask for**: lower gains nothing for them.
  Keep software transmitters off the vocal path.

### Margins and buffer size

* Use a **64-frame** host buffer. Try 32 frames only after a clean
  hour-long run at 64, and only if the Mac and session have headroom.
* Leave the margins at their defaults unless you have measured. To lower
  them, run the show's load for an hour and read the smallest margins in the
  status (`min_out_margin`, `min_in_margin`); [MACOS.md, "Choosing the
  margins"](MACOS.md#choosing-the-margins) says how far each may come down.
  Never go below about 300 µs.

## Configuration

Set these keys in the installed
`/Library/Application Support/OpenVirtualSoundcard/ovsc.toml`, keeping the
rest of the file (in particular `[device.ports]`, which moves two ports out of
the way of Audinate's ConMon service), then
`sudo launchctl kickstart -k system/org.openvirtualsoundcard.daemon`:

```toml
[device]
name = "vox-mac"          # unique on the network; no "-2" suffix in Dante Controller
interface = "en7"         # the Dante NIC by name, never "" on a show Mac
sample_rate = 48000       # the console's rate
bits_per_sample = 24
rx_channels = 8           # vocal inputs
tx_channels = 8           # processed returns
latency_ms = 2.0          # 1.0 once a long test run is clean
tx_latency_ms = 1.0       # returns as early as the console allows
state_file = "/Library/Application Support/OpenVirtualSoundcard/state.toml"

[device.ports]
cmc = 38800               # as installed: next to Dante Controller's ConMon
settings = 38700

[clock]
source = "ptp"

[audio]
backend = "coreaudio"

[coreaudio]
# input_margin_us = 500
# output_margin_us = 1000
# output_latency_ms = ...  only if the console's receive latency for the
#                          returns is above tx_latency_ms
# prevent_idle_sleep = true
```

Every value is checked at start. A latency below the minimum (1 ms with
the default transmit guard) is raised to it, with a warning in the log.

**The app and controllers win over this file.** A receive latency, device
name or show lock set from the app or from Dante Controller, and the
automatic `-2` rename, are saved in `state.toml` and override `ovsc.toml` at
every start. Change those in the app; to go back to the file's values, stop
the daemon and delete `latency_ns`, `name` or `lock_remote_settings` from
`state.toml`.

### The show lock

Once the latency, sample rate, encoding and names are set at soundcheck, turn
on the show lock in the app (Settings, "Lock settings from the network"). It
applies at once and interrupts nothing. While it is on, Dante Controller and
other controllers cannot change OVSC's latency, sample rate, encoding, device
name or channel names; routing stays open, and changes made in the app still
apply. Each refused attempt is logged and shown in the app with the
controller's address. The lock is saved and survives restarts.

The lock matters because some of these changes interrupt audio
([below](#changes-that-interrupt-audio)), and a controller elsewhere on the
network can make them without anyone at the Mac knowing.

## The network

* Use a wired Gigabit interface for Dante, named in `interface`. Turn Wi-Fi
  off on the show Mac, or at least keep it off the Dante subnet.
* Configure the switches for Dante as Audinate recommends: QoS with strict
  priority, **DSCP 56 (CS7) highest** for PTP and **DSCP 46 (EF) next** for
  audio; Energy Efficient Ethernet (EEE) off. OVSC marks its audio packets
  EF and its PTP packets CS7. Repeated clock unlocks are the typical sign of
  missing PTP priority or a congested 100 Mbit link.
* Set the console (or another hardware device) as **Preferred Leader** in
  Dante Controller. OVSC only follows the clock; it never leads.
* Give the Mac a fixed address or a DHCP reservation. An address change
  restarts the network engine (a few seconds of silence, see below).
* A computer that transmits to OVSC (another Mac, Dante Virtual Soundcard)
  must have only one interface on the Dante subnet: OVSC accepts a flow's
  audio only from the address it requested the flow from.
* Run no other Dante software on the show Mac. OVSC moves its ports out of
  the way of Audinate's ConMon service, but Dante Virtual Soundcard and OVSC
  must not both carry the show.

## The Mac

* Sleep: OVSC holds an idle-sleep assertion while the daemon runs. On a show
  Mac also run `sudo pmset -a sleep 0 disablesleep 1`, and check with
  `pmset -g assertions`.
* Turn off what can stall the Mac mid-show: automatic updates, Time Machine,
  Spotlight indexing of external drives, and background apps.
* Set the plug-in host to the device **Open Virtual Soundcard** for both
  input and output, at the console's sample rate.
* Put a true-peak limiter last in every chain that feeds in-ears or the PA.
  OVSC has no ceiling of its own: finite over-range output hard-clips at full
  scale. A plug-in that outputs NaN or infinity is sent as silence on that
  channel (and counted), not as full-scale noise.

## Patching

* Console → OVSC: patch the vocal channels' Dante outputs (direct outs) to
  OVSC's receive channels.
* OVSC → console: patch OVSC's transmit channels to the console inputs that
  feed the IEM mixes and PA. Patch them by **factory channel name** (`01`,
  `02`, ...): OVSC's transmit channels always answer to those, so a relabel
  does not break the patch.
* Set the console's receive latency for OVSC's returns as low as the console
  allows (1 ms), and check the console reports no late packets from OVSC.
* Do not rename the device after patching. Other devices subscribe by
  `channel@device`; a rename breaks every subscription to OVSC's outputs
  until they are patched again. The app asks before a rename and names the
  devices receiving from OVSC.

## Pre-show check

With the show session running at the show's buffer size:

1. **Name.** Dante Controller shows the device under its intended name, with
   no `-2` suffix, and the app shows no warning that another device uses the
   name. If another device already used the name, OVSC took `<name>-2` at
   start and logged an error: rename one of them and re-patch.
2. **Clock.** The app's Clock is **Locked**, its Leader is the console (or
   the Preferred Leader), and Dante Controller shows OVSC as a follower of
   the same leader. No other leader is reported.
3. **Status.** The app's Audio light is green **Flowing**, the Status tab
   shows no warnings, and every subscribed receive channel reads
   **receiving**. The log's status line reads `healthy` (before its
   `hal:` part).
4. **Returns.** Each return arrives at the console: check its meters, and that
   the console shows OVSC's flows without late packets.
5. **Real-time.** The log has no line saying a thread "is not real-time"
   (`grep -i 'real-time' /Library/Logs/OpenVirtualSoundcard/ovsc.log`), and
   the app shows no warning about it.
6. **A long run.** Run the session for at least as long as the show (a dress
   rehearsal is ideal). Afterwards the log must have no `health:` warning
   from the run (`grep health: /Library/Logs/OpenVirtualSoundcard/ovsc.log*`). In the last status line, the receive `late` count and the
   driver's `late_out`, `early_in` and `tx_underruns` must not have moved
   while the host played, and `min_out_margin` and `min_in_margin` must stay
   above 12 frames (250 µs at 48 kHz).
7. **Show lock.** Turn it on (Settings).
8. **Fallback.** Know how you will get the dry vocals to the IEMs and PA
   without the Mac, and test it once.

## During the show

Do not change, on OVSC or in Dante Controller:

* the receive latency;
* the sample rate, encoding or channel counts;
* the device name or interface.

The show lock refuses the first group from controllers; the app asks before
any of them.

### Changes that interrupt audio

| Change | What happens |
|---|---|
| Receive latency (app or a controller) | The network engine restarts. Inputs return after about 1 to 2 s. Every return flow to the console closes, and each receiver requests it again on its own timing, often several seconds. Core Audio sees a configuration change, and some hosts restart their audio engine. |
| Sample rate, encoding, channel counts, interface | The daemon restarts: 10 to 20 s of silence both ways. |
| Device rename | Every subscription to OVSC's outputs breaks until it is patched again. |
| Clock leader change | Nothing audible when the new leader was synchronised to the old one, the normal case (see below). |

## What happens when something fails

| Event | What you hear | What OVSC does |
|---|---|---|
| The Dante leader changes (reboot, new Preferred Leader, a better device joins) | Nothing | The clock stays continuous. The app shows Locking (amber) for about 2 s while it checks the new leader. |
| The leader goes silent and another device takes over | Nothing, if the new leader was synchronised to the old one | The new leader is followed once the old one has been silent for 1 s and the new one has sent two Syncs. |
| No leader at all | Nothing for 30 s | After 5 s without Syncs the clock holds over on its frequency for 30 s and audio keeps flowing. If a leader returns within the holdover and agrees within 250 µs, nothing is heard; if it differs by more, see the next row. If the holdover runs out, silence until a leader is heard and the clock locks. |
| A leader whose time differs by more than 250 µs (or a jump of more than 2 ms) | Silence, 1.6 s typically, up to about 5.5 s, then one slip | The clock steps once and relocks. Core Audio's timeline stays continuous; the host is not disturbed. |
| The leader's time jumps by 0.25 to 2 ms, same leader | Possibly dropouts on the returns, for up to about 15 s | The clock slews back at 0.1 ms per second instead of stepping, so audio keeps flowing, but the console may find returns late until the error is below its latency slack. Rare: it needs the leader's own time to jump (for example a leader re-syncing to an external reference). |
| Sync packets delayed by congestion | Nothing | The clock may unlock; it keeps its frequency and slews back gently. Repeated unlocks mean the network needs PTP priority. |
| Cable pulled and put back (same address) | Silence while unplugged, then audio returns | Receive flows that deliver nothing for 3 s are requested again; audio usually returns well under 2 s after the link does. The console requests its flows from OVSC again on its own timing. |
| The Mac's address changes | A few seconds of silence; the returns may take longer | About 2 s after the change the network engine restarts on the new address; inputs return when the clock locks. The returns come back once the console requests OVSC's flows again, on its own timing (often several seconds, more while it finds the new address): test this with your console. With `interface = ""` it could pick Wi-Fi: name the interface. |
| The console reboots | Silence until the console is back, and a few seconds more | OVSC stops the dead flows and requests them again. An unanswered request (after a 2 s wait) is retried within 1 s; a console that refuses while it boots is retried after 1 s, backing off to 5 s; one whose records are gone is looked up again every 2 to 4 s. Inputs can return several seconds after the console is ready. |
| A packet is lost | A gap of one packet (under 1 ms at 48 kHz) on that flow's channels | Counted per flow; the flow is marked degraded for 5 s. |
| The plug-in host overloads (misses its deadline) | A click | Counted by the driver as late output cycles. |
| A plug-in outputs NaN or infinity | Silence on that channel | Counted as non-finite output. It is a plug-in fault. |
| The transmit thread stalls (rare: the Mac is overloaded) | A gap on the returns about as long as the stall beyond the console's slack (about 0.5 ms at a 1 ms console latency) | Packets up to 40 ms overdue are sent at once, oldest first, for receivers with longer latencies; beyond 40 ms the flow resyncs and the skipped packets are counted. Late sends and skips are warnings in the app. |
| The daemon crashes (a panic anywhere) | 5 to 10 s of silence plus the clock relock; the returns may take longer | It logs the panic, says goodbye to the driver and exits; launchd starts a new daemon. Core Audio sees no change. The returns come back once the console requests OVSC's flows again, on its own timing. |
| The Core Audio driver faults (a caught panic) | Silence on every channel until Core Audio restarts | Run `sudo killall coreaudiod`; the host will likely need to reopen the device. |
| Another device uses OVSC's name mid-show | Nothing at once; new subscriptions may reach the wrong device | OVSC keeps its name, through engine restarts too (latency, address change); it logs an error and the app warns. Rename one device after the show. Only a restart of the daemon itself (a crash, a reboot) probes the name again and, while the other device still has it, takes `<name>-2`: clear any name warning before the show. |
| A controller changes a setting | See [Changes that interrupt audio](#changes-that-interrupt-audio) | With the show lock on, refused and listed in the app; without it, applied and listed. |
| The audio engine starts or the gate opens or closes | A 5 ms fade, not a click | The driver ramps in and out at its gate. |

## Health and warnings

The app's header has three lights. **Audio** is the one to watch:

| Audio light | Meaning | What to do |
|---|---|---|
| green **Flowing** | Audio can flow, and nothing audible went wrong in the last 30 s. | Nothing. |
| amber **Dropouts** | Audio flows, but something audible happened in the last 30 s. | Read the warnings: they say what and what to do. |
| amber **Stopped** | The driver holds the audio back: inputs and returns are silent. Usually for seconds while the clock settles or the engine restarts. | If it lasts, read the warnings (clock, engine). |
| red **Silent**, with Driver red **Failed** | The Core Audio driver hit an internal error and is silent until Core Audio restarts. | Run `sudo killall coreaudiod` (the app has a button to copy it), then restart the host's audio. All audio on the Mac stops for a few seconds. |

The Status tab lists warnings in plain words, red ones first. A red warning
means audio is down now in at least one direction: a failed driver, Core
Audio no longer calling the driver, the driver's gate closed for 2 s while
an application plays, a stopped transmit engine (the daemon then restarts
itself), a clock that has not locked, or a clock that lost its leader after
the holdover. Amber warnings follow:

* **Counters that rose** in the last 30 s, with how many and what to do:
  lost or late received packets (from the first one), early packets and
  datagrams from a stray sender (from 5), transmit send errors (with the
  system's error), late sends (from 5, with the worst lateness), skipped
  packets and restarts of a flow's timing, expired flows, late output or
  early input cycles, a margin run out, slips, invalid samples (NaN or
  infinity) from the host, and packets sent without the host's audio while
  it plays. A warning stays 30 s after its counter last rose. The first 10 s
  after the engine starts and the first 3 s after the host starts its audio
  do not count.
* **States while they hold**: the clock in holdover, with the seconds left
  before audio stops; the clock relocking; a jump of the clock; a new clock
  leader; subscribed channels without audio for 10 s; a transmit thread
  without real-time priority; another device using this one's name.
* **Controllers' changes** for 10 minutes, then under **Earlier** at the
  bottom.

The same warnings reach the log at
`/Library/Logs/OpenVirtualSoundcard/ovsc.log`: `health: <warning>` (WARN)
when one appears and `health: cleared: <what>` (INFO) once it has been gone
10 s. The status line every 30 s reads `healthy`, or `warnings:` and a
short list, before its `hal:` part. A script on the stage computer can poll the control socket
(`{"op":"status"}` on `/var/run/ovsc/control.sock`) and alarm when
`urgent_warnings` is above 0 or `driver.faulted` is true.

What the common warnings point at:

* **Late received packets**: `latency_ms` is too short for the network.
  Raise it after the show (the change interrupts audio).
* **Lost received packets**: the network: cabling, a switch port, or QoS.
* **Early packets**: a transmitter's clock disagrees with OVSC's: check that
  both follow the same leader.
* **Datagrams from a stray sender**: packets reached a flow that do not
  belong to it, usually a stale flow after a restart; it expires on its own
  within 4 s.
* **Send errors**: the Mac's network interface, or a receiver that went
  away.
* **Late output or early input cycles, a margin run out**: the host missed
  its deadlines. Raise the buffer, or lighten the session.
* **No real-time priority**: should not happen on macOS; restart the daemon
  after the show.

## Changes in this version that affect live use

* Returns have their own latency (`tx_latency_ms`), and the device reports it
  to Core Audio as its output latency.
* Latencies below 1 ms are refused from the app and controllers, and raised
  to 1 ms in `ovsc.toml` (packets leave 0.5 ms after their time).
* Leader changes, holdover and unlocks no longer interrupt audio in the
  normal case (see the table above).
* A flow plays at the larger of OVSC's latency and the transmitter's, as on
  Dante hardware.
* Receive flows read the packet size from the packet, as Dante receivers do,
  and accept packets only from the transmitter's address and the port its
  audio comes from.
* Recovery from a pulled cable or a rebooted console no longer waits on
  other transmitters, and failures show as Dante statuses
  (`NO_CONNECTION`, `RX_FAIL`, `CHANNEL_FORMAT`, `NO_RX`, `CHANNEL_LATENCY`).
* The device name is probed at start; a taken name gets `-2`.
* The show lock, and a list of controllers' changes in the app.
* The daemon follows its network interface, restarts itself after any panic,
  keeps its state file safe across power cuts, and keeps the Mac awake from
  start to shutdown.
* Audio packets are marked DSCP EF and PTP packets CS7.
* The driver fades at its gate, sends silence for NaN or infinity, and old
  audio can no longer replay a day later.

## What is not verified yet

These rest on code reading, Linux tests and simulation, not on hardware:

* Interoperation with your console: flow requests, packet sizes, the
  `fpp` range (OVSC accepts 2 to 32 frames per packet and 1 to 8 channels
  per flow from receivers), the new subscription statuses, refused
  controller writes while locked, and a flow re-requested with another
  packet size.
* That macOS keeps the DSCP marking on the wire (check with Wireshark).
* The clock figures (leader change, holdover accuracy, relock times) are
  from simulation and a replay of the real servo, not from a Mac on a Dante
  network.
* The 0.5 ms left for the transmit thread's wake-up and network transit at
  `tx_latency_ms = 1.0`: watch the console's late-packet counters for OVSC's
  returns.
* The interface watcher on macOS (a USB adapter re-attached, DHCP renewing
  a 169.254 address), the log rotation, and `F_FULLFSYNC` on the state file.
* The driver's gate fades in Core Audio on a Mac.
* The end-to-end macOS test (`ci/macos/e2e.sh`) has not been run on these
  changes yet.

## The sound-card bridge is not for live use

The `soundcard` backend bridges OVSC's channels to another audio device
(an interface, BlackHole) by resampling. It adds a buffer and a margin in
each direction (about 4.3 ms each way at 128 frames), drifts and corrects,
and recovers from device gaps with fades. Use the native Core Audio device
for live work.

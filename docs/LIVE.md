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
  later than the others, and the log says so for each flow. `ovsc discover
  --channels` lists the latency each transmit channel asks for. Keep
  software transmitters off the vocal path.

### Margins and buffer size

* Use a **64-frame** host buffer. Try 32 frames only after a clean
  hour-long run at 64, and only if the Mac and session have headroom.
* Leave the margins at their defaults unless you have measured. To lower
  them, run the show's load for an hour and read the smallest margins in the
  status (`min_out_margin`, `min_in_margin`); [MACOS.md, "Choosing the
  margins"](MACOS.md#choosing-the-margins) says how far each may come down.
  Never go below about 300 µs.

## Configuration

`/Library/Application Support/OpenVirtualSoundcard/ovsc.toml`, then
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

Every value is checked at start. A latency below 1 ms (or below the
transmit guard plus 0.5 ms) is refused with a message saying why, and the
daemon does not start.

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
   no `-2` suffix. If another device already used the name, OVSC took
   `<name>-2` at start and logged an error: rename one of them and re-patch.
2. **Clock.** The app's Clock is **Locked**, its Leader is the console (or
   the Preferred Leader), and Dante Controller shows OVSC as a follower of
   the same leader. No other leader is reported.
3. **Status.** The app shows no warnings, and Audio reads **Flowing**. Every
   subscribed receive channel reads **receiving**.
4. **Returns.** Each return arrives at the console: check its meters, and that
   the console shows OVSC's flows without late packets.
5. **Real-time.** The log has no line saying a thread runs "at normal
   priority" or "not real-time".
6. **A long run.** Run the session for at least as long as the show (a dress
   rehearsal is ideal). Afterwards the counters must not have moved: late,
   lost, early and dropped receive packets; transmit send errors, late and
   skipped packets; the driver's `late_out`, `early_in` and `tx_underruns`
   while the host played; and `min_out_margin` and `min_in_margin` must stay
   above 12 frames (250 µs at 48 kHz). The 30 s status line in
   `/Library/Logs/OpenVirtualSoundcard/ovsc.log` has them all.
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
| The Mac's address changes | A few seconds of silence | About 2 s after the change the network engine restarts on the new address; audio returns when the clock locks. With `interface = ""` it could pick Wi-Fi: name the interface. |
| The console reboots | Silence until the console is back | OVSC stops the dead flows and requests them again; a refused or silent console is retried within 1 s. |
| A packet is lost | A gap of one packet (under 1 ms at 48 kHz) on that flow's channels | Counted per flow; the flow is marked degraded for 5 s. |
| The plug-in host overloads (misses its deadline) | A click | Counted by the driver as late output cycles. |
| A plug-in outputs NaN or infinity | Silence on that channel | Counted as non-finite output. It is a plug-in fault. |
| The transmit thread stalls briefly (up to 40 ms) | Nothing, or a click | Overdue packets are sent at once, oldest first; beyond 40 ms the flow resyncs and the skipped packets are counted. |
| The daemon crashes (a panic anywhere) | 5 to 10 s of silence plus the clock relock | It logs the panic, says goodbye to the driver and exits; launchd starts a new daemon. Core Audio sees no change. |
| The Core Audio driver faults (a caught panic) | Silence on every channel until Core Audio restarts | Run `sudo killall coreaudiod`; the host will likely need to reopen the device. |
| Another device uses OVSC's name mid-show | Nothing at once; new subscriptions may reach the wrong device | OVSC never renames itself mid-show; it logs an error and reports the conflict. Rename one device after the show. |
| A controller changes a setting | See [Changes that interrupt audio](#changes-that-interrupt-audio) | With the show lock on, refused and listed in the app; without it, applied and listed. |
| The audio engine starts or the gate opens or closes | A 5 ms fade, not a click | The driver ramps in and out at its gate. |

## Health and warnings

The app's Status tab lists problems in plain words, and the log at
`/Library/Logs/OpenVirtualSoundcard/ovsc.log` has a status line every 30 s
and a warning when something goes wrong. Warnings about the transmit and
receive paths are logged as they happen, at most every 5 s per kind (every
10 s per receive flow).

Worth knowing:

* **Clock** Locking (amber) for a few seconds after a leader change is normal.
  Holdover means no leader is heard: find out why before the 30 s run out.
* **Late packets** on receive mean `latency_ms` is too short for the network;
  raise it (between songs is not the time: the change interrupts audio).
* **Lost packets** point at the network: cabling, a switch port, or QoS.
* **Early packets** mean a transmitter's clock disagrees with OVSC's: check
  that both follow the same leader.
* **Dropped datagrams** mean packets reached a flow that do not belong to it:
  usually a stale flow after a restart, which expires on its own within 4 s.
* **Transmit send errors** point at the Mac's network interface or a receiver
  that went away.
* **Late output cycles** (driver) mean the host missed deadlines: raise the
  buffer, or lighten the session.
* **Not real-time**: a thread did not get real-time scheduling. On macOS this
  should not happen; restart the daemon.

## Changes in this version that affect live use

* Returns have their own latency (`tx_latency_ms`), and the device reports it
  to Core Audio as its output latency.
* Latencies below 1 ms are refused (packets leave 0.5 ms after their time).
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

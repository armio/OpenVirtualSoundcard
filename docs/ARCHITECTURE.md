# Architecture

OpenVirtualSoundcard is split into small crates with one job each. Protocol knowledge
lives in pure, testable code; timing-critical work runs on dedicated threads;
everything else is async control-plane code.

```
                       ┌──────────────────────────── ovsc (CLI / daemon) ───┐
                       │  config · backends (tone, record, loopback, soundcard)  │
                       └───────────────┬─────────────────────────┬───────────────┘
                                       │ AudioIo (rings + clock) │ Device API
┌──────────────────────────────────────┴─────────────────────────┴─────────────────┐
│ ovsc-core                                                                   │
│                                                                                  │
│  mDNS responder/resolver ── ARC server ── CMC server ── conmon (info, heartbeat) │
│            │                     │  names, subscriptions                         │
│            ▼                     ▼                                               │
│     Directory ◄──── RX manager (reconcile loop) ───► flow-control client         │
│                            │ spawns                                              │
│                            ▼                                                     │
│                  RX flow tasks ──writes──► rx TimedRings ──► backend reads       │
│                                                                                  │
│  flow-control server ──► TX thread (real-time) ◄──reads── tx TimedRings ◄── backend writes │
└──────────────────────────────────────┬───────────────────────────────────────────┘
                                       │ MediaClock (lock-free)
┌──────────────────────────────────────┴──────────────┐   ┌────────────────────────┐
│ ovsc-clock: PTPv1 follower, servo, MediaClock   │   │ ovsc-proto: codecs │
└──────────────────────────────────────────────────────┘   └────────────────────────┘
```

## Crates

| Crate | What it does | I/O? |
|---|---|---|
| `ovsc-proto` | Encoders/decoders for every wire format: ARC, CMC, flow control (DBCP), conmon, media packets, DNS/mDNS, Dante TXT records. | none |
| `ovsc-clock` | `MediaClock` (lock-free read handle), the in-process PTPv1 follower and its servo, a development PTP master, and a system-clock source. | UDP 319/320 |
| `ovsc-core` | The device: discovery, control servers, flows, subscriptions, per-channel ring buffers, persistence. | UDP, threads |
| `ovsc-soundcard` | Bridges the rings to a sound card or virtual cable with drift-compensating resampling. | audio devices |
| `ovsc-shm` | `no_std` definitions shared with the macOS driver: the shared region's layout, rings, clock block, status blocks and the device timeline. | none |
| `ovsc-ipc` | The daemon–driver protocol, the driver configuration and its latency offsets, XPC transports (macOS) and an in-memory one for tests. | XPC |
| `ovsc-hal` | The macOS Core Audio driver (`OpenVirtualSoundcard.driver`), a static library linked into an AudioServerPlugIn bundle. | Core Audio |
| `ovsc-hal-server` | The daemon's side of the driver: the shared region, the clock mirror and the XPC service. | XPC |
| `ovsc` | The `ovsc` binary: daemon (`run`) and controller tools (`discover`, `info`, `route`). | |

## Time

Everything is expressed in **media time**: the PTP master's clock, in
nanoseconds, or in samples (`ns × rate / 1e9`). A media packet carries the
timestamp of its first frame as `(seconds, sample within second)`.
Receivers play the frame stamped `t` at media time `t + latency`.

`ovsc-clock` keeps a `ClockSnapshot`: a linear map from the local
monotonic clock to media time (`media = media_ref + (local − local_ref) ×
rate`). The PTP servo publishes a new snapshot after each measurement. The
map is continuous: corrections re-anchor at "now" and change the rate, never
the current reading, except for explicit steps when the offset is huge.
Readers use a seqlock, so they never block, which matters for audio
callbacks.

## Audio data path

Each channel has a `TimedRing`: a power-of-two array of `AtomicU64` slots.
Each slot packs the low 32 bits of a sample's timestamp with the sample. A
reader asking for timestamp `t` gets the sample only if the tag matches.
Otherwise the slot is stale, from a lost packet or from a lap ago, and reads
as silence. This gives:

* no locks and no blocking between the network and audio sides;
* automatic silence on packet loss, no read/write pointers to keep in sync;
* free fan-out: several flows or channels can write the same ring.

**Receive.** A flow task receives a packet, splits it per slot, and writes
each slot's samples into the ring of every local channel routed to that slot,
at the packet's timestamp. Backends read at `now − latency`.

**Transmit.** Backends write at `now + lead`. The transmit thread wakes when
the next packet is due. It reads `fpp` frames per channel at the packet
timestamp, sends packet `t` at media time `t + guard` (0.5 ms by default),
and drains keepalives. Flows without keepalives for 4 s expire.

## Control plane

* **Names and subscriptions** live in a `State` behind a mutex. Changes made
  by controllers (ARC) or the local API bump a generation counter, which wakes
  the mDNS advertiser, the persistence task and the RX manager.
* **RX manager** is a reconcile loop. It runs on every change, debounced by
  20 ms so that batches share flows, and once a second. It drops routes that
  no longer match, stops dead or empty flows, routes channels into existing
  flows (extending them with DBCP "update" when there is room), and resolves
  and requests new flows for the rest. It then publishes per-channel status
  for ARC.
* **Flow-control server** validates requests (rate, bit depth, channel ids,
  packet size), allocates flow ids and handles, and hands flows to the
  transmit thread.

## System sound card

The daemon reaches applications through backends. Most run inside the
daemon: the sound-card bridge, the recorder and so on. A native sound device
needs a small driver per OS that reads the same rings. The drivers stay
small: they copy samples and report time. Everything with protocol knowledge
stays in the safe, testable Rust daemon.

**macOS** (implemented, not yet tried with Dante hardware; [`MACOS.md`](MACOS.md)
has the details):

* The driver, `OpenVirtualSoundcard.driver`, is a Core Audio server plug-in written in
  Rust (`ovsc-hal`). Core Audio runs it in a helper process as
  `_coreaudiod`. It publishes one device, UID `org.openvirtualsoundcard.vsc`, whose
  inputs are the receive channels and whose outputs are the transmit
  channels.
* The daemon runs as the LaunchDaemon `org.openvirtualsoundcard.daemon` with the
  `coreaudio` backend. It owns the XPC Mach service `org.openvirtualsoundcard.audio`,
  which the driver lists in its Info.plist. XPC carries only control
  messages: a handshake, configurations, and the region.
* The daemon creates one shared region per process (`ovsc-shm`, 64 MiB +
  64 KiB of address space) and hands it to the driver as an `xpc_shmem`. It
  holds the clock block, status blocks, an IO trace and one `TimedRing` per
  channel and direction. The engine's own rings live there, so the receive
  threads write straight into what the driver reads, and the transmit thread
  reads what the driver wrote.
* The clock block mirrors the media clock synchronously on every change.
  The driver turns it into a continuous device timeline plus an integer ring
  offset, so Core Audio runs *on the network clock* without resampling, and
  clock steps or daemon restarts slip the audio once instead of disturbing
  Core Audio's timeline.
* When the daemon restarts, the driver swaps in the new region under running
  IO and retires the old mapping safely: a reader count, an immediate
  `MAP_FIXED` placeholder over the old range, and a grace period before
  unmapping.

**Windows** (planned): a WaveRT virtual audio driver (WDK) plus an ASIO
driver for DAWs, both reading the same region.

**Linux** (planned): a PipeWire node that drives the graph from the media
clock, or an ALSA ioplug plug-in.

## Testing strategy

* Codecs: round trips between encoder and decoder, plus captured bytes from
  real devices as test vectors.
* Clock: a deterministic servo tested with simulated drift and jitter, and a
  real-socket test against the bundled development master.
* Engine: integration tests run two devices on 127.0.0.1 with distinct ports
  and check bit-exact audio, flow sharing and growth, controller routing over
  ARC, and self-subscription.
* macOS driver, everything but Apple's code: the shared layout pinned by a
  golden hash, the seqlock, every 24-bit sample through Float32, timeline
  scenarios and the real servo replayed into the timeline, the protocol and
  transports, a forked cross-process test of the region, the driver through
  its C vtable with a fake host (properties, IO, retire stress, no
  allocation on the real-time paths), and a full stack: a real engine on
  127.0.0.1 with the driver driven by a simulated HAL.

These run in tiers:

| Tier | Where | What |
|---|---|---|
| Unit and integration tests | `cargo test --workspace` on Linux (`.github/workflows/ci.yml`) and macOS (`macos.yml`, job `check`); Windows is not tested for now | All of the above. On macOS this adds the XPC transport tests. |
| Lint | `ci.yml`, Linux | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, a `no_std` build of `ovsc-shm`, `shellcheck` and plist checks of the macOS packaging. |
| macOS cross-build | Linux with the macOS SDK, before pushing | Clippy for `aarch64-apple-darwin` and the driver bundle built in cross mode; nothing runs. |
| macOS check | `.github/workflows/macos.yml`, `macos-latest`, on every pull request | Clippy and tests on macOS, the ABI declarations compiled against the SDK, and the driver bundle built and checked. |
| macOS end to end | A developer's Mac with `ci/macos/e2e-local.sh`, or `macos.yml` run by hand on `macos-latest` and `macos-15` (gating) and `macos-15-intel` | `ci/macos/e2e.sh`: install, then scenarios S1 to S11 against the real Core Audio device with `tools/coreaudio-check`, including a bit-exact loop through the network engine, daemon and Core Audio restarts, channel and rate changes, and package install and uninstall. |

[`MACOS.md`](MACOS.md#testing) lists the scenarios. Not yet automated:
interoperability with Dante hardware and Dante Controller. See
[`ROADMAP.md`](ROADMAP.md).

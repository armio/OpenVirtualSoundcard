# Roadmap

Status markers: ✅ done · 🧪 done, untested against Dante hardware · ⏳ next · 💭 later

## 0.1: Foundation (this release)

- ✅ Protocol codecs (ARC, CMC, DBCP, conmon, media, mDNS), checked against real-device captures
- 🧪 Discovery: advertise `_netaudio-arc/cmc/chan` and resolve channels over mDNS
- 🧪 Control: channel lists, flows, subscriptions, renames and device name over ARC; CMC advertisement; conmon info and heartbeat
- 🧪 Unicast transmit flows (flow-control server, real-time paced sender, keepalive expiry)
- 🧪 Unicast receive flows (resolve, request, grow via update, keepalives, status reporting)
- 🧪 In-process PTPv1 follower (software timestamps), plus a development master
- ✅ CLI: `run`, `discover`, `info`, `route`, `unroute`
- ✅ Backends: tone, WAV recorder, loopback
- 🧪 Sound-card bridge (cpal) with sinc resampling and drift compensation; simulated and run against ALSA's null device, not yet on real audio hardware

## 0.2: Interoperability with real Dante gear

The 0.1 code has only been tested between OpenVirtualSoundcard instances. The first job
is to test against real equipment and fix what breaks.

- 🧪 Test against Dante Controller (Windows/macOS) and at least two hardware devices
  - done on macOS with Dante Controller and an AVIO-DAI2: the device appears, its channels list, the AVIO's channels route to it (several at once since flows are requested with all their slots), and Dante Controller on the same Mac reaches it over 127.0.0.1
  - still to do: routing from OpenVirtualSoundcard to hardware, renames and latency from Dante Controller, a second hardware device, Windows
  - capture each exchange with Wireshark and add the captures as test vectors
- 🧪 PTPv1 follower against a real Dante leader: locks to an AVIO-DAI2 with kernel receive timestamps on macOS; measure lock time, offset and stability, and tune the servo
- ⏳ Answer the open questions in `PROTOCOL.md` §11.2
- ⏳ Measure the minimum stable latency per OS
- ⏳ Sound-card bridge on real hardware and virtual cables (BlackHole, VB-Cable, snd-aloop); compensate device latency; auto-reconnect
- ⏳ Optional hardware timestamping (Linux `SO_TIMESTAMPING`) for lower latency

## 0.3: A real virtual sound card

- ✅ Shared-memory transport (clock block + rings) between the daemon and the macOS driver (`ovsc-shm`, `ovsc-ipc`)
- 🧪 **macOS**: AudioServerPlugIn driver clocked by the media clock, no resampling (`ovsc-hal`, [`MACOS.md`](MACOS.md)); end-to-end tests on GitHub's macOS runners
- 🧪 **macOS**: installer and service setup (`install.sh`, an installer package, a launchd LaunchDaemon, uninstaller)
- ⏳ **Linux**: PipeWire driver node (and/or ALSA ioplug plug-in)
- 💭 **Windows**: WaveRT virtual audio driver + ASIO driver
- ⏳ Installer and service setup on Linux (systemd) and Windows (Windows service)

macOS follow-ups:

- ⏳ Run the daemon as a dedicated `_ovsc` user instead of root
- ✅ Kernel receive timestamps for PTP (`SO_TIMESTAMP_MONOTONIC`): user-space timestamps made the clock step by 2 to 3 ms on a real network
- 🧪 The OpenVirtualSoundcard app (`apps/macos`, egui) and the daemon's control socket: status, settings and latency. Still to do: put it in the installer package, and run it in CI
- ⏳ Sleep and wake, not handled or tested yet: `CLOCK_UPTIME_RAW`, the host clock both sides use, stops while the Mac sleeps. Today the daemon only holds an idle-sleep assertion while the engine runs (`prevent_idle_sleep`)
- ⏳ Report a clock domain derived from the PTP grandmaster (Core Audio treats devices with the same nonzero domain as synchronized; it is 0, unspecified, today)
- ⏳ Sample-rate and channel changes from Dante Controller, applied without restarting the daemon (today: edit the configuration and restart it)
- ⏳ Require the driver's code signature from XPC peers (`xpc_connection_set_peer_code_signing_requirement`); today any process running as `_coreaudiod`, that is any HAL plug-in, may connect
- ⏳ Releases signed with a Developer ID and notarized (`build-pkg.sh` supports it). A locally built, ad-hoc signed driver already loads with SIP enabled (M1, macOS 26.6.2)
- 💭 A POSIX shared-memory fallback, in case a sandboxed driver helper refuses `xpc_shmem`
- ⏳ Fill in the measured IO timing in [`MACOS.md`](MACOS.md#measured-io-timing) from the end-to-end logs

## 0.4: Feature parity with commercial virtual sound cards

- 🧪 Settings from Dante Controller: the device name and the sample rate (conmon 0x81) set from Dante Controller on macOS; the latency (ARC 0x1100/0x1101) and the encoding (conmon 0x83) show there, setting them from Dante Controller is still to be tried; identify is still to do
- 🧪 Monitoring in Dante Controller: signal meters, the Latency tab and interface traffic from the heartbeat work on macOS; the Network Config tab (addressing, redundancy) is still blank
- ⏳ Multicast transmit flows (ARC 0x2201/0x2202, `_netaudio-bund`)
- ⏳ Receiving multicast flows
- 💭 Metering (conmon 0x8002 heartbeat peaks)
- 💭 AES67 mode (SAP/SDP, PTPv2)
- 💭 Clock leader role (PTPv1 master with proper election)
- 🧪 GUI app for status and settings: macOS (`apps/macos`); a tray icon and other platforms later

## Non-goals

- Dante Domain Manager (encrypted, authenticated), and any feature that
  requires circumventing access control.
- Impersonating Audinate products. OpenVirtualSoundcard identifies itself as OpenVirtualSoundcard.

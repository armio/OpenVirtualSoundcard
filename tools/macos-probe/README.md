# macOS sandbox probe

> **Historical.** Before the driver was written, this probe found out
> whether and how a Core Audio driver can reach the daemon. Its results are
> recorded below; it is no longer developed. The driver's Core Audio ABI
> mirror and its XPC shim started as copies of the probe's and live on in
> [`crates/ovsc-hal/src/abi.rs`](../../crates/ovsc-hal/src/abi.rs)
> and [`crates/ovsc-ipc/c`](../../crates/ovsc-ipc/c).
> [`docs/MACOS.md`](../../docs/MACOS.md) describes the driver.

Core Audio runs third-party server plug-ins (`.driver` bundles in
`/Library/Audio/Plug-Ins/HAL`) in a sandboxed helper process. The OpenVirtualSoundcard
driver must get audio and clock data from the OpenVirtualSoundcard daemon, so we need to
know which channels that sandbox allows on each macOS release. This probe
answers that on real machines.

- `plugin/`: `OvscProbe.driver`, a server plug-in that publishes no
  device. When Core Audio loads it, it tries each channel to the daemon and
  logs one `ovprobe:` line per check (`PASS`/`FAIL`/`INFO`).
- `daemon/`: `ovprobe-daemon`, a LaunchDaemon that offers each channel: an
  XPC Mach service (listed in the plug-in's `AudioServerPlugIn_MachServices`)
  plus one that is not listed, shared memory handed over with `xpc_shmem` in
  both directions, POSIX shared memory with modes 0666, 0660 (group
  `_coreaudiod`) and 0600, files in several directories, `AF_UNIX`, UDP and
  TCP on 127.0.0.1.
- `shim/`: XPC, Mach and sandbox helpers shared by both (a little C, because
  XPC delivers events through blocks).

The plug-in is a Rust static library linked into an `MH_BUNDLE` with clang
and ad-hoc signed, exactly how the real driver is built, so a successful load
also shows that such a bundle loads at all. CI runs an ad-hoc signed
[BlackHole](https://github.com/ExistentialAudio/BlackHole) build next to it
as a control.

## Running it

CI runs it when started by hand: Actions, "macOS probe", Run workflow
(`.github/workflows/macos-probe.yml`). On a Mac of your own (it installs a
LaunchDaemon and a driver, and restarts Core Audio):

```sh
tools/macos-probe/build.sh
tools/macos-probe/run-probe.sh
```

To remove it afterwards:

```sh
sudo launchctl bootout system/org.openvirtualsoundcard.probe
sudo rm -rf /Library/Audio/Plug-Ins/HAL/OvscProbe.driver \
    /Library/LaunchDaemons/org.openvirtualsoundcard.probe.plist /usr/local/libexec/ovprobe-daemon
sudo killall coreaudiod
```

## Results

First run, 2026-10-02, on GitHub's hosted runners (macOS 26.6.2 arm64,
macOS 15.7.9 arm64, macOS 15.7.9 x86_64). All three gave the same answers:

- The ad-hoc signed, Rust-built `MH_BUNDLE` loads, and so does the ad-hoc
  signed BlackHole control, which also enumerates as a device. amfid logs
  `AppleMobileFileIntegrityError -423 ("adhoc signed")` for both, but that
  does not stop either from loading.
- The plug-in runs in its own `Core Audio Driver (OvscProbe.driver)`
  process (`com.apple.audio.Core-Audio-Driver-Service.helper`) as
  `_coreaudiod` (uid and gid 202), natively (not under Rosetta).
  `sandbox_check` reports it as not sandboxed.
- Every channel works: XPC to both the listed and the unlisted Mach
  service; `xpc_shmem` regions created by either side, mapped read-write by
  the other, with writes visible both ways; POSIX shared memory with mode
  0666 or 0660 (group `_coreaudiod`), and created by the plug-in itself;
  reading daemon files and creating files in `/tmp`, `/private/var/tmp` and
  the per-user temp and cache directories; `AF_UNIX`, UDP and TCP on
  127.0.0.1; joining a multicast group; the real-time thread policy.
- Only the 0600 root-owned shared memory segment is refused (`EACCES`), as
  Unix permissions require.
- `CLOCK_UPTIME_RAW` equals `mach_absolute_time` converted with the
  timebase (125/3 on Apple silicon), and both processes see the same clock.

GitHub's images run with SIP disabled, and other reports describe a
sandboxed helper on some Macs, so OpenVirtualSoundcard uses only the documented
channel: an XPC Mach service listed in `AudioServerPlugIn_MachServices`,
carrying a shared region created with `xpc_shmem`.

### Sizing the OpenVirtualSoundcard region

Second run, the same day and runners. These checks size the shared region
of the OpenVirtualSoundcard design ([`docs/MACOS.md`](../../docs/MACOS.md)): one region per daemon, created
with `mmap(MAP_ANON|MAP_SHARED)` and handed over immediately with
`xpc_shmem_create`.

- 64 MiB + 64 KiB (the v1 layout) and 32 MiB + 64 KiB (the fallback) both
  map read-write in the driver helper in microseconds (7-43 µs). Patterns
  written at both ends of every 256 KiB stride are visible both ways: the
  daemon's in the lower half, the plug-in's in the upper half.
- `RLIMIT_MEMLOCK` is unlimited in the helper, and `mlock` of 2 MiB and of
  the whole 64 MiB region succeeds.
- Touching 16 MiB of the mapping, one `fetch_add(0)` per 16 KiB page, takes
  about 50 µs.
- Mapping an anonymous private region over a live xpc mapping with
  `mmap(MAP_FIXED)` (the driver's way to retire a dead daemon's region)
  works: every page then reads zero, a later `munmap` succeeds, and the
  daemon's copy is untouched.
- The daemon's `mach_absolute_time` heartbeat in the 64 MiB region agrees
  with the plug-in's clock (the residual skew is the 1 ms update interval).

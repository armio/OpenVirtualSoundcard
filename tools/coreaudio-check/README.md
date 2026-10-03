# coreaudio-check

A small Core Audio client that tests a device the way an application uses
it. The macOS CI uses it to check the OpenVirtualSoundcard driver end to end, and to
check itself against BlackHole.

```sh
coreaudio-check list                       # every device: id, channels, rate, name, UID
coreaudio-check info <uid>                 # the properties applications see
coreaudio-check wait <uid> [seconds]       # wait for a device to appear
coreaudio-check prop <uid> <fourcc> [--scope glob|inpt|outp] [--element N] [--type u32|f64|cfstring|u32s]
coreaudio-check wait-status <uid> <text> [seconds]   # poll OpenVirtualSoundcard's 'ovst' status string
coreaudio-check wait-rate <uid> <hz> [seconds]
coreaudio-check check <uid> [--rate HZ] [--inputs N] [--outputs N] [--zts-period N]
                [--input-safety N] [--output-safety N] [--input-latency N] [--output-latency N]
                [--clock-algorithm FOURCC] [--element-names a,b,..]
coreaudio-check walk <uid>                 # every object and property must be self-consistent
coreaudio-check loopback <uid> [--seconds N] [--rate HZ] [--buffer FRAMES] [--channels N]
                [--max-bad N] [--max-slips N] [--max-jumps N] [--expect-device-delay D]
                [--expect-rate-ppm X] [--rate-tol-ppm T] [--allow-outage SECS]
                [--expect-silent-input] [--snapshot-prop FOURCC]
```

`loopback` plays a different pseudo-random signal on each output channel and
records the inputs, expecting output `c` back on input `c`. The samples are
multiples of 2^-23 below -6 dBFS, so they survive float to 24-bit and back
exactly: a correct path returns every frame bit for bit. It reports the
round-trip delay, each frame that came back wrong or as silence, any slip of
the delay, discontinuities in the device's timeline, and the device's sample
rate measured against the host clock. The delay is given twice: in captured
frames, and in device time (input sample time minus output sample time of
each frame, from the IO cycle timestamps, which stays meaningful across
timeline jumps). The last line, `CA-RESULT result=PASS|FAIL key=value ...`,
is meant for scripts.

It exits 0 only if every channel locked and stayed within `--max-bad` and
`--max-slips` (both 0 by default). When Core Audio itself skips IO (an
overload, visible as a jump in the device's timeline, common on loaded CI
machines) it also allows 4 IO buffers of bad frames and 2 slips per jump,
for at most `--max-jumps` jumps (3 by default). `--allow-outage SECS`
accepts one run of bad frames up to that long (a daemon restart, say);
`--expect-silent-input` instead requires the inputs to be exactly silent.

`check` prints the properties a host relies on (rates, channel counts,
latencies and safety offsets, buffer size range, element names) and exits 1
on any mismatch with the expectations given. `walk` visits every object the
device owns and every standard selector it claims to have, and checks that
the size and the data agree.

The signal analysis (`src/analysis.rs`) has unit tests that run on any OS;
`abi_check.c` checks the Core Audio declarations in `src/ca.rs` against the
SDK (`clang -fsyntax-only abi_check.c`).

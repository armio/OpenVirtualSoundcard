# OpenVirtualSoundcard protocol reference

An independent, byte-level description of the network protocols spoken by
Dante-compatible audio devices, written so that OpenVirtualSoundcard can be implemented
from it. It describes **formats and behaviour** in tables and prose. It does
not contain source code from the projects it cites.

> **Legal.** Dante is a trademark of Audinate Pty Ltd. The Dante protocols are
> proprietary and undocumented. This file is an independent interoperability
> reference compiled from public reverse-engineering work (see
> [§12](#12-sources--credits)). It is not written, reviewed or endorsed by
> Audinate, and nothing in it is guaranteed to be correct.

---

## 0. Conventions

### 0.1 Byte order, offsets, strings

| Item | Rule |
|---|---|
| Integers | Big-endian (network order) everywhere: control, media and PTP. |
| `p` offsets | Offset from the first byte of the UDP payload, header included. |
| `c` offsets | ARC, CMC and DBCP *content* offset: `c = p − 10` (after the 10-byte header, [§3.1](#31-header-10-bytes)). |
| `r` offsets | Conmon *record* offset: `r = p − 24` (the record starts at the revision word, [§7.1](#71-header-32-bytes)). |
| Pointers (`ptr`) | `u16` absolute **packet** offsets (`p`), never content-relative. `0` means "absent". |
| Strings | NUL-terminated ASCII/UTF-8. Usually reached through a `ptr` into a string heap after the fixed records. Fixed-size text fields in conmon are NUL-padded. |
| Unknown fields | Named `unknownN (observed X)`. Send the observed value. |

### 0.2 Confidence markers

| Marker | Meaning |
|---|---|
| ✅ | Confirmed by two independent sources, or by one implementation **and** a real-device capture, or interop-tested by Inferno against Dante Controller / Dante hardware. |
| ⚠️ | Single source, or the bytes are known but their meaning is guessed. |
| ❓ | Unknown or unverified. |

### 0.3 Source abbreviations

| Tag | Project | Root used for paths |
|---|---|---|
| **INF** | Inferno, a Rust Dante-compatible *device* interop-tested with Dante Controller (DC) and Dante hardware | `inferno_aoip/src/…` |
| **NAC** | network-audio-controller ("netaudio") Rust core: controller encoders/decoders and a virtual-device encoder | `packages/netaudio-core/src/…` |
| **NAP** | netaudio Python package | `packages/netaudio/src/netaudio/…` |
| **CAP** | Real-device captures in netaudio's test fixtures. These are the strongest evidence available. | `tests/fixtures/…` (e.g. `protocol_packets.json` keys, `*.bin` files) |
| **STM** | Statime fork with PTPv1 (`inferno-dev` branch) | `statime/src/datastructures/messages_v1/…`, `statime-linux/src/…` |
| **SF** | Searchfire, Inferno's mDNS library | `searchfire/src/…` |

> **Independence caveat.** netaudio's virtual device (`NAC publications.rs`,
> `NAC discovery.rs`) reuses many of Inferno's constants, such as the TXT values
> and the board-info layout. Agreement between INF and the NAC *virtual device*
> is therefore **not** independent confirmation. Agreement with NAC *parsers
> validated against CAP*, or with CAP itself, is.

---

## 1. Overview

### 1.1 Service and port map

| Function | Transport | Address : port | Direction | Source | Conf |
|---|---|---|---|---|---|
| mDNS / DNS-SD | UDP multicast | 224.0.0.251 : 5353 | all ↔ all | INF `device_server/mdns_server.rs`, `mdns_client.rs`; NAC `discovery.rs` | ✅ |
| `_netaudio-arc._udp` (ARC: routing and control) | UDP unicast | device : **SRV port**, usually 4440 | controller → device | INF `protocol/proto_arc.rs` (`PORT`); CAP `webapp/fixtures/devices.json` | ✅ |
| `_netaudio-cmc._udp` (CMC: control and monitoring) | UDP unicast | device : **SRV port**, usually 8800 (Dante Virtual Soundcard for Windows observed on **38800**) | controller → device | INF `protocol/proto_cmc.rs`; CAP `devices.json` | ✅ |
| `_netaudio-dbc._udp` (DBCP: flow setup) | UDP unicast | device : SRV port 4455, empty TXT | RX device → TX device | CAP `devices.json`; NAC `protocol.rs` (`SERVICE_DBC`, `PORT_ARC_SECONDARY=4455`). INF does not advertise it. | ✅ port / ⚠️ need |
| `_netaudio-chan._udp` (one instance per TX channel) | — | SRV port = DBCP port (4455) | — | INF `mdns_server.rs`; CAP `devices.json` | ✅ |
| `_netaudio-bund._udp` (one instance per multicast TX flow) | — | SRV port 4455; media address is in TXT | — | INF `mdns_server.rs`, `mdns_client.rs` | ⚠️ INF only |
| Conmon settings requests | UDP unicast | device : 8700 (also echoed in the CMC reply) | controller → device | INF `protocol/mcast.rs` (`INFO_REQUEST_PORT`); NAC `protocol.rs` (`PORT_SETTINGS`) | ✅ |
| Conmon status and notifications | UDP multicast | **224.0.0.231 : 8702** | device → all | INF `device_server/info_mcast_server.rs`; NAC `protocol.rs` | ✅ |
| Heartbeat | UDP multicast | **224.0.0.233 : 8708**, about 1 Hz | device → all | INF `info_mcast_server.rs`; NAC `heartbeat.rs` | ✅ |
| Metering to controller | UDP | controller : 8751 (unicast) / 224.0.0.231 : 8752 (multicast, netaudio default) | device → controller | NAC `protocol.rs` (`ControllerMetering`, `MulticastMetering`), `commands/metering.rs` | ⚠️ |
| 224.0.0.230 / 224.0.0.232 | — | — | — | Not referenced by either project. Treat any use as unverified. | ❓ |
| PTPv1 event | UDP multicast | 224.0.1.129 : 319 | clock | STM `statime-linux/src/socket.rs` | ✅ |
| PTPv1 general | UDP multicast | 224.0.1.129 : 320 | clock | STM `socket.rs` | ✅ |
| PTPv2 (AES67 mode) | UDP multicast | 224.0.1.129 : 319/320, domain 0 | clock | STM `inferno-ptpv2.toml`; INF README | ✅ |
| Unicast audio | UDP unicast | RX IP : port chosen by RX and sent in the DBCP request | TX → RX | INF `device_server/flows_tx.rs`, `flows_rx.rs`; CAP `protocol_1102_opcode_0100_id_15.bin` | ✅ |
| Unicast keepalives | UDP unicast | TX source IP : source port of the audio | RX → TX | INF `flows_rx.rs`, `flows_tx.rs` | ✅ (interop) |
| Multicast audio | UDP multicast | 239.255.x.y : **4321** | TX → group | INF `device_server/tx_multicasts.rs` (`MEDIA_PORT = 4321`); CAP 0x3200/0x2200 captures show 239.255.255.56:4321 and 239.255.69.103:4321 | ✅ |
| AES67 multicast | RTP over UDP | prefix 239.69.0.0 (property 0x8060), port 5004 (property 0x0222 observed) | TX → group | CAP `core_device_settings_avio-aes3-1.bin`; NAC `responses/mod.rs` | ⚠️ |
| SAP (AES67 announcements) | UDP multicast | 239.255.255.255 : 9875 | TX → all | NAC `sap.rs` | ✅ (standard) |

Dante Controller sends ARC to a device on its own host at **127.0.0.1**
(ARC port from the SRV record), not at the address it advertises: seen with
Dante Controller on macOS, whose requests (protocol 0x2729, 0x1000 channel
counts every few seconds) went to 127.0.0.1:4440 while its ConMon service
reached the same device's CMC and settings ports at the advertised address.
A device that answers ARC only on its interface address is listed, but
without channels. (CAP, OpenVirtualSoundcard on macOS, 2026-10.) ✅

Ports for ARC, CMC and DBCP must be taken from the SRV records. Inferno can run
several instances on one IP by moving all four of its ports (setting `ALT_PORT`
gives ARC=n, CMC=n+1, DBCP=n+2, settings=n+3), and Dante Virtual Soundcard
publishes CMC on 38800, so peers clearly honour SRV ports
(INF `device_server/settings.rs`). ✅

### 1.2 Protocol families at a glance

| Family | Start code (`p0`) | Header | Section |
|---|---|---|---|
| ARC | protocol version, e.g. 0x2729, 0x27FF, 0x2809 | 10 bytes | [§3](#3-requestresponse-framing-arc-cmc-dbcp), [§4](#4-arc-opcodes) |
| CMC | 0x1200 | 10 bytes | [§5](#5-cmc-port-8800) |
| DBCP | `dbcp1` TXT value, e.g. 0x1102 or 0x1200 | 10 bytes | [§6](#6-flow-control--dbcp-port-4455) |
| Conmon / settings | 0xFFFF | 32 bytes | [§7](#7-conmon--settings-8700-8702-8708) |
| Heartbeat | 0xFFFE | 32 bytes | [§7.7](#77-heartbeat-224002338708) |
| Media | first byte 0x02 | 9 bytes | [§8](#8-audio-media-packets) |
| PTPv1 | `00 01` (versionPTP = 1) | 40 bytes | [§9](#9-clock-ptpv1) |

### 1.3 Minimum surface of a working device

Inferno is accepted by Dante Controller and Dante hardware with only the
following. This is the practical minimum for OpenVirtualSoundcard (INF `device_server/*.rs`).

| Area | Must implement |
|---|---|
| mDNS | Respond for `_netaudio-arc`, `_netaudio-cmc`, one `_netaudio-chan` per TX channel name, and `_netaudio-bund` per multicast flow. Resolve peers' `_netaudio-chan` / `_netaudio-bund`. |
| ARC server | 0x1000, 0x1002, 0x1003, 0x1100, 0x1102, 0x2000, 0x2010, 0x2013, 0x2200, 0x2201, 0x2202, 0x2320, 0x3000, 0x3001, 0x3010, 0x3014, 0x3200, 0x3300 |
| CMC server | 0x1001 |
| DBCP | Server for 0x0100/0x0101/0x0102 (as transmitter) and client for the same (as receiver) |
| Conmon | Answer 0x0061, 0x00C1, 0x0021, 0x0013, 0x0077. Announce 0x0060 and 0x00C0 at start-up. Send 0x0102 channel-change notifications and the 1 Hz heartbeat. |
| Media | Unicast and multicast TX/RX, keepalives |
| Clock | PTPv1 follower on `_DFLT` (Inferno uses Statime); optionally PTPv2 |

---

## 2. Discovery (mDNS/DNS-SD)

### 2.1 Service instances

| Service type | Instance label | SRV port | TXT | Source | Conf |
|---|---|---|---|---|---|
| `_netaudio-arc._udp.local` | device (friendly) name, e.g. `avio-bt-1` | ARC port | [§2.2](#22-txt-keys) | INF `mdns_server.rs`; CAP `devices.json` | ✅ |
| `_netaudio-cmc._udp.local` | device name | CMC port | [§2.2](#22-txt-keys) | same | ✅ |
| `_netaudio-dbc._udp.local` | device name | 4455 | empty | CAP `devices.json` | ✅ |
| `_netaudio-chan._udp.local` | `<tx channel name>@<device name>`, e.g. `01@Windows-PC` | 4455 | [§2.2](#22-txt-keys) | INF `mdns_server.rs`, `mdns_client.rs`; CAP `devices.json` | ✅ |
| `_netaudio-bund._udp.local` | `<bundle id>@<device name>`, where bundle id = 1-based TX flow id | 4455 | [§2.2](#22-txt-keys) | INF `mdns_server.rs`, `tx_multicasts.rs` | ⚠️ |
| `_dantevideo._udp.local` | (Dante AV, ignore) | — | — | NAC `protocol.rs` | ⚠️ |

**Channel aliases.** Inferno publishes **two** `_netaudio-chan` instances per TX
channel when the user label differs from the factory name: `<factory>@dev`
with the bare TXT flag `default`, and `<label>@dev` without it. On rename it
withdraws and re-registers both (INF `mdns_server.rs` `add_tx_channel`;
`arc_server.rs` 0x2013). A subscriber may therefore name a TX channel by
either string. ⚠️ (real devices' handling of `default` ❓)

**SRV target / A record.** Real devices point SRV at their *factory* host name,
e.g. `AVIOBT-5279b6.local.` or `LX-DANTE-081258.local.`, while the instance
uses the friendly name. Dante Virtual Soundcard (DVS) on Windows uses the
machine name (`W.local.`) (CAP `devices.json`). Searchfire, and so Inferno, uses
`<instance label>.local` as the SRV target of every service, so each channel
service has its own A record. Dante Controller tolerates this (SF
`src/broadcast/service.rs`). ✅

### 2.2 TXT keys

`_netaudio-arc`:

| Key | Meaning | Inferno | DVS Win (CAP) | AVIO (CAP) | lx-dante (CAP) | Conf |
|---|---|---|---|---|---|---|
| `arcp_vers` | ARC protocol version. Maps to the ARC protocol id as major·4096 + minor·256 + patch ([§3.2](#32-protocol-ids-start-codes)). | `2.7.41` | `2.8.15` | `2.8.9` | `2.7.41` | ✅ |
| `arcp_min` | Minimum ARC version. Also appears as word 0x0204 in the 0x1003 reply. | `0.2.4` | `0.2.4` | `0.2.4` | `0.2.4` | ✅ |
| `router_vers` | Firmware/router version | `4.0.2` | `4.4.0` | `4.3.0` | `4.0.1` | ✅ (value free-form) |
| `router_info` | Board or product string | board name | `Dante Virtual Soundcard for Windows` | `DIOBT` | `Audinate DCM` | ✅ |
| `router_debug` | Build string (optional) | — | `Build ` | — | `Build :702` | ⚠️ |
| `mf` | Manufacturer | `Inferno-AoIP` | `Audinate` | `Audinate` | `Digigram` | ✅ |
| `model` | Model code | `_000000000000000b` | `DvsWin` | `DIOBT` / `DIOUSB` | `LX-DANTE` | ⚠️ Inferno's hex form looks like an 8-byte product id ([§7.4.2](#742-0x00c0-manufacturer--product-make-model)) |

Inferno publishes the ARC service with TTL 4500 s. Other services use the
library default of 120 s (INF `mdns_server.rs`; SF `service.rs`). ⚠️

`_netaudio-cmc`:

| Key | Meaning | Inferno | DVS Win | AVIO | lx-dante | Conf |
|---|---|---|---|---|---|---|
| `id` | 8-byte device id as 16 lowercase hex digits. The same id appears in conmon headers. | `0000` + IPv4 hex + process id hex | `525400fffe123456` (EUI-64 from MAC) | `001dc1fffe5279b6` | `001dc10812580000` (MAC + `0000`) | ✅ |
| `process` | Process id, decimal. Inferno appends it to `id` so several instances can share a host. | `0` default | `0` | `0` | `0` | ✅ |
| `cmcp_vers` | CMC protocol version | `1.2.0` | `1.2.0` | `1.2.0` | `1.2.0` | ✅ |
| `cmcp_min` | Minimum CMC version | `1.0.0` | `1.0.0` | `1.0.0` | `1.0.0` | ✅ |
| `server_vers` | Server version | `4.0.2` | `4.2.0` | `4.1.0` | `4.0.0` | ✅ |
| `channels` | unknown bitmask | `0x6000004d` | `0x6000017f` | `0x6000004d` | `0x6000017f` | ❓ |
| `mf`, `model` | as for ARC | | | | | ✅ |

Inferno also appends two empty TXT strings ("really needed?" in the source);
there is no evidence they are required. ❓

`_netaudio-chan` (one per TX channel):

| Key | Meaning | Inferno | DVS Win (CAP) | Conf |
|---|---|---|---|---|
| `txtvers` | TXT schema version | `2` | `2` | ✅ |
| `dbcp1` | Start code / protocol id that DBCP requests to this device must use, as hex | `0x1102` | `0x1200` | ✅ |
| `dbcp` | Second DBCP version word. Also the last word of the 0x1003 version block. | `0x1004` | `0x1004` | ✅ |
| `id` | 1-based TX channel number to put in DBCP channel lists | `n` | `n` | ✅ |
| `rate` | Sample rate (Hz) | `48000` | `48000` | ✅ |
| `pcm` | `"<bytes per sample> <encoding-capability bitmap in hex>"`. Bitmap bits: 0x2 = PCM16, 0x4 = PCM24, 0x8 = PCM32. | `3 e` | `3 e` | ✅ (netaudio's own encoder emits `3 0xe` ⚠️) |
| `enc` | Bits per sample on the wire | `24` | `24` | ✅ |
| `en` | Duplicate of `enc` (older key; Inferno reads `enc`, falling back to `en`) | `24` | `24` | ✅ |
| `latency_ns` | Minimum receive latency the transmitter demands, in ns | RX latency (FIXME in Inferno) | `6000000` | ✅ |
| `fpp` | `"<max>,<min>"` frames per packet the TX accepts (note the order: maximum first) | `32,2` | `48,48` | ✅ |
| `nchan` | Maximum channel slots per flow | `min(8, tx_channels)` | `8` | ✅ |
| `default` | Bare flag on the factory-name instance | present | absent | ⚠️ |
| `b.<bundle>` | `b.<bundle id>=<1-based slot in that bundle>`. The channel is also available in multicast bundle `<bundle id>@<device>`. | when multicast | — | ⚠️ INF only |
| `at2` | bare flag, meaning unknown | — | present | ❓ |

`_netaudio-bund` (Inferno; one per multicast TX flow):

| Key | Meaning | Conf |
|---|---|---|
| `txtvers` | `1` | ⚠️ |
| `id` | bundle id (= TX flow id) | ⚠️ |
| `nchan` | number of slots in the flow | ⚠️ |
| `latency_ns` | as for `_netaudio-chan` | ⚠️ |
| `fpp` | **single** value: the flow's fixed frames per packet | ⚠️ |
| `rate`, `enc` (or `en`) | format | ⚠️ |
| `a.0` | multicast destination IPv4 (dotted) | ⚠️ |
| `p.0` | destination UDP port (4321) | ⚠️ |

Source: INF `mdns_server.rs` `add_multicast_bundle`, `mdns_client.rs` `query_bund`.

### 2.3 Names

| Rule | Detail | Source | Conf |
|---|---|---|---|
| Length | Device names ≤ **31** characters. Dante Controller ignores devices with longer names. Inferno truncates `NAME` to 31 and builds the default as app name (≤ 22) + space + 8-hex IP; the factory name is short app name (≤ 14) + `-` + 16-hex device id. | INF `device_info.rs` (comment), `device_server/settings.rs`; NAC `protocol.rs` (`DANTE_NAME_MAX_LENGTH`) | ✅ |
| Device name charset (as set by netaudio) | `[A-Za-z0-9-]`, no leading or trailing `-`. Inferno's default name contains a space and still works with DC, so treat the rule as advisory when *receiving*. | NAC `protocol.rs` `validate_dante_name` | ⚠️ |
| Channel label charset | Device charset plus `:` and `_` (e.g. `system:capture_13`) | NAC `validate_dante_channel_name`; CAP 0x3000 captures | ✅ |
| Channel references | Any printable ASCII plus space, ≤ 31 (e.g. `Output 01`, `Main Mix Left`) | NAC `validate_dante_channel_reference` | ⚠️ |
| `@` | Separates channel and device in `_netaudio-chan` labels. Labels are raw DNS labels and may contain `@` and spaces. | INF `mdns_server.rs`; CAP | ✅ |
| `.` as device | In subscriptions the TX device `.` means "this device" (local loopback, status 0x0004 SUBSCRIBE_SELF) | NAC `subscription_status.rs`, `commands/subscriptions.rs` | ⚠️ |

### 2.4 Query and response behaviour Dante peers rely on

| Behaviour | Detail | Source | Conf |
|---|---|---|---|
| Direct instance query | Receivers do not browse. They send one query for `<ch>@<dev>._netaudio-chan._udp.local` with QTYPE **SRV and TXT** (two questions), and likewise for bundles. A responder must answer queries whose name equals the **instance FQDN**, not only PTR browses. | INF `mdns_client.rs` `query`; SF `src/broadcast.rs` (matches service type *or* instance id) | ✅ |
| Answer section | Put PTR, SRV and TXT for the instance in the **answer** section and the A record in *additional*. Inferno's client reads SRV and TXT only from answers whose owner name matches the queried FQDN, case-insensitively. | SF `service.rs` `dns_response`; INF `mdns_client.rs` | ✅ |
| A fallback | If the additional section lacks the A record for the SRV target, the client sends a separate A query for the target. | INF `mdns_client.rs` | ✅ |
| QU bit | When the question has the unicast-response bit set, answer unicast to the querier, otherwise multicast. | SF `broadcast.rs` | ✅ (standard) |
| Timeouts | Inferno: 3 s per query. Multicast-IP probe: 3 × 400 ms. Resolves of several channels are staggered 8 ms apart. | INF `mdns_client.rs`, `channels_subscriber.rs` | ⚠️ |
| Multicast address reservation | Before using a multicast group, Inferno queries the A record `<d>.<c>.<b>.<a>.in-addr.local`. If unanswered it claims the name itself (A = own IP), re-checks, then starts sending. On conflict it picks a new random 239.255.x.y. Whether Dante does the same is unknown. | INF `tx_multicasts.rs`, `mdns_server.rs` `reserve_multicast_ip` | ⚠️ |
| Self-filter | Inferno attaches a private TXT `_inferno-response-origin.local` = `<id hex>,<process>` to its own answers so co-located instances ignore themselves. This is Inferno-internal, not Dante. | INF `mdns_client.rs` | ⚠️ |

---

## 3. Request/response framing (ARC, CMC, DBCP)

### 3.1 Header (10 bytes)

| p | Size | Field | Request | Response | Source | Conf |
|---|---|---|---|---|---|---|
| 0 | u16 | protocol id / start code | ARC version, 0x1200 (CMC), or `dbcp1` (DBCP) | echoed from request | INF `protocol/req_resp.rs`; NAC `protocol.rs` | ✅ |
| 2 | u16 | total length (entire UDP payload) | | | same; CAP | ✅ |
| 4 | u16 | sequence / transaction id | chosen by sender. netaudio never uses 0. | echoed | same | ✅ |
| 6 | u16 | opcode | | echoed | same | ✅ |
| 8 | u16 | result code | **0** | [§3.3](#33-result-codes) | same; CAP | ✅ |
| 10 | … | content (`c = 0`) | | | | |

netaudio builds requests as an 8-byte header followed by a payload whose first
word is `0x0000`. On the wire that is identical.

A responder answers to the request's source address and port, copying the
protocol id, sequence and opcode (INF `req_resp.rs` `respond_with_code`). ✅
Inferno ignores requests whose result code is not 0. ✅

### 3.2 Protocol IDs (start codes)

ARC protocol ids **are** the ARC version encoded as `major<<12 | minor<<8 | patch`
(NAC `protocol.rs` `arc_protocol`). This is confirmed by TXT values matching
the ids devices answer with: `2.7.41` → 0x2729, `2.8.9` → 0x2809,
`2.8.15` → 0x280F. ✅

| Id | Meaning | Who uses it | Source | Conf |
|---|---|---|---|---|
| 0x27FF | ARC "2.7.255", generic default | netaudio's default for queries. Real devices reply with the same id. | NAC `protocol.rs`; CAP `core_*_avio-aes3-1.bin` | ✅ |
| 0x2729 | ARC 2.7.41 | Dante Controller routing (0x3010, 0x3001, 0x3200, 0x3300, 0x2013). Inferno's own version (0x1003). | CAP `protocol_packets.json` (`protocol_2729_*`); INF `arc_server.rs` | ✅ |
| 0x2801 | ARC 2.8.1 | flow queries | NAC `protocol.rs` | ⚠️ |
| 0x2809 | ARC 2.8.9 ("modern") | AVIO devices; set device name; latency query | CAP; NAC | ✅ |
| 0x280C | ARC 2.8.12 | modern opcodes | NAC `responses/tests/arc_280c_capture.rs` | ⚠️ |
| 0x280F | ARC 2.8.15 | DVS for Windows | CAP `devices.json` | ✅ |
| 0x1200 | CMC | CMC port | INF `cmc_server.rs` (does not check); NAC `commands/mod.rs` | ✅ |
| 0x1102 | DBCP (`dbcp1`) | Inferno and A32 firmware flow requests | INF `protocol/flows_control.rs`; CAP `protocol_1102_opcode_0100_id_15.bin` | ✅ |
| 0x1200 (DBCP) | DBCP (`dbcp1`) | `dbcp1=0x1200` on DVS Windows. Same number as the CMC id, but used on a different port. | CAP `devices.json` | ✅ |
| 0xFFFF | Conmon / settings | Uses the 32-byte header of [§7](#7-conmon--settings-8700-8702-8708), not this one. | INF `mcast.rs`; NAC | ✅ |
| 0xFFFE | Heartbeat | 32-byte header ([§7.7](#77-heartbeat-224002338708)) | INF `info_mcast_server.rs`; NAC `heartbeat.rs` | ✅ |
| 0x0008 | "DDP lock" | device locking, different header | NAC `protocol.rs` | ⚠️ |

**Server rule.** Accept any ARC id that a client might use (0x27FF, 0x2729,
0x2801, 0x2809, 0x280C, 0x280F) and echo it. Inferno never inspects the
start code. Advertising `arcp_vers=2.7.41` keeps controllers on the legacy
opcodes of [§4](#4-arc-opcodes). ✅

### 3.3 Result codes

| Code | Meaning | Source | Conf |
|---|---|---|---|
| 0x0000 | request | INF, NAC, CAP | ✅ |
| 0x0001 | success | INF, NAC, CAP | ✅ |
| 0x8112 | success, more pages follow | INF `proto_arc.rs`; NAC; CAP (lx-dante 0x3000) | ✅ |
| 0x0022 | error | NAC `protocol.rs` (`RESULT_CODE_ERROR`) | ⚠️ |
| 0x0030 | "frontend unavailable" / rejected. Inferno returns it for 0x2320. netaudio's virtual device returns it for refused writes. | INF `arc_server.rs`; NAC `publications.rs` | ⚠️ |
| 0xFFFF | Inferno's placeholder error ("TODO"). Not observed from real devices. | INF `arc_server.rs` | ⚠️ |
| 0x0103, 0x0301, 0x0315 | DBCP errors ([§6.6](#66-errors)) | INF `flows_control.rs` | ⚠️ |

### 3.4 Content conventions

| Convention | Detail | Source | Conf |
|---|---|---|---|
| Empty query | Simple queries (0x1000, 0x1002, 0x1003, 0x1100 "all", 0x1102, 0x3300) carry **no content**: a 10-byte packet. | NAC `commands/device.rs`; CAP `protocol_2729_opcode_3300_id_8175.bin` (`2729000a033c33000000`) | ✅ |
| Socket descriptor (8 bytes) | `[u8 length = 8][u8 family = 2 (AF_INET)][u16 port][IPv4]`, i.e. `08 02 pp pp a b c d`. This looks like a truncated BSD `sockaddr_in`. netaudio also accepts length 4 (port only). | CAP (0x3200, 0x2200, DBCP 0x0100 captures); NAC `responses/flows.rs`; INF DBCP client | ✅ |
| Word-length descriptors | Several sub-structures begin with `[u8 length in 16-bit words][u8 0]`, e.g. `0a 00` = 20 bytes. | CAP; NAC `parse_flow_record` | ✅ |
| Alignment | Inferno 4-aligns socket descriptors and flow headers inside ARC heaps and 8-aligns the socket descriptor in DBCP requests. | INF `arc_server.rs`, `flows_control.rs`; CAP | ✅ |

> ❗ **Discrepancy: socket-descriptor prefix in ARC replies.** Inferno writes
> `0x8002` (bytes `80 02`) in 0x2200 and 0x3200 replies (INF
> `proto_arc.rs` `DestinationSocketDescriptor`, `arc_server.rs`). Real devices
> send `08 02` there (CAP `protocol_2729_opcode_3200_id_8172.bin`, and the 0x2200
> vector in NAC `responses/tests/flows.rs`), and netaudio rejects any length
> byte other than 4 or 8 in RX flows. Dante Controller apparently does not
> check. **OpenVirtualSoundcard should send `08 02`.** In DBCP, Inferno already uses `08 02`.

### 3.5 Pagination

**Request content** for paged queries (0x2000, 0x2010, 0x2200, 0x3000, 0x3200):

| c | Size | Field | Source | Conf |
|---|---|---|---|---|
| 0 | u16 | `0x0001` (netaudio requires exactly 1) | NAC `commands/mod.rs` `channel_query_payload`, `parser.rs` `parse_channel_page_start`; CAP | ✅ |
| 2 | u16 | first item, **1-based** (channel or flow number). 0 is invalid: Inferno answers 0xFFFF. | INF `proto_arc.rs` `extract_start_index`; NAC | ✅ |
| 4 | u16 | last item, or 0 = "to the end". netaudio sends `count` for 0x2010. Inferno ignores it. | NAC `build_transmitter_names_for_protocol` | ✅ |

**Response content:**

| c | Size | Field | Source | Conf |
|---|---|---|---|---|
| 0 | u8 | capacity: number of record slots reserved in this page | INF `serialize_items`; NAC parsers; CAP | ✅ |
| 1 | u8 | count of records actually present | same | ✅ |
| 2 | capacity × record size | fixed-size records. For flows each "record" is a `u16 ptr` (0 = empty). | same | ✅ |
| … | var | heap: shared descriptors, strings, sub-objects, all referenced by `ptr` | same | ✅ |

Result 0x8112 means more items exist; the client asks again with
`first = last returned + 1`. Inferno stops filling a page when it holds
`capacity` records or the heap passes about 800 bytes (INF `proto_arc.rs`
`PACKET_SIZE_SOFT_LIMIT`).

| Query | Page size | Evidence | Conf |
|---|---|---|---|
| 0x3000 RX channels | **16** | CAP lx-dante (`10 10` + 0x8112); NAC `RX_CHANNELS_PER_PAGE = 16` (rejects capacity > 16) | ✅ |
| 0x2000 / 0x2010 TX channels | **32** | NAC `TX_CHANNELS_PER_PAGE = 32`; INF uses `min(32, n)` | ✅ |
| 0x2200 / 0x3200 flows | 16 pointer slots | CAP (`10 01`, `10 04`); INF `min(MAX, 16)` | ✅ |

> ❗ **Discrepancy: RX page size.** Inferno uses capacity `min(32, rx_channels)`
> for 0x3000 and may report fewer records than the capacity (padding with zeroed
> records). netaudio requires `capacity ≤ 16` and `capacity == count` whenever
> `count > 0`, and requires every 0x8112 page to be full. Real devices follow
> netaudio's rule. **OpenVirtualSoundcard: RX pages of ≤ 16 records with capacity = count.
> TX pages of ≤ 32 records with capacity = count. 0x8112 only on full pages.**

### 3.6 Batch-write convention (renames and subscriptions)

| c | Size | Field | Conf |
|---|---|---|---|
| 0 | u8 | `unknown0`: observed `0x02` in "batch" requests (netaudio, DC), and equal to the page capacity (`0x20`) in Dante Controller's "page" requests | ⚠️ |
| 1 | u8 | record count. This is the only byte Inferno reads. | ✅ |
| 2 | count × size | records | ✅ |
| … | | optional zero padding, then a string heap | ✅ |

Sources: INF `proto_arc.rs` `deserialize_items`; NAC `commands/subscriptions.rs`,
`commands/device.rs`; CAP `preset/protocol_2729_opcode_3001_*`, `3010_*`.

---

## 4. ARC opcodes

### 4.0 Summary

| Opcode | Name | INF server | NAC client | Section | Conf |
|---|---|---|---|---|---|
| 0x1000 | channel and flow counts | ✔ | ✔ | [4.2](#42-0x1000-channel-and-flow-counts) | ✅ |
| 0x1001 | set or reset device name (protocol 0x2809) | ✘ | ✔ | [4.3](#43-0x1001-set-device-name) | ✅ (CAP) |
| 0x1002 | get device name | ✔ | ✔ | [4.4](#44-0x1002-get-device-name) | ✅ |
| 0x1003 | device info: names, board, versions | ✔ | ✔ | [4.5](#45-0x1003-device-info) | ✅ |
| 0x1100 | read device properties (latency, sample rate, …) | stub (110 zero bytes) | ✔ | [4.6](#46-0x1100--0x1101--0x1102--0x1f01-device-properties) | ✅ |
| 0x1101 | write device properties (set latency, fpp, AES67 prefix) | ✘ | ✔ | [4.6](#46-0x1100--0x1101--0x1102--0x1f01-device-properties) | ⚠️ |
| 0x1102 | property directory | stub (94 zero bytes) | ✔ | [4.6](#46-0x1100--0x1101--0x1102--0x1f01-device-properties) | ✅ |
| 0x1F01 | store current configuration | ✘ | ✔ | [4.6](#46-0x1100--0x1101--0x1102--0x1f01-device-properties) | ⚠️ |
| 0x2000 | TX channels (factory names) | ✔ | ✔ | [4.7](#47-0x2000-tx-channels) | ✅ |
| 0x2010 | TX channel labels (friendly names) | ✔ | ✔ | [4.8](#48-0x2010-tx-channel-labels) | ✅ |
| 0x2013 | rename TX channels | ✔ | ✔ | [4.9](#49-0x2013-rename-tx-channels) | ✅ |
| 0x2032 | TX channel capabilities | ✘ | ✔ | [4.10](#410-0x2032-0x2204-0x2320-0x3201-minor-opcodes) | ⚠️ |
| 0x2200 | TX flows | ✔ | ✔ | [4.11](#411-0x2200-tx-flows) | ✅ |
| 0x2201 | create multicast TX flow (legacy "fixed") | ✔ | ✔ | [4.12](#412-0x2201-create-multicast-tx-flow) | ✅ |
| 0x2202 | delete TX flow (legacy) | ✔ | ✔ | [4.13](#413-0x2202-delete-tx-flows) | ✅ |
| 0x2204 | TX flow labels | ✘ | ✔ | [4.10](#410-0x2032-0x2204-0x2320-0x3201-minor-opcodes) | ⚠️ |
| 0x2320 | unknown; sent by DC | answers 0x0030 | ✘ | [4.10](#410-0x2032-0x2204-0x2320-0x3201-minor-opcodes) | ❓ |
| 0x3000 | RX channels and subscription status | ✔ | ✔ | [4.14](#414-0x3000-rx-channels) | ✅ |
| 0x3001 | rename RX channels | ✔ | ✔ | [4.15](#415-0x3001-rename-rx-channels) | ✅ |
| 0x3010 | set (or clear) subscriptions | ✔ | ✔ | [4.16](#416-0x3010-set-subscriptions) | ✅ |
| 0x3014 | remove subscriptions | ✔ (first entry only) | ✔ | [4.17](#417-0x3014-remove-subscriptions) | ✅ |
| 0x3200 | RX flows | ✔ | ✔ | [4.18](#418-0x3200-rx-flows) | ✅ |
| 0x3201 | external (RTP/AES67) receiver subscription | ✘ | ✔ | [4.10](#410-0x2032-0x2204-0x2320-0x3201-minor-opcodes) | ⚠️ |
| 0x3300 | RX port ranges | ✔ | ✔ | [4.19](#419-0x3300-rx-port-ranges) | ✅ |
| 0x2400, 0x2438, 0x2600–0x2602, 0x3400, 0x3401, 0x3410, 0x3600 | "modern" ARC 2.8.x equivalents | ✘ | ✔ | [4.20](#420-modern-arc-28x-opcodes) | ⚠️ |

Sources: INF `device_server/arc_server.rs`, `protocol/proto_arc.rs`;
NAC `commands/mod.rs`, `protocol.rs`.

### 4.1 Shared structures

**Common channel descriptor (16 bytes).** One instance is shared by all
channels that have the same format. 0x2000 and 0x3000 records point at it.

| Off | Size | Field | Inferno | Real (CAP) | Conf |
|---|---|---|---|---|---|
| 0 | u32 | sample rate | rate | 48000 | ✅ |
| 4 | u8 | `unknown1 (observed 1)` | 1 | 1 | ✅ |
| 5 | u8 | `unknown2 (observed 1)` | 1 | 1 | ✅ |
| 6 | u16 | current encoding (bits) | 24 | 24 (0x18) | ✅ |
| 8 | u16 | `unknown3 (observed 0x0400)` | 0x0400 | 0x0400 | ✅ |
| 10 | u16 | encoding (repeat) | 24 | 24 | ✅ |
| 12 | u16 | encoding (repeat) | 24 | 24 | ✅ |
| 14 | u16 | encoding-capability bitmap: 0x2 PCM16, 0x4 PCM24, 0x8 PCM32. Inferno calls it `pcm_type` ("usually 0xe, 4 in older devices"). | 0x000E | 0x000E (AVIO), 0x0004 (lx-dante) | ✅ |

Sources: INF `proto_arc.rs` `CommonChannelsDescriptor`; NAC `parser.rs`
`parse_channel_audio_metadata`, `channel_audio_publication`; CAP
`20250517_*_get_receivers_response.bin`.

**Socket descriptor (8 bytes):** see [§3.4](#34-content-conventions).

### 4.2 0x1000 channel and flow counts

Request: empty content.

Response:

| p | c | Size | Field | Inferno | AVIO (CAP) | Conf |
|---|---|---|---|---|---|---|
| 10 | 0 | u16 | capability word. Inferno: byte `c0` is `unknown (observed 0, "or 5")`; byte `c1` has bit 4 = supports TX-channel rename and bit 5 = supports TX multicast. netaudio: bit 0x1000 means the device uses the modern 0x2601 flow creation. | 0x0030 | 0x0DF9 | ✅ bits 0x10/0x20 and 0x1000; others ❓ |
| 12 | 2 | u16 | TX channel count | n_tx | 2 | ✅ |
| 14 | 4 | u16 | RX channel count | n_rx | 2 | ✅ |
| 16 | 6 | u16 | `unknown4 (observed 4, 1 or 2)` | 4 | 2 | ❓ |
| 18 | 8 | u16 | max channel slots per TX flow | min(8, n_tx) | 2 | ✅ |
| 20 | 10 | u16 | max channel slots per RX flow | 8 | 8 | ✅ (NAC name) |
| 22 | 12 | u16 | max TX flows | 32 | 2 | ✅ |
| 24 | 14 | u16 | max RX flows | 32 | 2 | ✅ |
| 26 | 16 | u16 | `unknown5 (observed n_tx+n_rx (Inferno), 2 (AVIO))` | n_tx+n_rx | 2 | ❓ |
| 28 | 18 | u16 | `unknown6 (observed 1)` | 1 | 1 | ❓ |
| 30 | 20 | u16 | network interface count | 1 | 1 | ✅ (NAC) |
| 32 | 22 | 12–16 | zero. netaudio reads p36 RX error-flag mask and p38 RX error-field mask (protocol ≥ 0x2711), and p44 resource-extension offset (≥ 0x2802). | 12 zero bytes | 16 zero bytes | ⚠️ |

Sources: INF `proto_arc.rs` `channels_and_flows_count`, `arc_server.rs`;
NAC `parser.rs` `parse_channel_count`; CAP
`20250517_200646_416392_avio-aes3-1_get_channel_count_response.bin`.

### 4.3 0x1001 set device name

Sent with protocol **0x2809** by netaudio (CAP
`device_rename/protocol_2809_opcode_1001_id_27507.bin`:
`2809 0015 261b 1001 0000` + `avio-bt-11\0`).

| Request c | Field |
|---|---|
| 0 | new name, NUL-terminated (≤ 31 characters) |
| (empty) | Content length 0 resets the device to its factory/default name (NAC `build_reset_name`) |

Response: result 1, empty content. The device then re-registers its mDNS
services under the new name and emits notifications 288 and/or 4110
([§7.3](#73-message-types-request--response--notification)). Inferno does not implement 0x1001;
its name is fixed by configuration.

Sources: NAC `protocol.rs` `build_set_device_name`, NAP
`dante/application.py` (`DEVICE_NAME_NOTIFICATION_IDS`). ✅ request
(CAP) / ⚠️ response and notifications.

> On the CMC port, opcode 0x1001 means something else: registration
> ([§5.1](#51-0x1001-registration--device-advertisement)).

### 4.4 0x1002 get device name

Request: empty. Response content: device (friendly) name + NUL.

Sources: INF `arc_server.rs` (`GET_DEVICE_NAME_OPCODE`, "used by
network-audio-controller"); CAP `core_device_name_avio-aes3-1.bin`. ✅

### 4.5 0x1003 device info

Request: empty. Response: a header of pointers into a name block, a version
block and a string heap. Two real layouts were captured. Both are consistent
with offsets `c2` and `c4` being **pointers** to the blocks.

| c | Field | lx-dante (CAP, 0x2729) | AVIO (CAP, 0x27FF) | Inferno | netaudio reads as | Conf |
|---|---|---|---|---|---|---|
| 0 | `unknown0` | 0x0000 | 0x001C | 0 | — | ❓ |
| 2 | ptr → name block | 0x0014 (→ c10) | 0x001C (→ c18) | **0** | — | ✅ (two captures) |
| 4 | ptr → version block | 0x0020 (→ c22) | 0x0028 (→ c30) | **0** | — | ✅ |
| 6 | ptr → board / model string | `Audinate DCM` | `DIOAES3` | board name | "model code" | ✅ |
| 8 | ptr → build string | `:702` | `:800` | `:705` | "port" | ✅ |
| 10 | lx: name block. AVIO: `unknown (0x3100)` | | | | | |

Name block, at the pointer in `c2`:

| +0 | u16 `unknown (observed 0x0500)` |
|---|---|
| +2 | ptr → friendly device name |
| +4 | ptr → factory/default device name, e.g. `LX-DANTE-081258`, `AVIOAES3-53ef37` |
| +6 | ptr → friendly device name (again) |
| +8, +10 | 0 |

Version block, at the pointer in `c4`:

| +0 | +2 | +4 | +6 | +8 | +10 | +12 | +14 |
|---|---|---|---|---|---|---|---|
| `unknown` (lx 0x0400, AVIO 0x0A0A) | 0 | `unknown` (0x0400 / 0x0403) | `unknown` (0x0100 / 0) | **ARC version** (0x2729 / 0x2809) | **arcp_min** 0x0204 | `dbcp1`? (0x1102 lx and Inferno / 0x1200 AVIO) | **dbcp** 0x1004 |

Inferno sends the lx-dante shape with the two block pointers and most version
words zeroed. Only 0x2729 at c30 and 0x1102 at c34 are filled. Dante Controller
accepts that. netaudio reads friendly and factory names at **fixed** c12/c14,
which works for lx-dante and Inferno but returns empty strings for AVIO.
**OpenVirtualSoundcard: emit the lx-dante layout with real pointers (c2 = 0x0014,
c4 = 0x0020) and the observed version words.**

Sources: INF `proto_arc.rs` `get_device_names`, `arc_server.rs`; NAC
`responses/device.rs` `parse_device_info`; CAP `core_device_info_lx-dante.bin`,
`core_device_info_avio-aes3-1.bin`.

### 4.6 0x1100 / 0x1101 / 0x1102 / 0x1F01: device properties

The *property* family carries sample rate, latency, frames per packet and AES67
settings. Each property id is a u16 and **bit 15 set** means the value is a
`u32` stored elsewhere and referenced by pointer.

#### 4.6.1 0x1100 read properties

Request content: empty (= all properties), or
`[u8 0][u8 n][n × u16 property id]`. Dante Controller asks for 19 ids
(captured by Inferno). netaudio's latency query asks for 23:
`0201 8204 8205 0210 0211 8218 8219 8301 8302 8306 0310 0311 0303 8021 00F0 8060 0022 0063 0064 0065 0222 0212 8321`
(NAC `commands/mod.rs` `LATENCY_CONFIG_QUERY_INFO_CODES`; INF
`arc_server.rs` comment on 0x1100).

Response content:

| c | Size | Field | Conf |
|---|---|---|---|
| 0 | u8 | `unknown0 (observed 0x24, 0x1B, 0x17, 0x12; netaudio's virtual device sends 2)` | ❓ |
| 1 | u8 | record count n | ✅ |
| 2 | n × 4 | records `[u16 id][u16 value-or-ptr]`. id bit 15 clear: inline u16 value. id bit 15 set: ptr to a u32. id = 0: "unavailable", and the u16 holds the requested id. | ✅ |
| … | | u32 value pool | ✅ |

Example, AVIO (CAP `core_device_settings_avio-aes3-1.bin`, 31 records):
`8020→48000`, `8204→1 000 000`, `8205→1 000 000`, `8301→1 000 000`,
`8306→1 000 000`, `8302→20 312 500`, `8321→2 000 000`, `8060→239.69.0.0`,
`0210=16`, `0211=16`, `0212=48`, `0303=2`, `0222=5004`, `83F0→5`.

**Inferno sends 110 zero bytes** (record count 0) and Dante Controller accepts
it, though DC then shows no latency or sample rate. ✅ (interop)

**OpenVirtualSoundcard answers** with byte 0 = 2, like netaudio's virtual device, and
one record per requested id (all of the ids below for an empty request):
`8020` sample rate, `8204` default latency (the configuration's), `8205`
configured latency (the one set last, applied at the next start of the
network engine), `8301` the running receive latency, `8302` 40 ms maximum,
`8306` 1 ms minimum, and `0211` / `0310` frames per packet. Any other id is
"unavailable" (`0000 id`). The u32 pool follows the records, in record order.
⚠️ (not yet checked against what Dante Controller displays)

#### 4.6.2 Property ids

| Id | Name (source) | Type | Observed | Conf |
|---|---|---|---|---|
| 0x8020 | sample rate (NAC) | u32 | 48000 | ✅ |
| 0x8021 | `unknown` | u32 | 0 | ❓ |
| 0x0022 / 0x0023 / 0x0024 | `unknown` (0x0023 = 24, possibly encoding) | u16 | 1 / 0x18 / 1 | ❓ |
| 0x8060 | AES67 multicast prefix (NAC) | u32 IPv4 | 239.69.0.0 | ✅ |
| 0x0062 | `unknown` | u16 | 1 | ❓ |
| 0x0063 | AES67 configured: 3 = on, 1 = off (NAC) | u16 | 1 | ⚠️ |
| 0x0064, 0x0065, 0x00F0 | `unknown` (often "unavailable") | — | — | ❓ |
| 0x0201 | `unknown` | u16 | 1 | ❓ |
| 0x8204 | "default latency" / "TX flow latency" (NAC uses both names) | u32 ns | 1 ms | ✅ value / ⚠️ name |
| 0x8205 | "configured latency" / "unicast configured latency" | u32 ns | 1 ms | ✅ / ⚠️ |
| 0x020A, 0x020B | `unknown` | u16 | 0 | ❓ |
| 0x0210 | TX flow frames per packet | u16 | 16 | ⚠️ |
| 0x0211 | unicast configured frames per packet | u16 | 16 | ⚠️ |
| 0x0212 / 0x0213 / 0x0214 | `unknown` (fpp-like) | u16 | 48 / 16 / 16 | ❓ |
| 0x0222 | `unknown`, value 5004 (AES67 RTP port?) | u16 | 0x138C | ❓ |
| 0x8218, 0x8219 | `unknown` (requested by DC, unavailable on AVIO) | — | — | ❓ |
| 0x8301 | "active latency" / "RX flow latency" | u32 ns | 1 ms | ✅ / ⚠️ |
| 0x8302 | maximum latency | u32 ns | 20.3125 ms | ✅ |
| 0x8304 | "pre-3.0 compatibility" (NAC) | u32 | — | ⚠️ |
| 0x8306 | minimum latency | u32 ns | 1 ms | ✅ |
| 0x8321 | `unknown` | u32 | 2 000 000 | ❓ |
| 0x0310 | RX flow frames per packet | u16 | 16 | ⚠️ |
| 0x0311 / 0x0312 | `unknown` | u16 | 16 / 48 | ❓ |
| 0x0303 | RX flow default slots | u16 | 2 | ⚠️ |
| 0x0309, 0x0209, 0x0601, 0x83F0 | `unknown` | — | 3, 1, 0, 5 | ❓ |

Sources: NAC `responses/mod.rs` (`DEVICE_SETTINGS_INFO_*`),
`commands/performance.rs` (`PROPERTY_*`); CAP `core_device_settings_*.bin`,
`core_latency_config_avio-aes3-1.bin`.

#### 4.6.3 0x1101 write properties: setting latency

The request reuses the property-record shape, but **byte 0 is the count**:

| c | Size | Field | Conf |
|---|---|---|---|
| 0 | u8 | record count n | ⚠️ |
| 1 | u8 | `unknown1 (observed 4 in performance writes; 1 in the AES67-prefix write)` | ❓ |
| 2 | n × 4 | `[u16 id][u16 inline value or ptr]`. Pointers start at `p = 12 + 4n`. | ✅ |
| … | | u32 values, in record order | ✅ |

**"Set latency" as sent by Dante Controller** (replayed byte-for-byte by
netaudio; NAC `commands/mod.rs` `LATENCY_SET_PREAMBLE`,
`commands/settings.rs`; test `set_latency_matches_captured_250_microsecond_packet`),
protocol 0x27FF or 0x2809, 40 bytes:

| p | Bytes | Meaning |
|---|---|---|
| 0–9 | `27FF 0028 ssss 1101 0000` | header |
| 10 | `05 04` | count 5, `unknown1` 4 |
| 12 | `8205 0020` | configured latency → ptr p32 |
| 16 | `0211 0004` | unicast fpp = 4 |
| 20 | `8301 0024` | RX latency → ptr p36 |
| 24 | `0310 0004` | RX fpp = 4 |
| 28 | `8302 8306` | 5th record. The "value" 0x8306 is not a valid pointer, so the meaning is ❓ (perhaps a read-back list of max/min latency). |
| 32 | u32 | latency ns |
| 36 | u32 | latency ns (repeated) |

The device's reply to 0x1101 is unknown (❓). netaudio's virtual device answers
result 1 with a 4-record property block echoing the applied values
(NAC `publications.rs` `LatencyApplied`).

**OpenVirtualSoundcard** takes the latency from `8205`, or else `8301`, and refuses (an
error result, nothing changed) a value outside 1 to 40 ms. It saves the new
latency with the device's state, answers like netaudio's virtual device
(`8205`, `0211`, `8301`, `0310` with the applied values), then restarts its
network engine with it while the PTP clock keeps running: subscriptions drop
for about a second and come back. A pointer past the end of the packet reads
as no value, so the 5th record above is ignored. ⚠️

**Set AES67 multicast prefix** (protocol 0x2809): content
`01 01 | 8060 0010 | a b c d` (NAC `commands/settings.rs`). ⚠️

#### 4.6.4 0x1102 property directory

Request: empty. Response content: `[u16 n][n × (u16 property id, u16 flags)]`.
Observed flags: 1 and 3. Bit 0 is probably "readable" and bit 1 "writable" (❓).
lx-dante lists 25 entries: `8020:1 8021:3 0022:3 0024:1 8060:3 00F0:3 0201:3 8204:3 8205:3 020A:1 020B:1 0210:3 0211:3 0212:3 0213:1 0214:1 8301:3 8306:1 8302:1 0310:3 0311:1 0312:1 0303:3 83F0:1 0601:1`.
Inferno sends 94 zero bytes, i.e. n = 0.

Sources: CAP `property_directory/protocol_2729_opcode_1102_id_12358036.bin`;
NAC `parse_property_directory`; INF `arc_server.rs`. ✅ format / ⚠️ flags.

#### 4.6.5 0x1F01 store current configuration

Empty content. Persists settings (NAC `commands/performance.rs`). ⚠️

### 4.7 0x2000 TX channels

Request: paged ([§3.5](#35-pagination)). Response records (8 bytes):

| Off | Size | Field | Inferno | Conf |
|---|---|---|---|---|
| 0 | u16 | channel number (1-based, consecutive from the requested start) | n | ✅ |
| 2 | u16 | `unknown (observed 7)` | 7 | ⚠️ |
| 4 | u16 | ptr → common channel descriptor | shared | ✅ |
| 6 | u16 | ptr → **factory** channel name, e.g. `01` | `"%02d"` | ✅ |

netaudio requires the shared descriptor to sit **immediately after** the record
array, with names after it. Inferno does this when capacity = count. Some
devices send a body header of `00 00` followed by records terminated by a zero
channel number (NAC `parser.rs` `parse_tx_info_page`). ⚠️

Sources: INF `proto_arc.rs` `get_transmit_channels`, `arc_server.rs`; NAC `parser.rs`.

### 4.8 0x2010 TX channel labels

Request: paged. netaudio sends `[1][1][count]`. Response records (6 bytes):

| Off | Size | Field | Conf |
|---|---|---|---|
| 0 | u16 | channel number (Inferno and the netaudio virtual device write it twice). netaudio ignores this word. | ⚠️ |
| 2 | u16 | channel number | ✅ |
| 4 | u16 | ptr → user label | ✅ |

Inferno starts the heap with 4 zero bytes. netaudio's encoder pads the record
array to 4-byte alignment. Both are harmless. netaudio also accepts a `00 00`
header variant whose record count is derived from the first name pointer.

Sources: INF `proto_arc.rs`, `arc_server.rs`; NAC `parser.rs`
`parse_tx_friendly_page`, `publications.rs`.

### 4.9 0x2013 rename TX channels

Request (CAP `transmitter_channel_rename/protocol_2729_opcode_2013_id_1.bin`:
`2729 001d 49a4 2013 0000 | 02 01 | 0000 0001 0018 | 000000000000 | "tett\0"`):

| c | Field |
|---|---|
| 0 | `unknown0 (observed 0x02)` |
| 1 | count n |
| 2 | n × `[u16 unknown (0)][u16 channel number][u16 ptr → new label]` |
| … | 6 zero bytes, then labels |

An empty label (ptr to end of packet, or no string) resets to the factory name
(NAC `build_reset_channel_name`). Response: result 1. Inferno sends content
`00 00`, noting that devices sometimes send `00 01 00 00 <ch>`. On no match,
Inferno answers 0xFFFF. After the rename Inferno re-publishes the channel's mDNS
instances. ✅ request (CAP + INF) / ⚠️ response.

### 4.10 0x2032, 0x2204, 0x2320, 0x3201: minor opcodes

| Opcode | Request | Response | Source | Conf |
|---|---|---|---|---|
| 0x2032 TX channel capabilities | empty | `[u8 0][u8 n][n × (u16 first TX ch, u16 last TX ch, u16 unknown)]` | NAC `responses/flows.rs` `parse_transmit_channel_capabilities` | ⚠️ |
| 0x2204 TX flow labels | paged | empty page `02 00` (netaudio virtual device). Dante Controller asks it when Device View opens; OpenVirtualSoundcard answers the empty page. | NAC `publications.rs`; CAP | ⚠️ |
| 0x2320 | sent by DC, content unknown | Inferno answers result **0x0030** with empty content | INF `arc_server.rs` | ❓ |
| 0x3201 external receiver subscription (manual RTP/AES67 flow into RX channels; default port 4321) | complex | — | NAC `commands/external_subscription.rs` | ⚠️ |

### 4.11 0x2200 TX flows

Request: paged by flow number (`[1][first flow][0]`). Response:
`[u8 capacity (16)][u8 count][capacity × u16 ptr → flow record (0 = empty)]`,
then the objects.

**Flow record:**

| Off | Size | Field | Inferno | Real (CAP) | Conf |
|---|---|---|---|---|---|
| 0 | u16 | flow number (1..32) | index+1 | 32 | ✅ |
| 2 | u16 | flow type / config flags. Inferno: 0x0011 unicast, 0x0002 multicast. netaudio: 2 = persistent native multicast, 6 = persistent with explicit destinations (AES67), +0x10 = not advertised. | 0x11 / 0x02 | 0x0002 (multicast) | ✅ 2 / ⚠️ 0x11 |
| 4 | u32 | sample rate | | 192000 | ✅ |
| 8 | u16 | `unknown (observed 0)` | 0 | 0 | ✅ |
| 10 | u16 | encoding (bits) | | 24 | ✅ |
| 12 | u16 | destination count | 1 | 1 | ✅ |
| 14 | u16 | channel slot count N | | 8 | ✅ |
| 16 | 2 × dests | ptr → socket descriptor per destination | | → `08 02 10e1 efff4567` (239.255.69.103:4321) | ✅ |
| … | 2 × N | TX channel number per slot, 0 = empty slot | | `0010 0000 …` | ✅ |
| … | u16 | ptr → flow extension | | | ✅ |

**Flow extension (20 bytes):**

| Off | Size | Field | Inferno (unicast) | Real multicast (CAP) | Conf |
|---|---|---|---|---|---|
| 0 | u8 | length in words = 0x0A | 0x0A | 0x0A | ✅ |
| 1 | u8 | 0 | 0 | 0 | ✅ |
| 2 | u16 | `unknown (observed 1)` | 1 | 1 | ✅ |
| 4 | u16 | ptr → receiving device name (unicast), else 0 | RX host | 0 | ✅ |
| 6 | u16 | ptr → receiver's flow name (unicast), else 0 | RX flow name | 0 | ✅ |
| 8 | u16 | **frames per packet**. Inferno calls it "unknown, 0x10 or 0x3C" and hard-codes 0x10. netaudio decodes it as fpp; 0x3C = 60. | 16 (fixed) | 16 | ✅ (NAC + CAP) |
| 10 | u16 | ptr → local flow name. Inferno: `<flow id>_<process id>`. Real: `32`. | | `32` | ✅ |
| 12 | u32 | latency ns (seen on multicast flows) | 0 | 1 000 000 | ✅ |
| 16 | u16 | media class: 1 = native Dante, 3 = RTP/AES67 (netaudio) | 0 | 0 | ⚠️ |
| 18 | u16 | 0 | 0 | 0 | ✅ |

Sources: INF `proto_arc.rs` `query_tx_flows`, `arc_server.rs`; NAC
`responses/flows.rs` `parse_tx_flow_page`, `parse_flow_record`; CAP vector in NAC
`responses/tests/flows.rs` (`tx_flows_parser_preserves_authentic_zero_channel_placeholders`).

> **OpenVirtualSoundcard:** report the real fpp at +8 and use `08 02` socket descriptors.

### 4.12 0x2201 create multicast TX flow

Legacy, "fixed" family. The controller chooses the flow number.

| p | Size | Field | Conf |
|---|---|---|---|
| 10 | u8 | `unknown (0x01)` | ⚠️ |
| 11 | u8 | descriptor count (1) | ✅ |
| 12 | u16 | ptr → flow descriptor (0x0010) | ✅ |
| 14 | u16 | 0 | ✅ |
| 16 | u16 | flow number (1..32) | ✅ |
| 18 | u16 | flags: **2** = native multicast (Inferno requires 2); 6 = AES67 with destinations; +0x10 = not advertised; 0 = not persistent | ✅ 2 |
| 20 | u32 | sample rate | ✅ |
| 24 | u16 | 0 | ✅ |
| 26 | u16 | encoding | ✅ |
| 28 | u16 | destination count (0 for native) | ✅ |
| 30 | u16 | channel count N | ✅ |
| 32 | … | destination ptrs, then N × u16 TX channel numbers (0 = empty) | ✅ |
| … | u16 | ptr → extension (4-aligned) | ✅ |
| ext | 20 | `0A 00`, …, +8 fpp, +10 ptr → label, +16 media class (1 native, 3 AES67) | ⚠️ |
| … | | label, then socket descriptors `08 02 port ip` (AES67 only) | ⚠️ |

Inferno's response: result 1 with content `[u16 n][u16 0][n × u16 created flow numbers]`,
or 0xFFFF if nothing was created. The new flow first goes through the
multicast-address check ([§2.4](#24-query-and-response-behaviour-dante-peers-rely-on)), then
starts sending to 239.255.x.y:4321 and is advertised as `_netaudio-bund`, with
`b.<id>` TXT added to each member channel.

Sources: INF `proto_arc.rs` `create_multicast_tx_flow`, `arc_server.rs`,
`tx_multicasts.rs`; NAC `commands/flows.rs` `build_fixed_multicast_flow`.

### 4.13 0x2202 delete TX flows

Content `[u16 n][u16 0][n × u16 flow number]`. Response: result 1 with empty
content, or 0xFFFF. Inferno deletes only multicast flows by number. Unicast
flows belong to their DBCP handle.

Sources: INF `arc_server.rs`; NAC `commands/flows.rs` `build_delete_tx_flow`. ✅

### 4.14 0x3000 RX channels

Request: paged, 16 per page. Response record (20 bytes):

| Off | Size | Field | Inferno | AVIO / lx (CAP) | Conf |
|---|---|---|---|---|---|
| 0 | u16 | RX channel number | n | 1… | ✅ |
| 2 | u16 | flags: 0x0008 = may subscribe to own TX channels; 0x0400 = rename prohibited (netaudio) | 0x0006 | 0x0006 / 0x000F | ✅ values / ⚠️ bits |
| 4 | u16 | ptr → common channel descriptor | | 0x0050 | ✅ |
| 6 | u16 | ptr → subscribed TX channel name (0 = none) | | | ✅ |
| 8 | u16 | ptr → subscribed TX device name (0 = none; `.` = self) | | | ✅ |
| 10 | u16 | ptr → RX channel name (label) | | | ✅ |
| 12 | u16 | receiver status: 0x0101 while a flow delivers audio, else 0x0000 | | | ✅ |
| 14 | u16 | subscription status code (table below) | | | ✅ |
| 16 | u32 | `unknown (observed 0)` | 0 | 0 | ✅ |

Inferno treats p12..p15 as one u32: 0x01010009 = unicast receiving,
0x0101000A = multicast receiving, 0x00000001 = unresolved. That is identical on
the wire. When its subscriber is not running, Inferno reports capacity 0 and
count 0.

**Subscription status codes** (u16 at +14):

| Code | Name | Meaning | Source |
|---|---|---|---|
| 0x0000 | NONE | no subscription | NAC, CAP |
| 0x0001 | UNRESOLVED (with rx status 0) | TX channel or device not found yet | INF (`Unresolved`), NAC, CAP |
| 0x0002 | RESOLVED | found, not yet processed | NAC |
| 0x0003 | RESOLVE_FAIL | | NAC |
| 0x0004 | SUBSCRIBE_SELF | connected to own TX (loopback) | NAC |
| 0x0005 | RESOLVED_NONE | channel explicitly absent | NAC |
| 0x0007 | IDLE | | NAC |
| 0x0008 | IN_PROGRESS | setting up flow | INF (`InProgress`), NAC |
| 0x0009 | DYNAMIC | receiving unicast | INF, NAC, CAP |
| 0x000A | STATIC | receiving multicast | INF, NAC, CAP |
| 0x000E | MANUAL | manual flow | NAC |
| 0x000F | NO_CONNECTION | TX unreachable | NAC |
| 0x0010 / 0x0011 | CHANNEL_FORMAT / BUNDLE_FORMAT | format mismatch | NAC |
| 0x0012 / 0x0013 | NO_RX / RX_FAIL | receiver out of flows / failed | NAC |
| 0x0014 / 0x0015 | NO_TX / TX_FAIL | transmitter out of flows / failed | INF (`TooManyTxFlows`, `TxFail`), NAC |
| 0x0016 / 0x0017 | QOS_FAIL_RX / QOS_FAIL_TX | bandwidth | NAC |
| 0x0018 | TX_REJECTED_ADDR | usually ARP failure | NAC |
| 0x0019 | INVALID_MSG | TX rejected request | NAC |
| 0x001A | CHANNEL_LATENCY | TX latency > max RX latency | NAC |
| 0x001B | CLOCK_DOMAIN | different clock subdomains | NAC |
| 0x001C | UNSUPPORTED | | NAC |
| 0x001D / 0x001E | RX_LINK_DOWN / TX_LINK_DOWN | | NAC |
| 0x001F | DYNAMIC_PROTOCOL | no suitable protocol | NAC |
| 0x0020 | INVALID_CHANNEL | | NAC |
| 0x0021 | TX_SCHEDULER_FAILURE | e.g. < 1 ms unicast on 100 Mb/s | NAC |
| 0x0022 | SUBSCRIBE_SELF_POLICY | | NAC |
| 0x0023 / 0x0024 | TX_NOT_READY / RX_NOT_READY | warnings | NAC |
| 0x0025 | TX_FANOUT_LIMIT_REACHED | | NAC |
| 0x0026 | TX_CHANNEL_ENCRYPTED | | NAC |
| 0x0027 | TX_RESPONSE_UNEXPECTED | | NAC |
| 0x0040–0x0046 | TEMPLATE_* / RX_ / TX_UNSUPPORTED_SUB_MODE | template subscriptions | NAC |
| 0x0060 / 0x0061 | TX_ACCESS_CONTROL_DENIED / _PENDING | | NAC |
| 0x0070–0x0072 | HDCP / encryption / transport unsupported | | NAC |
| 0x00FF | SYSTEM_FAIL | | NAC |

Sources: INF `proto_arc.rs` `get_receive_channels`, `device_server/channels_subscriber.rs`
(`SubscriptionStatus`); NAC `parser.rs` `parse_rx_page`, `subscription_status.rs`;
CAP `20250517_*_get_receivers_response.bin`.
✅ for 0, 1, 8, 9, 0x0A, 0x14, 0x15. ⚠️ for the rest (netaudio's labels).

### 4.15 0x3001 rename RX channels

| Variant | Content | Source | Conf |
|---|---|---|---|
| batch (netaudio) | `02 01` · `[u16 ch][u16 ptr = 0x0014]` · 4 zero bytes · label | NAC `commands/device.rs` | ✅ |
| page (DC, protocol 0x2729) | `20 20` · 32 × `[u16 ch][u16 ptr]` · labels from p 0x008C | CAP `preset/protocol_2729_opcode_3001_*`; NAC `build_receive_channel_name_page_2729` | ✅ |

Records are 4 bytes `[u16 channel][u16 ptr → label]`. Response: result 1, empty.
Inferno then multicasts an RX-channel-change notification (0x0102,
[§7.4.6](#746-0x0102-rx-channel-change-and-other-change-notifications)) for the renamed channels.
Inferno replies 0xFFFF if nothing matched.

### 4.16 0x3010 set subscriptions

| Variant | Content | Source | Conf |
|---|---|---|---|
| batch (netaudio, protocol 0x27FF, ≤ 16 records) | `02 n` · n × `[u16 rx ch][u16 ptr → TX channel][u16 ptr → TX device]` · zero pad so strings start at p ≥ 52 · strings | NAC `commands/subscriptions.rs` `build_add_subscriptions` | ✅ |
| page (DC, protocol 0x2729, ≤ 32) | `20 n` · n × 6-byte records · zero pad · string heap at **fixed p = 0x028C** (strings de-duplicated) | CAP `subscription/protocol_2729_opcode_3010_*`, `preset/…3010…`; NAC `build_subscription_page_2729` | ✅ |

A record whose two pointers are both 0 **clears** that RX channel's subscription.
Dante Controller uses this for "unsubscribe"; the capture has 32 such records in
a 652-byte packet. Response: result 1, empty content.

Inferno's behaviour: it records the subscription immediately with status
UNRESOLVED (1), because DC re-reads 0x3000 right after the reply. It then
resolves asynchronously ([§10](#10-subscription-workflow)). If its subscriber is
not running it does not answer.

Sources: INF `proto_arc.rs` `set_channels_subscriptions`, `arc_server.rs`,
`channels_subscriber.rs`.

### 4.17 0x3014 remove subscriptions

Content `[u16 n][n × u32 rx channel number]`. Inferno's captured example:
`27ff 0010 4a1c 3014 0000 | 0001 | 00000002` removes RX channel 2. netaudio
frames this as a u32 count overlapping the result field; the bytes are the same.
Inferno handles only the first entry. Response: result 1, empty.

Sources: INF `arc_server.rs` (comment with the captured packet); NAC
`build_remove_subscriptions`. ✅ format / ⚠️ multi-entry.

### 4.18 0x3200 RX flows

Request: paged (`[1][first][0]`). Response:
`[u8 capacity 16][u8 count][capacity × u16 ptr]` plus records.

**Record (legacy "20-byte" layout):**

| Off | Size | Field | Inferno | lx-dante (CAP) | Conf |
|---|---|---|---|---|---|
| 0 | u16 | flow number | | 1, 2, 3, 5 | ✅ |
| 2 | u16 | flags. Observed 1. Bit 0x4000 selects a 48-byte "modern" record (netaudio). | 1 | 0x0001 | ✅ |
| 4 | u32 | sample rate | | 48000 | ✅ |
| 8 | u32 | encoding. Inferno writes u16 0 + u16 bits. | | 24 | ✅ |
| 12 | u16 | interface count I | 1 | 1 | ✅ |
| 14 | u16 | channel slot count N | | 2 | ✅ |
| 16 | u16 | bitmap words W per slot | ⌈rx/16⌉ | 8 | ✅ |
| 18 | I × u16 | ptr → receiver endpoint (socket descriptor of **this receiver's** IP and RX port; multicast: group:port) | | `08 02 3813 c0a8016c` / `08 02 10e1 efffff38` | ✅ (`08 02`, see [§3.4](#34-content-conventions)) |
| … | N × u16 | ptr → bitmap of local RX channels fed by slot k. Bit b of word w set means RX channel 16w+b+1 (LSB first). | | | ✅ |
| … | u16 | ptr → status descriptor | | | ✅ |

**Status descriptor (16 bytes):**

| Off | Size | Field | Inferno | CAP | Conf |
|---|---|---|---|---|---|
| 0 | u16 | status code (same table as [§4.14](#414-0x3000-rx-channels)) | 9 | 9 (unicast) / 0x0A (multicast) | ✅ |
| 2 | u16 | interface state bitmap | 1 | 1 / 0 | ✅ |
| 4 | u16 | `flags (observed 0x0800)` | 0x0800 | 0x0800 | ✅ |
| 6 | u16 | `unknown (0)` | 0 | 0 | ✅ |
| 8 | u32 | latency ns | from flow | 1 000 000 / 2 000 000 | ✅ |
| 12 | u16 | transport: 0 = Dante, 3 = external RTP | 0 | 0 | ✅ |
| 14 | u16 | ptr → external-RTP identity (transport 3) | 0 | 0 | ⚠️ |

> ❗ **Layout order.** Real devices write each record **header first**, followed
> by its endpoint, bitmaps and status. netaudio *requires* every sub-object to
> lie after the record's pointer table and before the next record. Inferno
> writes the sub-objects *before* the header, which Dante Controller accepts
> but netaudio rejects. **OpenVirtualSoundcard should use header-first order.**

Sources: INF `proto_arc.rs` `query_rx_flows`, `arc_server.rs`, `flows_rx.rs`;
NAC `responses/flows.rs` `parse_receiver_flow_page`, `parse_receiver_flow_record`;
CAP `receiver_flow_inventory/protocol_2729_opcode_3200_id_8172.bin`.

### 4.19 0x3300 RX port ranges

Request: empty. Response content (8 bytes):
`[u16 range1 first][u16 range1 last][u16 range2 first][u16 range2 last]`.
Real (DC ↔ lx-dante): `3800 397F 3980 39FF` (14336–14719, 14720–14847).
Inferno sends `3800 38FD 38FE 38FF` and notes the reply is needed to avoid a
"clock domain mismatch" error in Dante Controller. Real receivers take unicast
RX ports from range 1 (e.g. 0x3801, 0x3813, 0x3829). The second range's role
is ❓. Inferno itself uses ephemeral ports and still interoperates.

Sources: INF `arc_server.rs`; NAC `responses/flows.rs` `parse_receiver_port_ranges`;
CAP `receiver_port_ranges/*`. ✅ format / ⚠️ semantics.

### 4.20 Modern ARC 2.8.x opcodes

Only devices advertising `arcp_vers` ≥ 2.8.x receive these. A device that
advertises 2.7.41 should not. They use segmented records, media-type selectors
(3 = audio, 4 = video, 5 = ancillary) and 24-byte query bodies.

| Opcode | Legacy equivalent | Notes | Source |
|---|---|---|---|
| 0x2400 | 0x2000 | TX channel status page | NAC `commands/flows.rs`, `responses/channel_status.rs` |
| 0x2438 | — | reconcile TX channel names | NAC `commands/mod.rs` |
| 0x2600 / 0x2601 / 0x2602 | 0x2200 / 0x2201 / 0x2202 | flow inventory, create, delete. Device allocates the id. | NAC `commands/multicast.rs`; CAP fixtures `multicast_creation_2809*.json` |
| 0x3400 / 0x3401 | 0x3000 / 0x3001 | RX channel status / rename | NAC; CAP `receiver_channel_status/*` |
| 0x3410 | 0x3010 | subscription page with 8-byte records `[rx ch][media type][ptr ch][ptr dev]` | NAC `build_modern_arc_subscription_page` |
| 0x3600 | 0x3200 | RX flow status | NAC; CAP `receiver_flow_status/*` |

All ⚠️ (netaudio only, with captures). Not needed for a 2.7.41 device.

---

## 5. CMC (port 8800)

### 5.1 0x1001 registration / device advertisement

A controller registers with every device it manages, and netaudio repeats this
every **10 s** (NAP `dante/services/cmc.py`).

Request (protocol 0x1200, opcode 0x1001):

| c | Size | Field | Conf |
|---|---|---|---|
| 0 | u16 | 0 | ⚠️ |
| 2 | 6 | controller MAC | ⚠️ |
| 8 | u16 | 0 | ⚠️ |

Response (result 1), 22 bytes of content:

| c | Size | Field | Inferno | Conf |
|---|---|---|---|---|
| 0 | u16 | process id | process id | ✅ |
| 2 | 8 | device id, the same value as CMC TXT `id` | `00 00 a b c d pp pp` | ✅ |
| 10 | u16 | `unknown1 (observed 1)` | 1 | ✅ |
| 12 | u16 | `unknown2 (observed 0)` | 0 | ✅ |
| 14 | 4 | device IPv4 | own IP | ✅ |
| 18 | u16 | settings (conmon) port, 8700 | info-request port | ✅ |
| 20 | u16 | `unknown3 (observed 0)` | 0 | ✅ |

netaudio's virtual device encodes the same bytes (with process id 0 and the
device id built from the IP), and its parser checks only the sequence number
and result 1. Inferno ignores the request content.

Sources: INF `protocol/proto_cmc.rs`, `device_server/cmc_server.rs`; NAC
`commands/mod.rs` `build_cmc_register`, `publications.rs` `CmcRegistration`,
`responses/device.rs` `parse_cmc_registration_response`.
✅ (two implementations; Inferno interop-tested).

### 5.2 0x3010 metering start / stop (on the CMC port)

Not to be confused with ARC 0x3010. The controller asks the device to stream
level meters to `controller IPv4 : port` (8751 unicast, 8752 multicast).
Content: `[u16 0][controller MAC 6][u16 0][u16 4][u16 ptr][u16 2][u16 ptr][u16 0x000A][controller name\0, padded to even][u16 1][u16 1][u16 ptr][u16 1][u16 port][u16 1][u16 0][IPv4][u16 port][6 × 0][u16 port][2 × 0]`.
"Stop" is the same packet with the last 16 bytes zeroed. Metering frames
([§7.8](#78-metering-frames)) are conmon-framed. Inferno does not implement this.

Sources: NAC `commands/metering.rs` (checked against CAP
`metering/controller_start.bin`). ⚠️

---

## 6. Flow control / DBCP (port 4455)

DBCP is how a **receiver** asks a **transmitter** for a unicast flow. It uses
the 10-byte framing of [§3](#3-requestresponse-framing-arc-cmc-dbcp). The start
code is the TX's `dbcp1` TXT value. Inferno's client always sends 0x1102, and
its server accepts any start code and echoes it. Dante Virtual Soundcard
advertises 0x1200 ([§2.2](#22-txt-keys)). ✅

### 6.1 Opcodes

| Opcode | Name | Request content | Success response |
|---|---|---|---|
| 0x0100 | request flow | [§6.2](#62-0x0100-request-flow) | result 1, content = 6-byte handle |
| 0x0101 | stop flow | 6-byte handle | result 1, empty |
| 0x0102 | update channels | handle(6) + `u16 N` + N × u16 TX channel ids | result 1, empty |

### 6.2 0x0100 request flow

N = number of channel slots requested (≤ TX `nchan`). Every pointer is a
packet offset.

| p | c | Size | Field | A32 capture | Conf |
|---|---|---|---|---|---|
| 0 | — | u16 | start code (`dbcp1`) | 0x1102 | ✅ |
| 2 | — | u16 | length | 0x0050 | ✅ |
| 4 | — | u16 | sequence | 0x0000 | ✅ |
| 6 | — | u16 | opcode 0x0100 | | ✅ |
| 8 | — | u16 | 0 | | ✅ |
| 10 | 0 | u16 | ptr → receiver device name, the start of the heap: `0x30 + 2N` | 0x0038 | ✅ |
| 12 | 2 | u32 | sample rate | 48000 | ✅ |
| 16 | 6 | u32 | bits per sample | 24 | ✅ |
| 20 | 10 | u16 | `unknown (observed 1)`. Inferno warns if it is not 1. | 1 | ✅ |
| 22 | 12 | u16 | N | 4 | ✅ |
| 24 | 14 | u16 | ptr → receiver socket descriptor (8-aligned) | 0x0048 | ✅ |
| 26 | 16 | N × u16 | TX channel id (TXT `id`) per slot; 0 = unused slot | 1, 0, 0, 0 | ✅ |
| 26+2N | 16+2N | u16 | ptr → trailer, always `0x1C + 2N` | 0x0024 | ✅ |
| 28+2N | 18+2N | u16 | trailer +0: `0x0A00` (length 10 words) | 0x0A00 | ✅ |
| 30+2N | 20+2N | u16 | trailer +2: `unknown (observed 0x0002)` | 0x0002 | ✅ |
| 32+2N | 22+2N | u16 | trailer +4: **frames per packet** | 16 | ✅ |
| 34+2N | 24+2N | u16 | trailer +6: ptr → receiver's flow name | 0x0043 | ✅ |
| 36+2N | 26+2N | 12 | trailer +8..+19: `unknown (observed all zero)` | zero | ✅ |
| 48+2N | 38+2N | str | receiver device name + NUL | `A32-000001` | ✅ |
| … | | str | receiver flow name + NUL. Inferno: `<local flow id>_<process id>`; A32: `1`. | `1` | ✅ |
| … | | 0–7 | zero padding to an 8-byte packet offset | | ✅ |
| ptr | | 8 | socket descriptor `08 02 <rx port> <rx IPv4>`: where to send the audio | `08 02 3801 0afe4e0b` | ✅ |

The captured A32 request (CAP
`receiver_port_ranges_fallback/protocol_1102_opcode_0100_id_15.bin`, a Ferrofish
A32 with Brooklyn II firmware) matches Inferno's encoder and decoder byte for
byte. ✅

> ⚠️ netaudio's test labels `p24` "transport descriptor pointer" (agrees) and
> `p26` "transport descriptor count". In the capture `p26` is 1 only because the
> first slot requests TX channel 1. Inferno's slot-list reading is the one
> interop-tested.

### 6.3 Response: flow handle

Result 1 with 6 bytes of content: the handle. It is opaque to the receiver,
which stores it for 0x0101 and 0x0102. Inferno builds it as a u32 internal flow
index plus a u16 random cookie and validates both. Real devices' handle
structure is ❓. (INF `flows_control.rs`, `flows_tx.rs`)

### 6.4 0x0101 stop flow

Content = handle. Unknown handle → 0x0103. Inferno's receiver ignores errors
here: when it stops sending keepalives, the TX times the flow out anyway. ✅

### 6.5 0x0102 update channels

Content: `handle (6) | u16 N | N × u16 TX channel id (0 = clear slot)`.
Inferno's receiver uses it to fill free slots of an existing flow instead of
opening another. On the TX side it also revives a flow that expired from
missing keepalives. Unknown handle → 0x0103. ✅

**A Dante AVIO refuses to grow a flow** (seen with an AVIO-DAI2, firmware of
2025, on a real network): a flow requested with one channel id in 0x0100
could not take a second channel through 0x0102, and the second subscription
stayed red in Dante Controller. OpenVirtualSoundcard's receiver therefore requests each
flow with **all of its slots**, the unused ones as channel 0, so that 0x0102
only fills slots the flow already has; if a transmitter still refuses, the
channel gets a 0x0100 flow of its own. ✅ (interop)

### 6.6 Errors

The result code carries the error. Content is empty.

| Code | Meaning | Source | Conf |
|---|---|---|---|
| 0x0103 | flow not found / stream expired (no keepalives) | INF `flows_control.rs` | ⚠️ |
| 0x0301 | sample-rate mismatch | INF | ⚠️ |
| 0x0315 | too many TX flows | INF | ⚠️ |
| 0x0302 | Inferno's own choice for "fpp > 256" and "channel id out of range" ("TODO") | INF `flows_control_server.rs` | ❓ |

A receiver should map DBCP failures to RX status 0x0014 NO_TX or
0x0015 TX_FAIL. Inferno always uses 0x15.

### 6.7 Transmitter (server) behaviour (Inferno)

| Step | Behaviour | Conf |
|---|---|---|
| Validate | Sample rate must equal the device rate, else 0x0301. fpp ≤ 256. Channel ids ≤ TX count. | ⚠️ |
| Same destination | If a flow to the same receiver IP:port already exists, treat the request as a channel update and return the **existing** handle. | ⚠️ |
| Socket | Open a UDP socket on the device IP with an **ephemeral source port**, connected to the receiver's IP:port. Audio leaves from this port and keepalives arrive on it. | ✅ |
| Timing | Start at the next timestamp that is a multiple of fpp ([§8.4](#84-timestamps-and-ptp)). | ✅ |
| Keepalive | Any datagram received on the flow socket refreshes a **4 s** expiry. An expired flow stops sending until 0x0102 revives it, and is reclaimed on the next 0x0100. | ✅ (interop) |
| Limits | 32 flows (else 0x0315); ≤ 8 slots advertised; fpp 2..256 accepted (TXT advertises `32,2`). | ⚠️ |

Sources: INF `device_server/flows_control_server.rs`, `flows_tx.rs`.

### 6.8 Receiver (client) behaviour (Inferno)

Send to the TX's `_netaudio-chan` SRV address and port (4455). Wait up to 3 s
for a reply whose opcode **and** sequence match, ignoring stray packets.
Number sequences from 1, incrementing per request. See [§10](#10-subscription-workflow)
for the whole flow. (INF `protocol/flows_control.rs`) ✅

---

## 7. Conmon / settings (8700, 8702, 8708)

"Conmon" (control and monitoring, netaudio's name) uses a 32-byte header with
start code 0xFFFF. Requests go **unicast to the device on UDP 8700**. Status
replies and unsolicited notifications are **multicast to 224.0.0.231:8702**.

### 7.1 Header (32 bytes)

| p | Size | Field | Notes | Conf |
|---|---|---|---|---|
| 0 | u16 | start code | 0xFFFF conmon, 0xFFFE heartbeat | ✅ |
| 2 | u16 | total length | | ✅ |
| 4 | u16 | sequence | Per sender. Inferno increments it per message across all types. netaudio requires non-zero for writes. | ✅ |
| 6 | u16 | process id | Inferno writes its process id. **netaudio rejects packets where this is not 0.** Real captures: 0. | ✅ |
| 8 | 8 | device id | Device messages: own id, e.g. `001dc1081258 0000` or `001dc1fffe50692e`. Controller requests: a MAC + `0000`, all zero (identify), `52 54 00 00 00 00 00 00` ("RT", sample-rate/encoding writes), or the target device's MAC. Probably ignored. | ✅ / ⚠️ |
| 16 | 8 | vendor | ASCII `Audinate`. netaudio requires it exactly. Inferno truncates its vendor string to 8 bytes. | ✅ |
| 24 | u16 | record revision | Version of the record format. Devices: 0x0724, 0x0727, 0x072E, 0x0738; Inferno 0x072A. Requests: 0x0731, 0x073A, 0x0734, 0x0727, 0x073E, 0x0724. Several decoders change layout by revision. netaudio's request builder splits it into `07` + "suffix". | ✅ |
| 26 | u16 | message type (opcode) | [§7.3](#73-message-types-request--response--notification) | ✅ |
| 28 | u32 | request value | Requests: usually 100 (0x64), sometimes 0. Device messages: 0. | ✅ |
| 32 | … | body | | |

Inferno matches requests on bytes 24..31 as `07 ?? 00 TT 00 00 00 ??`, i.e. any
revision low byte and any final byte. (INF `info_mcast_server.rs`)

Sources: INF `protocol/mcast.rs`; NAC `protocol.rs` `conmon_opcode`,
`commands/mod.rs` `settings_packet`, `commands/clock.rs`; CAP
`protocol_packets.json` (`protocol_FFFF_*`), `core_responses_golden.json`.

### 7.2 Transport behaviour

| Item | Behaviour | Source | Conf |
|---|---|---|---|
| Replies | Inferno sends every status message to 224.0.0.231:8702 **from its 8700 socket**. netaudio's virtual device also replies unicast to the requester for 0x0061 and 0x00C1. | INF; NAP `dante/virtual_device_requests.py` | ✅ / ⚠️ |
| Fire-and-forget | Sample rate, encoding, reboot and identify writes get no direct reply. Confirmation arrives as a multicast status or notification. | NAP `capture/fact.py` | ⚠️ |
| Start-up | Inferno announces 0x0060 (board) and 0x00C0 (product) once at start-up. | INF | ✅ |
| Group 224.0.0.231:8700 | netaudio's virtual device also joins 224.0.0.231 on port 8700, implying some requests may arrive multicast. | NAP `virtual_device.py` | ❓ |

### 7.3 Message types (request → response / notification)

Status messages double as notifications. The netaudio notification ids are the
message-type numbers in decimal (NAP `core/_abi.py`).

| Request | Reply / notification type | Dec | Name | Inferno | Source | Conf |
|---|---|---|---|---|---|---|
| — | 0x0010 | 16 | topology change | — | NAP `_abi.py` | ⚠️ |
| 0x0013 | 0x0011 | 17 | interface status (network info) | ✔ | INF; NAC; CAP | ✅ |
| 0x0015 | 0x0014 | 20 | switch configuration | — | NAC | ⚠️ |
| 0x0021 | 0x0020 | 32 | clock config / clocking status | ✔ | INF; NAC; CAP | ✅ |
| — | 0x0022 / 0x0024 / 0x0026 | | clock master / unicast / identifier status | — | NAC `responses/conmon.rs` | ⚠️ |
| 0x0041 | 0x0040 | 64 | interface statistics | — | NAC | ⚠️ |
| 0x0061 | 0x0060 | 96 | "Dante model" / platform versions (board info) | ✔ | INF; NAC; CAP | ✅ |
| 0x0063 | — | | identify (blink) | — | NAC `build_identify` | ⚠️ |
| 0x0077 | 0x0078 | 120 | clear configuration (status) | ✔ (status only) | INF; NAC | ✅ |
| 0x0081 | 0x0080 | 128 | sample rate (probe / set) | — | NAC; CAP | ✅ |
| 0x0083 | 0x0082 | 130 | encoding (probe / set) | — | NAC; CAP | ✅ |
| 0x0085 | 0x0084 | 132 | sample-rate pull-up | — | NAC | ⚠️ |
| — | 0x0086, 0x00E0, 0x0106 | | unmapped status records | — | NAC | ❓ |
| 0x0090 | 0x0092 | 146 | system reset (reboot / factory) / device reboot | — | NAC; CAP | ✅ request |
| 0x00C1 | 0x00C0 | 192 | manufacturer versions (make / model / product info) | ✔ | INF; NAC; CAP | ✅ |
| — | 0x0100 | 256 | routing ready (netaudio also calls it "routing capacity status") | — | NAP, NAC | ⚠️ |
| — | 0x0101 | 257 | TX channel change | — | NAP | ⚠️ |
| — | 0x0102 | 258 | **RX channel change** (bitmask) | ✔ sends | INF `mcast.rs`; NAC | ✅ |
| — | 0x0103 | 259 | TX label change | — | NAP | ⚠️ |
| — | 0x0104 / 0x0105 | 260 / 261 | TX flow change / RX flow change | — | NAP | ⚠️ |
| — | 0x0106 | 262 | property change | — | NAP | ⚠️ |
| — | 0x0120 | 288 | routing device change (e.g. rename) | — | NAP | ⚠️ |
| 0x1006 | 0x1007 | 4103 | AES67 (set / status) | — | NAC | ⚠️ |
| 0x1008 | 0x1009 | 4105 | lock-reset status | — | NAC | ⚠️ |
| 0x100A | 0x100B | 4107 | codec / gain | — | NAC | ⚠️ |
| — | 0x100E | 4110 | settings change (also "panel status") | — | NAP, NAC | ⚠️ |
| 0xFF04 | 0xFF05 | | diagnostic export fragments | — | NAC `conmon_export.rs` | ⚠️ |

How netaudio reacts (NAP `dante/application.py`, `dante/state.py`):
257, 259 and 262 trigger a TX-channel re-read; 258 and 262 an RX re-read;
258, 261 and 288 a subscription re-read; 288 and 4110 a device-name re-read;
260 and 261 a flow re-read. **OpenVirtualSoundcard should emit 258 when any RX
subscription or status changes, 257 or 259 on TX renames, and 260 or 261 on
flow changes.** ⚠️

### 7.4 Status record layouts

Offsets are record offsets `r` (`r = p − 24`). `r+0` is the revision and
`r+2` the message type.

#### 7.4.1 0x0060 "Dante model" (board info / platform versions)

Total packet 232 bytes (Inferno, AD4D, A32).

| r | Size | Field | Inferno value | Real (Shure AD4D, CAP) | Min rev | Conf |
|---|---|---|---|---|---|---|
| 0x08 | u32 | platform software version `[maj u8][min u8][patch u16]` | 4.1.6 | 4.2.0 | | ✅ |
| 0x0C | u32 | platform hardware version | 4.1.3 | 4.0.2 | | ✅ |
| 0x10 | u32 | platform API version | 0 | 4.2.1 | | ⚠️ |
| 0x14 | 8 | platform model identifier, ASCII | board name | `Bklyn2` | | ✅ |
| 0x1C | u32 | primary capabilities ([§7.4.7](#747-capability-bits-0x0060)) | 0x0000_1000 | 0x8E7C_D4CB | 0x0200 | ✅ |
| 0x20 | u32 | preferred link speed (Mb/s) | 0 | 1000 | 0x0200 | ⚠️ |
| 0x24 | u32 | device status flags | 0 | 0 | 0x0704 | ⚠️ |
| 0x28 | u32 | software version 4th field | 2 | 28 | 0x0701 | ⚠️ |
| 0x2C | u32 | hardware version 4th field | 1 | 11 | 0x0701 | ⚠️ |
| 0x30 | u32 | ROM boot version | 1.0.0 | 1.3.71 | 0x0704 | ⚠️ |
| 0x34 | u32 | supported clock-protocol flags | 0 | 1 | 0x0707 | ⚠️ |
| 0x38 | u32 | `unknown (observed 0, 0x300)` | 0 | 0x0000_0300 | | ❓ |
| 0x3C | u32 | read-only capabilities | 0 | 0 | 0x070A | ⚠️ |
| 0x40 | 128 | platform model name | board name | `Brooklyn II` | 0x070C | ✅ |
| 0xC0 | u32 | monitoring capabilities | **0x1F**. Inferno observed that a value of 0 makes controllers poll the device for info about once per second. | 0x1B | 0x0717 | ✅ |
| 0xC4 | u32 | secondary capabilities | 0 | 0x41 | 0x071E | ⚠️ |
| 0xC8 | u32 | domain capability values | 0 | 7 | 0x0723 | ⚠️ |
| 0xCC | u32 | domain capability validity | 0 | 7 | 0x0723 | ⚠️ |
| 0xD0 | u16 | plugin count; `r+0xD2` u16 ptr to 24-byte plugin records | 0 | | 0x0731 | ⚠️ |

Sources: INF `info_mcast_server.rs` `send_board_info`; NAC
`responses/device.rs` `parse_dante_model`, `responses/mod.rs`; CAP
`model_refresh/protocol_FFFF_message_0060_*`, `dante_model_*_capture`.

#### 7.4.2 0x00C0 manufacturer / product ("make model")

Total 368 bytes.

| r | Size | Field | Inferno | AD4D (CAP) | Min rev | Conf |
|---|---|---|---|---|---|---|
| 0x08 | 8 | manufacturer id, ASCII | manufacturer | `Shure` | | ✅ |
| 0x10 | 8 | product id (binary or ASCII) | board name | `00…0b` | | ⚠️ |
| 0x18 | 8 | serial id | 0 | 0 | | ⚠️ |
| 0x20 | u32 | manufacturer software version | 0 | 0 | | ⚠️ |
| 0x24 | u32 | manufacturer firmware version | crate version | 11.0.0 | | ✅ |
| 0x28 | u32 | manufacturer capabilities | 0 | 0 | 0x0606 | ⚠️ |
| 0x2C | u32 | software version 4th field | 0 | 0 | 0x0701 | ⚠️ |
| 0x30 | u32 | firmware version 4th field | 0 | 17 | 0x0701 | ⚠️ |
| 0x34 | 128 | manufacturer name | manufacturer | `Shure Inc.` | 0x0701 | ✅ |
| 0xB4 | 128 | product name | model name | `AD4D` | 0x0701 | ✅ |
| 0x134 | u32 | product version | 0 | 0.0.1 | 0x0704 | ⚠️ |
| 0x138 | str | friendly product version | — | | 0x0712 | ⚠️ |

Sources: INF `send_product_info`; NAC `parse_make_model`; CAP
`model_refresh/protocol_FFFF_message_00C0_*`. The requests 0x0061 and 0x00C1 are
bare 32-byte headers, e.g. `ffff 0020 0fdb 0000 | 000eddfd4e130000 | Audinate | 0731 0061 00000000`.

#### 7.4.3 0x0011 interface status

| p | Size | Field | Inferno | A32 (CAP) | Conf |
|---|---|---|---|---|---|
| 32 | u16 | interface count | 1 | 1 | ⚠️ |
| 34 | u16 | `unknown (0 or 1)` | 0 | 1 | ❓ |
| 36 | u32 | link speed (Mb/s) | speed | 1000 | ✅ |
| 40 | u16 | per-interface mode / flags | 1 | 1 (AVIO: 3) | ⚠️ |
| 42 | 6 | MAC | | | ✅ |
| 48 | 4 | IPv4 | | | ✅ |
| 52 | 4 | netmask | | | ✅ |
| 56 | 4 | DNS server | gateway (Inferno: "doesn't matter") | 8.8.8.8 | ✅ (CAP lab test) |
| 60 | 4 | gateway | gateway | 192.168.1.1 | ✅ |
| 64 | … | `00 18 00 30` + zeros (Inferno, A32, AVIO) / `00 00 00 0a` (lx-dante) | | | ❓ |

Sources: INF `send_network_info`; NAC `responses/network.rs`; CAP
`dhcp_dns_gateway_order.json`, `interface_status_*` in `core_responses_golden.json`.

#### 7.4.4 0x0020 clock status

| r | Size | Field | Inferno | Conf |
|---|---|---|---|---|
| 0x04 | u32 | congestion delay (µs) | 0 | ⚠️ |
| 0x08 | u16 | clock state: 0 none, 1 passive, 2 undisciplined, 3 disciplined | 3 | ⚠️ |
| 0x0A | u16 | servo state: 0 faulty, 1 reset ("PLL not locked" per Inferno), 2 synchronizing, 3 synchronized, 4 unknown, 5 delay reset, 6 none | 3 | ⚠️ |
| 0x0C | u16 | clock source | 0 | ⚠️ |
| 0x0E | u8 | preferred leader (0/1) | 0 | ⚠️ |
| 0x0F | u8 | stratum | 0x9F ("was 0xFF") | ❓ |
| 0x10 | i32 | frequency offset, ppb | measured | ✅ |
| 0x14 | 6+2 | PTPv1 device UUID (MAC) + 2 reserved | MAC + 0 | ✅ |
| 0x1C | 6+2 | master UUID + 2 | master id | ✅ |
| 0x24 | 6+2 | grandmaster UUID + 2 | master id | ✅ |
| 0x2C | … | ports / extensions (revision dependent) | 76 zero bytes | ⚠️ |

Revision 0x0100 uses a short form (port count at `r+0x14`). Inferno sends
0x0020 only when its PTP daemon publishes a master id.

**OpenVirtualSoundcard** reports clock and servo states from its follower: locked 3/3,
locking 2/2, no master 2/1 (with a zero master UUID), free-running 2/6. It
answers 0x0021 at any time and also sends 0x0020 by itself when the state
changes, so controllers follow it without asking. ⚠️

Sources: INF `send_clock_stats`; NAC `responses/clock.rs` `parse_ptp_clock_status`.

#### 7.4.5 0x0080 / 0x0082 / 0x0084 sample rate / encoding / pull-up status

The "configurable u32" record:

| r | Size | Field | lx-dante sample rate (CAP) | Conf |
|---|---|---|---|---|
| 0x08 | u16 | offset of the value vector, relative to `r` | 0x0018 | ✅ |
| 0x0A | u16 | number of values | 6 | ✅ |
| 0x0C | u32 | current value | 44100 | ✅ |
| 0x10 | u32 | requested / pending value (0 = none) | 0 | ✅ |
| 0x14 | u16 | update mode: 0 fixed, 1 or 2 writable (pre-0x0501: reboot required) | 2 | ✅ |
| 0x16 | u16 | 0 | 0 | ✅ |
| 0x18 | n × u32 | supported values | 44100 48000 88200 96000 176400 192000 | ✅ |

For pull-up (0x0084) at revision ≥ 0x070F, `r+0x1C` is a u32 of flags
(bit 0 = disabled by host) and the vector follows it. Encoding example (CAP):
current 24, values `24 16 32`.

Sources: NAC `responses/conmon.rs` `parse_configurable_u32_status`,
`publications.rs` (`Audio`); CAP `sample_rate_status_*`, `encoding_status_*`;
INF (commented capture in `info_mcast_server.rs`). Inferno does **not** support
changing sample rate or encoding.

**OpenVirtualSoundcard** answers 0x0081 and 0x0083 with 0x0080 and 0x0082: its six
sample rates and the encodings 16, 24 and 32. When its owner can apply a
change (the macOS daemon), it sets the capability bits 0x08 and 0x10 in
0x0060 and update mode 2, takes a set request (`u32 1`, `u32 value`), reports
the value as pending and restarts with it; otherwise the mode is 0 (fixed)
and set requests are ignored. ⚠️ (not yet tried with Dante Controller)

#### 7.4.6 0x0102 RX channel change and other change notifications

Inferno sends message type **0x0102** (revision 0x072A, start code 0xFFFF) with
body `[u16 L][L bytes bitmask]`. Bit `i % 8` of byte `i / 8` is set when RX
channel index i (0-based) changed. With nothing to report it sends L = 1 and
a zero byte. netaudio reads the same thing as "u32 0 at p28, u16 count at p32,
bytes from p34". Inferno sends it after subscription state changes and RX
renames. ✅ (INF `protocol/mcast.rs` `make_channel_change_notification`; NAC
`parse_unmapped_0102_status`)

Bodies of the other change notifications (0x0101, 0x0103–0x0106, 0x0120) are
not documented. Sending the same bitmask shape is a reasonable guess. ❓

#### 7.4.7 Capability bits (0x0060)

| Field | Bit | Meaning | Source | Conf |
|---|---|---|---|---|
| primary | 0x0000_0001 | identify supported | NAC; INF comment | ✅ |
| primary | 0x0000_0008 | sample-rate configurable | NAC; INF | ✅ |
| primary | 0x0000_0010 | encoding configurable | NAC; INF | ✅ |
| primary | 0x0000_0200 | sample-rate pull-up | NAC | ⚠️ |
| primary | 0x0000_1000 | "has manufacturer name" (Inferno sets only this) | INF | ⚠️ |
| primary | 0x0000_2000 | switch redundancy | NAC | ⚠️ |
| primary | 0x0000_4000 | static IPv4 configurable | NAC; INF ("network configurable") | ✅ |
| primary | 0x0000_8000 | detailed metering | NAC | ⚠️ |
| primary | 0x0400_0000 | AES67 | NAC; INF | ✅ |
| primary | 0x0800_0000 | device locking | NAC; INF | ✅ |
| primary | low byte, other bits | Inferno: "identify, sample rate & encoding, reboot, factory reset (was 0xDB)" | INF | ❓ |
| read-only | 0x80 / 0x2000 / 0x4000 | external word clock / switch redundancy / static IPv4 are read-only | NAC | ⚠️ |
| monitoring | 0x01 / 0x02 / 0x04 / 0x08 / 0x10 | interface stats / clock / per-channel signal presence / RX-flow max latency / late packets | NAC; INF (sets 0x1F) | ✅ |

### 7.5 Settings commands (writes and probes)

The header fields and revisions below are what netaudio sends. The device-id
field is usually the controller MAC + `0000`. Bodies start at p32.

| Action | Rev | Type | p28 (u32) | Body | Conf |
|---|---|---|---|---|---|
| Probe sample rate | 0x073A | 0x0081 | 100 | `u32 0 (mode probe)`, `u32 0` | ✅ (CAP replies) |
| **Set sample rate** | 0x0727 | 0x0081 | 100 | `u32 1 (mode set)`, `u32 rate`. Device-id field `52 54 00 00 00 00 00 00`. | ⚠️ |
| Probe encoding | 0x073A | 0x0083 | 100 | `u32 0`, `u32 0` | ⚠️ |
| **Set encoding** | 0x0727 | 0x0083 | 100 | `u32 1`, `u32 bits (16/24/32)` | ⚠️ |
| Pull-up probe / set | 0x073A | 0x0085 | 0 | `u32 flags (0 probe, 1 set)`, `u32 value (0 none, 1 +4.1667 %, 2 +0.1 %, 3 −0.1 %, 4 −4.0 %)`, `4 × u32 0` | ⚠️ |
| **Identify** | 0x0731 | 0x0063 | 100 | none. Device-id field all zero. | ⚠️ |
| **Reboot** / factory reset | 0x073A | 0x0090 | 100 | `u16 1 (present)`, `u16 mode (0 reboot, 1 factory)` | ✅ (CAP `reboot/protocol_FFFF_message_0090_id_27657.bin`) |
| Clear configuration | 0x073E | 0x0077 | 100 | `u32 action (0 probe, 1 clear all, 2 keep IP settings)`. Status 0x0078: `u32 supported-modes mask`, `u32 executed mode`. Inferno replies `3, 0`. | ⚠️ |
| AES67 enable / probe | 0x0734 / 0x073A | 0x1006 | 100 | `u16 present`, `u16 enable` | ⚠️ |
| Gain level (2-channel codec products) | 0x073A | 0x100A | 0 | `u16 1, u16 1, u16 12, u16 16, u16 dir (0x0102 in, 0x0201 out), u16 0, u32 1<<(ch-1), u32 level 1..5` | ⚠️ |
| Codec status probe | 0x073A | 0x100A | 0 | `u32 0, u32 0` | ⚠️ |
| Interface status probe | 0x073A | 0x0013 | 100 | 8 × 0 | ✅ |
| Interface statistics | 0x073A | 0x0041 | 0 (8- or 30-byte body) | zeros | ⚠️ |
| Switch configuration | 0x073A | 0x0015 | 100 | `u32 0` | ⚠️ |
| Lock-reset status | 0x073A | 0x1008 | value | — | ⚠️ |
| Clock control / refresh | 0x073A or 0x0734 | 0x0021 | 100 | `r+8` u16 mask (bit 0 clock source, bit 1 preferred leader, bit 3 subdomain); `r+10` source; `r+12` u8 preferred; `r+16..32` subdomain name; `r+32` u16 extended mask / `r+34` values (0x0001 global unicast delay req, 0x0004 follower only, 0x0008 PTPv1 enabled, 0x0010 PTPv2 enabled, 0x0200/0x0400 aggregate v1/v2 unicast delay req); rev 0x073A adds `r+40` mask and v2 domain, priorities, DSCP, per-port records. An empty mask means "refresh status"; Inferno answers it with 0x0020. | ⚠️ |
| Diagnostic export | 0x0724 | 0xFF04 | 0 | `tag[4]`, `u16 selector`, `u16 0` | ⚠️ |

Sources: NAC `commands/settings.rs`, `commands/clock.rs`, `commands/mod.rs`;
INF `info_mcast_server.rs`.

### 7.6 Notification / status ID table (decimal)

16 topology change · 17 interface status · 32 clocking status · 96 versions
(0x0060) · 120 clear-config status · 128 sample rate · 130 encoding · 132
pull-up · 146 device reboot · 192 manufacturer versions · 256 routing ready ·
257 TX channel change · 258 RX channel change · 259 TX label change · 260 TX
flow change · 261 RX flow change · 262 property change · 288 routing device
change · 4103 AES67 status · 4107 codec status · 4110 settings change.
(NAP `core/_abi.py`) ⚠️ for names, ✅ for the numbers being message types.

### 7.7 Heartbeat (224.0.0.233:8708)

Sent every **1 s**. The 32-byte header uses start code **0xFFFE**, the device
id at p8 (netaudio uses it as the device EUI-64) and `Audinate` at p16. Bytes
24..31 are `00 08 00 01 10 00 00 00` (Inferno and netaudio's virtual device;
meaning ❓). Body = a sequence of TLV records:

| Off | Size | Field |
|---|---|---|
| 0 | u16 | record length in bytes, **multiple of 4** (netaudio rejects others) |
| 2 | u16 | record type |
| 4 | u16 | sub-header length S (≥ 2, observed 4) |
| 6 | u16 | payload length |
| 8 | S | sub-header: `u16 sequence` (Inferno uses the conmon sequence), `u16 0` |
| 8+S | … | payload |

| Type | Payload | Inferno | Source | Conf |
|---|---|---|---|---|
| 0x8000 | interface traffic: payload length field 4, `+12 u16 16, u16 0`, `+16 u16 entry count`, `+18 u16 entry width (16)`, entries `u32 tx bytes/s, u32 rx bytes/s, u32 tx errors, u32 rx errors`. An AVIO-DAI2 sending one 2-channel flow at 1 ms reported 455 631 B/s out and 2 036 B/s in: frame bytes per second, not bits. Dante Controller shows it as TX/RX Utilization and Errors. | not sent | NAC `heartbeat_interface_traffic.rs`; INF (commented sample); CAP (AVIO-DAI2, OpenVirtualSoundcard) | ✅ |
| 0x8001 | `i32` clock frequency offset, ppb. Inferno sends a 4-byte payload (record length 16); an AVIO-DAI2 a 16-byte one, the offset then 12 zero bytes (record length 28). | sent when a clock is available | INF; NAC `heartbeat_clock.rs`; CAP (AVIO-DAI2) | ✅ |
| 0x8002 | signal presence / peaks: `u16 tx count, u16 tx first index (0), u16 rx count, u16 rx first index (0), u16 levels offset (24, record-relative), u16 0`, then TX levels then RX levels (1 byte per channel), zero-padded to 4. Dante Controller's signal meters come from it. | sent | INF; NAC `signal_presence.rs`; CAP (AVIO-DAI2: `0002 0000 0000 0000 0018 0000 1015 0000`) | ✅ |
| 0x8003 | per-RX-flow latency: `u16 n flows, u16 first index 0, u16 entries offset (24), u16 0, u32 sample rate`, then n × u32 maximum observed latency in samples since the last heartbeat. Entry i is network interface `i / F`, receive flow id `i % F + 1`, with F the receive flow capacity (ARC 0x1000), and a real receiver sends all F × interfaces entries, 0 for idle flows. OpenVirtualSoundcard measures the latency per packet as arrival time minus the packet's timestamp. | sent | INF; NAC `heartbeat_connection_health.rs`; CAP (netaudio fixture `sequence-41132`: `0002 0000 0018 0000 0000bb80 000003ee 00000000`) | ✅ |
| 0x8004 | per-flow late-packet counters, cumulative: `u16 n, u16 first index 0, u16 entries offset (20), u16 0`, then n × u32, indexed like 0x8003 | sent | NAC; CAP (same fixture: `0002 0000 0014 0000 00000339 00000000`) | ✅ |

> ❗ **0x8002 lengths.** Inferno writes record length `24 + n` and payload
> `12 + n` *unpadded* while padding the bytes. netaudio requires both lengths
> to be multiples of 4, and its encoder uses payload = `(12 + n + 3) & ~3` and
> record = `12 + payload`. **OpenVirtualSoundcard should pad the lengths.**

An AVIO-DAI2 sends 0x8001, 0x8000 and 0x8002 in that order every second, the
records' own sequence differing from the header's; a receiver adds 0x8003 and
0x8004. OpenVirtualSoundcard sends all five in the same shapes (rebuilt byte for byte in
its tests). Dante Controller's signal meters follow 0x8002, and its Latency
tab (histogram, peak, average, late packets) follows 0x8003 and 0x8004 ✅
(interop). The Latency tab stayed empty while OpenVirtualSoundcard's 0x8003 listed only
the flows up to the highest active one and came without 0x8004.
Its levels are the peaks of the last 250 ms of each channel's ring; its
traffic comes from the network interface's counters.

Level byte (Inferno): `round(−40·log10(peak / full scale))`, clamped to 0..255.
That is 0.5 dB steps of attenuation: 0 = full scale, 255 = silence.
netaudio's encoder uses 0xFF for silence. ✅

Sources: INF `info_mcast_server.rs` `send_heartbeat`, `device_server/peaks.rs`;
NAC `heartbeat*.rs`, `publications.rs`.

### 7.8 Metering frames

Conmon-framed (0xFFFF, `Audinate` at p16). p24 = message version (1, 2 or 3).
v1/v2: p25 = TX count, p26 = RX count, levels from p27. v3: u16 counts at
p26/p28, levels from p30. Sent to the address registered through CMC 0x3010
([§5.2](#52-0x3010-metering-start--stop-on-the-cmc-port)). Real frames show 0xFE
for silent channels. ⚠️ (NAC `responses/device.rs` `parse_metering_frame`; CAP
`metering_frame_*`)

---

## 8. Audio (media) packets

Plain UDP, not RTP. The same format is used for unicast and multicast flows.

### 8.1 Header (9 bytes)

| p | Size | Field | Notes | Source | Conf |
|---|---|---|---|---|---|
| 0 | u8 | `unknown0 (observed 0x02)` | Inferno always sends 2 and ignores it on receive. | INF `flows_tx.rs`, `flows_rx.rs` | ✅ value / ❓ meaning |
| 1 | u32 | seconds | `timestamp_samples / rate` | INF | ✅ |
| 5 | u32 | sub-second sample index | `timestamp_samples mod rate`, 0..rate−1 | INF | ✅ |
| 9 | … | audio | | | |

The receiver reconstructs `ts = seconds·rate + subsec` (wrapping arithmetic)
and rejects packets shorter than 9 bytes.

### 8.2 Payload

| Item | Rule | Conf |
|---|---|---|
| Ordering | **Frame-major, interleaved.** For each frame (sample instant), one sample for every slot in flow-slot order, then the next frame. Stride = N × bytes per sample. | ✅ |
| Sample format | Signed two's complement, **big-endian**, 2, 3 or 4 bytes (16, 24 or 32 bit) as negotiated (`enc`, DBCP bits per sample). Inferno keeps samples internally as left-justified i32 and sends the top 2, 3 or 4 bytes, with optional TPDF dither when reducing depth. | ✅ |
| Empty slots | Slots with TX channel id 0 carry zeros. | ✅ |
| Size | `9 + fpp × N × bytes`. Receivers derive fpp as `(len − 9) / (N × bytes)`. | ✅ |
| MTU | Inferno keeps payloads ≤ 1400 bytes and caps fpp at ⌊1400 / (N × bytes)⌋ (e.g. 8 ch × 24 bit → fpp ≤ 58). | ⚠️ |

README note in Inferno: "Dante multicasts are … just cut off 9 bytes at the
front of every UDP packet" to get raw L24 audio. ✅

### 8.3 Frames per packet

The transmitter advertises `fpp=<max>,<min>` (Inferno `32,2`; DVS `48,48`).
The receiver chooses a value in range and sends it in the DBCP request
(trailer +4). Inferno picks `min(tx max, MTU limit)` for the lowest overhead.
The A32 receiver asked for 16, and AVIO devices report 16 in properties
0x0210/0x0310. A multicast flow's fpp is fixed by the transmitter (TXT `fpp`,
0x2200 extension +8). ✅

### 8.4 Timestamps and PTP

| Rule | Detail | Source | Conf |
|---|---|---|---|
| Media clock | `sample_index = floor(ptp_time_ns × rate / 10⁹)`, where ptp_time is the PTP-disciplined time (Statime's exported virtual clock for Inferno). Then `seconds = sample_index / rate`, `subsec = sample_index mod rate`. | INF `media_clock.rs`, `flows_tx.rs` | ✅ |
| Packet stamp | Timestamp of the packet's **first frame**. | INF | ✅ |
| Alignment | Inferno starts a flow at the next timestamp that is a multiple of fpp and steps by exactly fpp per packet. Timestamps are therefore fpp-aligned in absolute sample time. | INF `flows_tx.rs` `bootstrap_next_ts` | ⚠️ (Dante's own alignment ❓) |
| Send time | Inferno sends a packet as soon as PTP time reaches its first frame's timestamp. Samples come from a buffer filled ahead of time by the application, or by a TX-latency offset. | INF | ⚠️ |
| −500 µs | Inferno subtracts 500 µs (24 samples at 48 kHz) from every transmitted timestamp. Its author found that Dante receivers misbehave when packets appear to come from the future, so a stamp slightly in the past is the safer error. | INF `flows_tx.rs` (`CLOCK_OFFSET_NS`) | ✅ (interop) |
| Playout | The receiver plays frame `ts` at local media time `ts + latency`, where latency = max(TX `latency_ns`, own configured latency). Inferno writes incoming samples into a ring buffer at index `ts + latency_samples` and zero-fills holes. | INF `flows_rx.rs`, `channels_subscriber.rs` | ✅ |
| Latency telemetry | The receiver measures `now − ts` per packet, keeps the maximum, and reports it in heartbeat 0x8003. | INF `flows_rx.rs`, `info_mcast_server.rs` | ✅ |
| Discontinuities | Inferno's TX re-bootstraps if it lags more than the TX latency or the clock jumps backwards by more than 192 000 samples. | INF `flows_tx.rs` | ⚠️ |

### 8.5 Keepalives (unicast only)

| Item | Detail | Source | Conf |
|---|---|---|---|
| Who | Receiver → transmitter, from the receiver's RX socket (the port given in DBCP), **to the source IP:port of the received audio**. | INF `flows_rx.rs` | ✅ (interop) |
| When | Inferno: each flow every **250 ms**, round-robin across flows, and only once audio has arrived (`last_source` known). | INF | ⚠️ |
| Content | Inferno sends 2 bytes `13 37`. Inferno's TX accepts any datagram, and Dante transmitters accept Inferno's. What Dante receivers send is ❓. | INF | ⚠️ |
| Expiry | The transmitter stops a unicast flow after **4 s** without keepalives (Inferno `KEEPALIVE_TIMEOUT_SECONDS = 4`; DBCP error 0x0103 "stream expired"). | INF `flows_tx.rs`, `flows_control.rs` | ✅ |
| Multicast | No keepalives. Multicast flows never expire. | INF | ✅ |

### 8.6 Multicast flows

Destination 239.255.x.y:4321. Inferno picks random x and y and checks for
conflicts via mDNS ([§2.4](#24-query-and-response-behaviour-dante-peers-rely-on)).
Real devices also use 239.255.x.y:4321 (CAP). Receivers bind the group:port,
join on their interface, and need no DBCP. Slot k of the bundle carries the
channel whose TXT is `b.<id>=k+1`. ✅

---

## 9. Clock (PTPv1)

Dante devices synchronise with **IEEE 1588-2002 (PTPv1)** in the default
subdomain. Devices with AES67 enabled also run PTPv2 and bridge the two.

### 9.1 Transport

UDP to 224.0.1.129. Sync and Delay_Req go to the event port 319; Follow_Up and
Delay_Resp to the general port 320. Big-endian. ✅ (STM
`statime-linux/src/socket.rs`; IEEE 1588-2002)

Observations from a real network (macOS, an AVIO-DAI2 as leader):

* **Receive timestamps must come from the kernel.** Timestamps taken in user
  space when the socket read returns made the follower step by 2 to 3 ms
  every few seconds on a busy Mac, and the clock never stayed locked.
  OpenVirtualSoundcard reads `SO_TIMESTAMP_MONOTONIC` on macOS (`mach_absolute_time` of
  the packet's arrival); elsewhere it still stamps in user space. ✅ (interop)
* Some Ethernet adapters truncate PTP datagrams: an Intel-based Thunderbolt
  adapter delivered Sync packets 12 bytes shorter than their IP length, and
  macOS dropped them (`netstat -s` counts them as "data size < data length").
  `tcpdump` shows them as `truncated-ip`. Another adapter on the same switch
  worked. ✅ (interop)
* A router or a switch with IGMP snooping and no querier may not forward
  224.0.1.129: `tcpdump udp port 319` then shows nothing at all. ⚠️

### 9.2 Common header (40 bytes)

| p | Size | Field | Value | Conf |
|---|---|---|---|---|
| 0 | u16 | versionPTP | 1. Statime checks `00 01` to distinguish v1 from v2. | ✅ |
| 2 | u16 | versionNetwork | 1 | ✅ |
| 4 | 16 | subdomain | `_DFLT` NUL-padded. A Dante "clock subdomain" changes this. | ✅ |
| 20 | u8 | messageType | 1 = event (Sync, Delay_Req), 2 = general | ✅ |
| 21 | u8 | sourceCommunicationTechnology | 1 = Ethernet | ✅ |
| 22 | 6 | sourceUuid | sender MAC | ✅ |
| 28 | u16 | sourcePortId | | ✅ |
| 30 | u16 | sequenceId | | ✅ |
| 32 | u8 | control | 0 Sync, 1 Delay_Req, 2 Follow_Up, 3 Delay_Resp, 4 Management | ✅ |
| 33 | u8 | reserved | 0 | ✅ |
| 34 | u8 | flags (high byte) | 0 | ✅ |
| 35 | u8 | flags (low byte) | bit 0 LI_61, bit 1 LI_59, bit 2 BOUNDARY_CLOCK, **bit 3 ASSIST (two-step)**, bit 4 EXT_SYNC, bit 5 PARENT_STATS, bit 6 SYNC_BURST | ✅ |
| 36 | 4 | reserved | 0 | ✅ |

Source: STM `messages_v1/header.rs`, `control_field.rs`, `mod.rs`.

### 9.3 Sync and Delay_Req body (84 bytes; total 124)

| p | Size | Field |
|---|---|---|
| 40 | 8 | originTimestamp: u32 seconds, u32 nanoseconds (meaningless in two-step Sync) |
| 48 | u16 | epochNumber |
| 50 | i16 | currentUTCOffset |
| 52 | 1 | reserved |
| 53 | u8 | grandmasterCommunicationTechnology |
| 54 | 6 | grandmasterClockUuid |
| 60 | u16 | grandmasterPortId |
| 62 | u16 | grandmasterSequenceId |
| 64 | 3 | reserved |
| 67 | u8 | grandmasterClockStratum |
| 68 | 4 | grandmasterClockIdentifier (ASCII, e.g. `DFLT`, `ATOM`, `GPS`) |
| 72 | 2 | reserved |
| 74 | i16 | grandmasterClockVariance |
| 76 | 1 | reserved |
| 77 | u8 | grandmasterPreferred (Dante "preferred leader") |
| 78 | 1 | reserved |
| 79 | u8 | grandmasterIsBoundaryClock |
| 80 | 3 | reserved |
| 83 | i8 | syncInterval (log₂ s; Statime's Delay_Req sends 0x7F) |
| 84 | 2 | reserved |
| 86 | i16 | localClockVariance |
| 88 | 2 | reserved |
| 90 | u16 | localStepsRemoved |
| 92 | 3 | reserved |
| 95 | u8 | localClockStratum (Statime follower: 255) |
| 96 | 4 | localClockIdentifier (Statime: `DFLT`) |
| 100 | 1 | reserved |
| 101 | u8 | parentCommunicationTechnology |
| 102 | 6 | parentUuid |
| 108 | 2 | reserved |
| 110 | u16 | parentPortField |
| 112 | 2 | reserved |
| 114 | i16 | estimatedMasterVariance |
| 116 | i32 | estimatedMasterDrift |
| 120 | 3 | reserved |
| 123 | u8 | utcReasonable |

Source: STM `sync_or_delay_req.rs`, `common/grandmaster_v1.rs`; IEEE 1588-2002. ✅

### 9.4 Follow_Up (12 bytes; total 52)

| p | Size | Field |
|---|---|---|
| 40 | 2 | reserved |
| 42 | u16 | associatedSequenceId |
| 44 | 8 | preciseOriginTimestamp (u32 s, u32 ns) |

✅ (STM's **deserializer** and IEEE 1588-2002). STM's serializer writes these at
p40/p42, which is inconsistent. It is never used because the fork is
follower-only.

### 9.5 Delay_Resp (20 bytes; total 60)

| p | Size | Field |
|---|---|---|
| 40 | 8 | delayReceiptTimestamp |
| 48 | 1 | reserved |
| 49 | u8 | requestingSourceCommunicationTechnology |
| 50 | 6 | requestingSourceUuid |
| 56 | u16 | requestingSourcePortId |
| 58 | u16 | requestingSourceSequenceId |

Source: STM `delay_resp.rs`. ✅

### 9.6 Two-step operation (ASSIST)

With ASSIST set, the Sync's own origin timestamp is ignored. The follower pairs
the Sync's receive time with the `preciseOriginTimestamp` of the Follow_Up that
has the same sequence id, in either arrival order. Without ASSIST it uses the
Sync's origin timestamp directly (one-step). Delay is measured with
Delay_Req/Delay_Resp. (STM `port/slave.rs` `handle_sync_v1`,
`handle_follow_up_v1`) ✅. Dante leaders send two-step: ⚠️ (implied by Statime
handling ASSIST; not stated).

### 9.7 Leader election (BMC)

PTPv1 has no Announce message. Every Sync carries the grandmaster dataset
(stratum, identifier, variance, preferred, boundary). Statime's fork feeds Syncs
into a PTPv2-style BMCA: preferred → a "preferred" priority1, otherwise a
default priority1; clockClass 248; GM identity from the 6-byte UUID; steps
removed from `localStepsRemoved`. (STM `bmc/dataset_comparison.rs`,
`port/bmca.rs`) ⚠️ The fork cannot act as a PTPv1 **leader** ("master operation
not implemented yet for PTPv1"), so OpenVirtualSoundcard as PTPv1 leader is uncharted. ❓

Dante-side knobs, set through conmon 0x0021 ([§7.5](#75-settings-commands-writes-and-probes)): preferred leader,
follower-only, PTPv1 and PTPv2 enable, subdomain name, unicast delay requests.
Status is reported in 0x0020 ([§7.4.4](#744-0x0020-clock-status)). ⚠️

### 9.8 PTPv2 / AES67 coexistence

| Fact | Source | Conf |
|---|---|---|
| Dante devices with AES67 enabled run PTPv2 (domain 0, 224.0.1.129) as well as PTPv1 and bridge them. Inferno can therefore follow PTPv2 (e.g. ptp4l) **only** if some Dante device has AES67 enabled. | INF README "Clocking options" | ✅ |
| Dante hardware uses priority1 249 in PTPv2. Statime's Inferno config uses 251. | STM `inferno-ptpv1.toml` (comment) | ⚠️ |
| Inferno can be PTPv2 leader through upstream Statime. Mixing networks needs a Dante AES67 bridge. | INF README | ⚠️ |
| AES67 streams: multicast 239.69.x.x (prefix property 0x8060), RTP L24, SAP/SDP announcements on 239.255.255.255:9875. 48 kHz only on Dante. | NAC `sap.rs`, `sdp.rs`; INF README | ⚠️ |

---

## 10. Subscription workflow

### 10.1 Receiver side (Inferno's procedure, interop-tested)

1. **Controller write.** DC sends ARC **0x3010** with `rx channel → (TX channel name, TX device name)`.
   The device stores it, marks the channel UNRESOLVED (0x0001), and replies
   result 1 at once (DC immediately re-reads 0x3000). A record with zero
   pointers, or ARC 0x3014, unsubscribes. ([§4.16](#416-0x3010-set-subscriptions))
2. **De-duplicate.** RX channels subscribed to the same `(device, channel)` are
   aliases and share one flow slot. If another local channel is already
   receiving that source, reuse it. No network traffic is needed.
3. **Resolve.** Query mDNS for `<TX channel>@<TX device>._netaudio-chan._udp.local`
   (SRV + TXT; 3 s timeout; parallel lookups staggered 8 ms). From TXT take
   `id`, `nchan`, `enc`/`en`, `fpp=max,min`, `latency_ns`, `dbcp1`, and
   `b.<bundle>` if present. On failure, stay UNRESOLVED and retry. Inferno
   retries every **9 s** while anything is unresolved.
4. **Multicast shortcut.** If the TXT has `b.<id>=<k>`, resolve
   `<id>@<TX device>._netaudio-bund._udp.local` for `a.0`, `p.0`, `nchan`,
   `enc`, `fpp`, `latency_ns`. Bind and join the group, map bundle slot k−1 to the
   RX channel, and skip to step 8. No DBCP, no keepalives.
5. **Group per transmitter.** Group the remaining channels by TX DBCP address
   (SRV of `_netaudio-chan`). Inferno drops a group if the TX's channels
   disagree on `nchan`, `enc` or `dbcp1`. It does not yet check that the
   sample rate matches (TODO in Inferno). First use free slots in **existing**
   flows from that TX via DBCP **0x0102**. Then open new flows: chunks of
   `min(nchan, RX channel count, 8)` channels, each with its own RX UDP socket
   (Inferno: ephemeral port; Dante devices: a port from 0x3300 range 1).
6. **Request.** DBCP **0x0100** to the TX: sample rate, bits, slot list of TX
   channel ids, fpp (= `min(fpp_max, MTU limit)`), RX socket, the device's name
   and a local flow name (`<flow id>_<process id>`). Store the handle. On error,
   set status **0x0015 TX_FAIL** (or 0x0014 NO_TX for 0x0315) and drop the flow.
7. **Mark in progress.** Status 0x0008 IN_PROGRESS. Multicast an RX-channel-change
   notification (0x0102, bitmask of affected channels).
8. **Receive.** Write samples at `ts + latency` with
   latency = max(TX `latency_ns`, own minimum latency).
9. **Keepalive.** Every 250 ms send a small datagram from the RX socket to the
   TX's audio source address (unicast only).
10. **Report.** Once packets flow (checked every ≤ 2 s), set status
    **0x0101/0x0009** (unicast) or **0x0101/0x000A** (multicast) and notify
    (0x0102). 0x3000 shows the TX names and status. 0x3200 lists the flow with
    its slot→RX-channel bitmaps, endpoint, status and latency. Heartbeat 0x8003
    reports measured latency.
11. **Supervise.** If a used flow gets no packets for **2 s** and is older than
    **6 s**, stop it (DBCP 0x0101; errors ignored), set its channels back to
    UNRESOLVED, notify, and re-resolve. Flows with no remaining used slots are
    stopped and closed.
12. **Persist.** Save subscriptions and restore them at start-up as UNRESOLVED.

Sources: INF `device_server/arc_server.rs`, `channels_subscriber.rs`,
`flows_rx.rs`, `mdns_client.rs`, `protocol/flows_control.rs`.

### 10.2 Transmitter side

1. Answer mDNS for every `<name>@<device>._netaudio-chan` (factory and label
   instances) and `_netaudio-bund`.
2. Serve DBCP 0x0100/0x0101/0x0102 ([§6.7](#67-transmitter-server-behaviour-inferno)).
3. Send audio ([§8](#8-audio-media-packets)). Expire unicast flows 4 s after the last keepalive.
4. Report flows in ARC 0x2200, with the receiver's device and flow names in the
   extension (+4, +6), and emit 260 TX-flow-change notifications ⚠️.
5. For multicast: ARC 0x2201 creates the flow, then advertise `_netaudio-bund`
   and add `b.<id>=<slot>` to the member channels. ARC 0x2202 withdraws it.

---

## 11. Open questions

### 11.1 Discrepancies and implementation decisions

| Topic | Inferno | netaudio / real captures | Recommendation |
|---|---|---|---|
| Socket descriptor in ARC 0x2200/0x3200 | `80 02` | `08 02` (CAP) | Send `08 02`. Accept both. |
| RX channel page (0x3000) | capacity `min(32, n)`, zero-padded, count may be smaller | capacity ≤ 16 and = count (CAP lx-dante uses 16 + 0x8112) | 16 per page, capacity = count |
| RX flow record layout (0x3200) | sub-objects before the header | header first, sub-objects inside the record span (CAP; netaudio requires it) | header first |
| 0x1003 block pointers (c2, c4) and version words | zero | real pointers and version words (CAP) | fill as lx-dante |
| TX flow extension +8 | hard-coded 0x0010 ("unknown") | frames per packet (NAC + CAP) | send real fpp |
| Heartbeat 0x8002 lengths | unpadded `24+n` / `12+n` | multiples of 4 (NAC) | pad |
| Conmon p6 (process id) | process id | must be 0 for netaudio to parse | send 0 (keep process id in CMC TXT and CMC reply) |
| DBCP p26 | first slot's TX channel id (interop-tested) | netaudio test calls it "transport descriptor count" | follow Inferno |
| 0x1100 / 0x1102 | zero-filled stubs | full property lists (CAP) | implement real properties so DC can show and set latency |
| 0x1001 set name, 0x1101 set latency, conmon 0x81/0x83 sample rate and encoding, 0x63 identify, 0x90 reboot | not implemented | formats known ([§4.3](#43-0x1001-set-device-name), [§4.6.3](#463-0x1101-write-properties-setting-latency), [§7.5](#75-settings-commands-writes-and-probes)) | implement. Replies to 0x1101 still need captures. |
| `pcm` TXT | `3 e` | real `3 e`; netaudio's encoder `3 0xe` | `3 e` |

### 11.2 Unknowns worth capturing with Wireshark

Capture with Dante Controller and at least one Dante hardware device, and
optionally DVS. Filter `udp.port in {4440 4455 8700 8702 8708 8800} or ip.dst==224.0.0.233 or ip.dst==224.0.1.129`.

- ARC 0x2320: request content and the real device reply (Inferno answers 0x0030).
- ARC 0x1101: device reply to "set latency". The meaning of the 5th record `8302 8306`, of byte `c1` (4 vs 1), and of byte `c0` in 0x1100 replies (0x24, 0x1B, …).
- Property ids marked ❓ in [§4.6.2](#462-property-ids), especially 0x0222 (= 5004?), 0x8321, 0x83F0, 0x0212/0x0312, 0x0303.
- 0x1000: `c0` capability bits other than 0x0010/0x0020/0x1000; fields at p16, p26, p28.
- 0x1003: `c0`, the 0x0500 and 0x3100 words, and the version-block words (0x0400/0x0A0A, 0x0403, 0x0100); whether c34 is really `dbcp1`.
- RX record flags 0x0002/0x0004; TX record word +2 (= 7); the u32 at RX record +16.
- 0x2010 record word +0, and whether real devices list only renamed channels.
- DBCP: the trailer words `0x0002` and the 12 zero bytes; the structure of real 6-byte handles; real error codes for bad channel or fpp; what Dante receivers send as keepalives (content and interval); whether Dante TX also expires after 4 s.
- Media header byte 0 (= 0x02): version or flags? Does any device send another value (e.g. for 32-bit or AES3)?
- Whether Dante transmitters align packet timestamps to fpp multiples, and the true send-time offset relative to the timestamp.
- 0x3300 second port range: purpose (multicast? redundancy?).
- `_netaudio-chan` `default` flag and the `at2` flag; `_netaudio-cmc` `channels=0x6000…` bitmask; whether real devices advertise `_netaudio-bund` and `in-addr.local` reservations as Inferno does.
- Conmon: who sends to 224.0.0.231:8700; whether replies go unicast as well as multicast; meaning of the device-id field in controller requests; the bodies of notifications 0x0101, 0x0103–0x0106, 0x0120, 0x100E.
- Heartbeat bytes 24–31 (`00 08 00 01 10 00 00 00`).
- 224.0.0.230 and 224.0.0.232: any traffic at all?
- PTPv1: Dante's sync interval, stratum and identifier values, preferred-leader encoding, and how a PTPv1 **leader** must behave (Delay_Resp timing, Follow_Up). Also unicast delay requests (conmon 0x0021 flag 0x0001).
- Modern ARC (2.8.x): whether Dante Controller ever uses 0x34xx/0x26xx against a device advertising 2.7.41.

---

## 12. Sources & credits

| Project | Licence | Links | What we used |
|---|---|---|---|
| **Inferno** by Teodor Woźniak | GPL-3.0-or-later or AGPL-3.0-or-later | <https://github.com/teodly/inferno> · <https://gitlab.com/lumifaza/inferno> | Device-side behaviour of every protocol; interop-tested against Dante Controller and Dante hardware. *No code copied; layouts re-described.* |
| **network-audio-controller** ("netaudio") by Chris Ritsen and contributors | Public domain / Unlicense | <https://github.com/chris-ritsen/network-audio-controller> | Controller-side encoders and decoders, the property and status-code catalogues, notification ids, and **real-device capture fixtures** (AVIO, lx-dante, Shure AD4D, Ferrofish A32, DVS for Windows). |
| **Statime** (Pendulum Project / Trifecta Tech Foundation), fork with PTPv1 | Apache-2.0 / MIT | <https://github.com/pendulum-project/statime> · fork: <https://github.com/teodly/statime> (branch `inferno-dev`) | PTPv1 message layouts and follower behaviour. |
| **Searchfire** (fork of Searchlight) | Apache-2.0 / MIT | via Inferno | mDNS responder behaviour Dante peers accept. |
| **IEEE 1588-2002** | IEEE standard | <https://standards.ieee.org/ieee/1588/3140/> | PTPv1 reference. |

Thanks to all of the above. Errors in this document are OpenVirtualSoundcard's.

**Dante** is a trademark of **Audinate Pty Ltd**. The Dante protocols are
proprietary and undocumented. This document is an independent interoperability
reference produced by observing public implementations and network traffic. It
is not endorsed by, affiliated with, or approved by Audinate, and comes with no
warranty of correctness or fitness for any purpose.

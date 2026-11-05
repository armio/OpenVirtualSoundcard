//! How subscriptions survive faults and changes, end to end on 127.0.0.1:
//! a transmitter that goes away and comes back, one that never answers or
//! can't be found, a subscription re-spelled by a controller, a transmitter
//! that needs more latency than ours, and channels left without a source.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ovsc_clock::system_clock;
use ovsc_core::client;
use ovsc_core::directory::{ResolvedChannel, StaticDirectory};
use ovsc_core::{AudioIo, Channels, Device, DeviceConfig, Ports};
use ovsc_proto::arc::{self, SubscriptionStatus};
use ovsc_proto::discovery::ChannelTxt;
use ovsc_proto::frame::Frame;

fn ports(base: u16) -> Ports {
    Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 }
}

fn config(name: &str, base: u16, process_id: u16) -> DeviceConfig {
    DeviceConfig {
        name: name.into(),
        interface: "127.0.0.1".into(),
        tx_channels: Channels::Count(4),
        rx_channels: Channels::Count(4),
        latency_ms: 10.0,
        ports: ports(base),
        discovery: false,
        process_id,
        ..Default::default()
    }
}

/// Deterministic 24-bit test signal, distinct per channel.
fn signal(ts: u64, ch: usize) -> i32 {
    let v = (ts.wrapping_mul(2654435761).wrapping_add(ch as u64 * 7919) & 0xff_ffff) as i32;
    (v - 0x80_0000) << 8
}

/// Keeps the transmit rings filled ahead of time, like an audio backend,
/// until dropped.
struct Feeder {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feeder {
    fn start(io: AudioIo) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let lead = io.sample_rate as u64 / 10;
            let mut written = io.now().unwrap();
            let mut buf = vec![0i32; 8192];
            while !flag.load(Ordering::Relaxed) {
                let until = io.now().unwrap() + lead;
                if until > written {
                    let n = ((until - written) as usize).min(buf.len());
                    for ch in 0..io.tx.len() {
                        for (i, s) in buf[..n].iter_mut().enumerate() {
                            *s = signal(written + i as u64, ch);
                        }
                        io.write_tx(ch, written, &buf[..n]);
                    }
                    written += n as u64;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Checks that `rx_ch` of `io` carries `tx_ch`'s signal from `start` to the
/// read position (`now - latency`), `delay` samples later than stamped.
fn verify_from(
    io: &AudioIo,
    rx_ch: usize,
    tx_ch: usize,
    start: u64,
    delay: u64,
) -> Result<(), String> {
    let end = io.now().unwrap() - io.latency_samples();
    if end <= start {
        return Err("nothing to read yet".into());
    }
    let mut present = 0;
    for ts in start..end {
        if let Some(s) = io.rx[rx_ch].read_one(ts) {
            present += 1;
            if s != signal(ts - delay, tx_ch) {
                return Err(format!("rx {rx_ch}: sample at {ts} is {s:#x}, want tx {tx_ch}"));
            }
        }
    }
    let want = (end - start) as usize;
    if present < want * 99 / 100 {
        return Err(format!("rx {rx_ch}: only {present}/{want} samples arrived"));
    }
    Ok(())
}

/// Checks that `rx_ch` of `io` carries `tx_ch`'s signal for the last 100 ms.
fn verify(io: &AudioIo, rx_ch: usize, tx_ch: usize) -> Result<(), String> {
    let end = io.now().unwrap() - io.latency_samples();
    verify_from(io, rx_ch, tx_ch, end - io.sample_rate as u64 / 10, 0)
}

async fn eventually(what: &str, mut check: impl FnMut() -> Result<(), String>) {
    let mut last = String::new();
    for _ in 0..200 {
        match check() {
            Ok(()) => return,
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}: {last}");
}

fn status_of(dev: &Device, rx_ch: usize) -> Result<(), String> {
    match dev.rx_channels()[rx_ch].status {
        SubscriptionStatus::ReceivingUnicast => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

/// A directory of `tx`'s channels.
fn directory_of(tx: &Device) -> StaticDirectory {
    let directory = StaticDirectory::new();
    for entry in tx.directory_entries() {
        directory.insert(entry);
    }
    directory
}

fn txt(id: u16) -> ChannelTxt {
    ChannelTxt {
        id,
        sample_rate: 48_000,
        bits_per_sample: 24,
        pcm_type: 0x0e,
        latency_ns: 1_000_000,
        fpp_max: 32,
        fpp_min: 4,
        nchan: 4,
        dbcp1: 0x1102,
        is_default_name: true,
        multicast: None,
    }
}

/// A transmitter that disappears for longer than a flow may stay silent
/// (a reboot, a cable pulled at the console) is asked again quickly, while
/// it is away and after: audio returns within about a second of it coming
/// back, not after a fixed 5 s retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transmitter_back_from_an_outage_is_heard_again_soon() {
    let clock = system_clock();
    let tx_config = config("back-tx", 24600, 21);
    let tx = Device::start(tx_config.clone(), clock.clone()).await.unwrap();
    let directory = directory_of(&tx);
    let rx = Device::start_with_directory(config("back-rx", 24610, 22), clock.clone(), directory)
        .await
        .unwrap();
    let feeder = Feeder::start(tx.audio());
    let io = rx.audio();
    rx.subscribe(1, "02", "back-tx").unwrap();
    eventually("audio on rx 1", || verify(&io, 0, 1)).await;

    drop(feeder);
    tx.shutdown().await;
    tokio::time::sleep(Duration::from_millis(5500)).await;
    assert_ne!(rx.rx_channels()[0].status, SubscriptionStatus::ReceivingUnicast);

    let tx = Device::start(tx_config, clock.clone()).await.unwrap();
    let back = Instant::now();
    let feeder = Feeder::start(tx.audio());
    eventually("audio on rx 1 again", || verify(&io, 0, 1)).await;
    let took = back.elapsed();
    assert!(took < Duration::from_millis(2500), "audio returned {took:?} after the transmitter");
    eventually("receiving status", || status_of(&rx, 0)).await;

    drop(feeder);
    rx.shutdown().await;
    tx.shutdown().await;
}

/// A subscription that can't be resolved (a powered-off wireless receiver:
/// every mDNS lookup waits 2 s) does not delay a new subscription to a
/// transmitter that is there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unresolvable_subscription_does_not_hold_up_others() {
    let clock = system_clock();
    let tx = Device::start(config("found-tx", 24620, 23), clock.clone()).await.unwrap();
    let directory = directory_of(&tx);
    directory.set_miss_delay(Duration::from_secs(2));
    let rx = Device::start_with_directory(config("found-rx", 24630, 24), clock.clone(), directory)
        .await
        .unwrap();
    let feeder = Feeder::start(tx.audio());
    let io = rx.audio();

    rx.subscribe(1, "01", "powered-off").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked = Instant::now();
    rx.subscribe(2, "03", "found-tx").unwrap();
    eventually("receiving status on rx 2", || status_of(&rx, 1)).await;
    let took = asked.elapsed();
    assert!(took < Duration::from_secs(1), "rx 2 took {took:?}");
    eventually("audio on rx 2", || verify(&io, 1, 2)).await;
    assert_eq!(rx.rx_channels()[0].status, SubscriptionStatus::Unresolved);

    drop(feeder);
    rx.shutdown().await;
    tx.shutdown().await;
}

/// A transmitter that is found but doesn't answer (its mDNS records still
/// cached after it lost power) does not delay a subscription to another
/// one, and shows as not responding (0x000F), not as refusing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_transmitter_does_not_hold_up_others() {
    let clock = system_clock();
    let tx = Device::start(config("live-tx", 24640, 25), clock.clone()).await.unwrap();
    let directory = directory_of(&tx);
    let silent = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    directory.insert(ResolvedChannel {
        device: "silent-tx".into(),
        channel: "01".into(),
        addr: Ipv4Addr::LOCALHOST,
        flow_control_port: silent.local_addr().unwrap().port(),
        txt: txt(1),
    });
    let rx = Device::start_with_directory(config("live-rx", 24650, 26), clock.clone(), directory)
        .await
        .unwrap();
    let feeder = Feeder::start(tx.audio());

    rx.subscribe(1, "01", "silent-tx").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked = Instant::now();
    rx.subscribe(2, "02", "live-tx").unwrap();
    eventually("receiving status on rx 2", || status_of(&rx, 1)).await;
    let took = asked.elapsed();
    assert!(took < Duration::from_secs(1), "rx 2 took {took:?}");

    eventually("rx 1 not responding", || match rx.rx_channels()[0].status {
        SubscriptionStatus::Other(0x0f) => Ok(()),
        other => Err(format!("{other:?}")),
    })
    .await;

    drop(feeder);
    rx.shutdown().await;
    tx.shutdown().await;
    drop(silent);
}

/// A flow that died is stopped before it is requested again: a transmitter
/// at its flow limit would refuse the new one while the old still counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_flow_is_stopped_before_it_is_requested_again() {
    use ovsc_proto::dbcp;
    use ovsc_proto::frame::{self, result};

    // Grants flows but never sends audio, so that every flow dies, and
    // takes its time to stop one.
    let fake = Arc::new(tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let port = fake.local_addr().unwrap().port();
    let log = Arc::new(std::sync::Mutex::new(Vec::<(&str, u8)>::new()));
    let seen = log.clone();
    let server = tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut handle = 0u8;
        loop {
            let (n, src) = fake.recv_from(&mut buf).await.unwrap();
            let Ok(req) = Frame::parse(&buf[..n]) else { continue };
            let h = req.header;
            let resp = match h.opcode {
                dbcp::opcode::REQUEST_FLOW => {
                    handle += 1;
                    seen.lock().unwrap().push(("request", handle));
                    dbcp::encode_flow_created(&h, [0, 0, 0, 0, 0, handle])
                }
                dbcp::opcode::STOP_FLOW => {
                    let stopped = dbcp::decode_stop_flow(&req).unwrap()[5];
                    seen.lock().unwrap().push(("stop", stopped));
                    let (fake, seen) = (fake.clone(), seen.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        seen.lock().unwrap().push(("stopped", stopped));
                        let resp = frame::encode(h.protocol, h.seq, h.opcode, result::SUCCESS, &[]);
                        let _ = fake.send_to(&resp, src).await;
                    });
                    continue;
                }
                _ => frame::encode(h.protocol, h.seq, h.opcode, result::SUCCESS, &[]),
            };
            let _ = fake.send_to(&resp, src).await;
        }
    });
    let directory = StaticDirectory::new();
    directory.insert(ResolvedChannel {
        device: "mute-tx".into(),
        channel: "01".into(),
        addr: Ipv4Addr::LOCALHOST,
        flow_control_port: port,
        txt: txt(1),
    });
    let rx = Device::start_with_directory(config("mute-rx", 24660, 27), system_clock(), directory)
        .await
        .unwrap();
    rx.subscribe(1, "01", "mute-tx").unwrap();
    eventually("the flow to be requested again", || {
        let l = log.lock().unwrap();
        if l.iter().any(|&(what, h)| what == "request" && h == 2) {
            Ok(())
        } else {
            Err(format!("{l:?}"))
        }
    })
    .await;
    let l = log.lock().unwrap().clone();
    let stopped = l.iter().position(|&e| e == ("stopped", 1));
    let again = l.iter().position(|&e| e == ("request", 2));
    assert!(stopped.is_some() && stopped < again, "{l:?}");
    rx.shutdown().await;
    server.abort();
}

/// Several dead flows to one transmitter are stopped together, not one
/// after another: a console back from a reboot may not answer stops for
/// handles it no longer knows, and each unanswered stop waits 1 s before
/// the flows are requested again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_flows_to_one_transmitter_are_stopped_together() {
    use ovsc_proto::dbcp;
    use ovsc_proto::frame::{self, result};

    // Grants flows but never sends audio, so that every flow dies, and
    // never answers a stop.
    let fake = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = fake.local_addr().unwrap().port();
    let log = Arc::new(std::sync::Mutex::new(Vec::<(&str, u8, Instant)>::new()));
    let seen = log.clone();
    let server = tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut handle = 0u8;
        loop {
            let (n, src) = fake.recv_from(&mut buf).await.unwrap();
            let Ok(req) = Frame::parse(&buf[..n]) else { continue };
            let h = req.header;
            let resp = match h.opcode {
                dbcp::opcode::REQUEST_FLOW => {
                    handle += 1;
                    seen.lock().unwrap().push(("request", handle, Instant::now()));
                    dbcp::encode_flow_created(&h, [0, 0, 0, 0, 0, handle])
                }
                dbcp::opcode::STOP_FLOW => {
                    let stopped = dbcp::decode_stop_flow(&req).unwrap()[5];
                    seen.lock().unwrap().push(("stop", stopped, Instant::now()));
                    continue;
                }
                _ => frame::encode(h.protocol, h.seq, h.opcode, result::SUCCESS, &[]),
            };
            let _ = fake.send_to(&resp, src).await;
        }
    });
    // One channel per flow, so that three channels take three flows.
    let directory = StaticDirectory::new();
    for id in 1..=3u16 {
        directory.insert(ResolvedChannel {
            device: "reboot-tx".into(),
            channel: format!("{id:02}"),
            addr: Ipv4Addr::LOCALHOST,
            flow_control_port: port,
            txt: ChannelTxt { nchan: 1, ..txt(id) },
        });
    }
    let rx =
        Device::start_with_directory(config("reboot-rx", 24730, 34), system_clock(), directory)
            .await
            .unwrap();
    for id in 1..=3 {
        rx.subscribe(id, &format!("{id:02}"), "reboot-tx").unwrap();
    }
    eventually("the flows to be requested again", || {
        let l = log.lock().unwrap();
        if l.iter().any(|&(what, h, _)| what == "request" && h > 3) {
            Ok(())
        } else {
            Err(format!("{:?}", l.iter().map(|&(w, h, _)| (w, h)).collect::<Vec<_>>()))
        }
    })
    .await;
    let l = log.lock().unwrap().clone();
    let again = l.iter().position(|&(what, h, _)| what == "request" && h > 3).unwrap();
    for old in 1..=3 {
        let stop = l.iter().position(|&(what, h, _)| what == "stop" && h == old);
        assert!(stop.is_some_and(|s| s < again), "flow {old} not stopped first: {l:?}");
    }
    // The flows die 3 s after they were granted, a millisecond apart, so
    // the once-a-second check can find one dead a tick before the others.
    // Flows found dead together are stopped together: at most two batches
    // for three flows, and the flows are requested again within about the
    // 1 s an unanswered stop takes after the last batch. Stopping them one
    // by one would space the first stops of all three 1 s apart.
    let first_stops: Vec<Instant> = (1..=3)
        .map(|old| l.iter().find(|&&(what, h, _)| what == "stop" && h == old).unwrap().2)
        .collect();
    let together = (0..3).any(|a| {
        (0..3).any(|b| {
            a != b
                && first_stops[a].max(first_stops[b]) - first_stops[a].min(first_stops[b])
                    < Duration::from_millis(100)
        })
    });
    let at: Vec<(&str, u8, Duration)> = l.iter().map(|&(w, h, t)| (w, h, t - l[0].2)).collect();
    assert!(together, "the stops were not sent together: {at:?}");
    let last_batch = *first_stops.iter().max().unwrap();
    let waited = l[again].2 - last_batch;
    assert!(
        waited < Duration::from_millis(1600),
        "requested again {waited:?} after the last stops: {at:?}"
    );
    rx.shutdown().await;
    server.abort();
}

/// Dante Controller rewrites a subscription when a channel or device is
/// renamed, or re-clicks one restored under its factory name with the
/// label. If it still names the transmit channel being received, the flow
/// and the audio carry on without a gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_respelled_subscription_keeps_its_flow_and_audio() {
    let clock = system_clock();
    let tx = Device::start(config("spell-tx", 24670, 28), clock.clone()).await.unwrap();
    let directory = directory_of(&tx);
    let first = tx.directory_entries().into_iter().find(|e| e.channel == "01").unwrap();
    // The channel's label, and the device under another name.
    directory.insert(ResolvedChannel { channel: "Vox".into(), ..first.clone() });
    directory.insert(ResolvedChannel { device: "stage-box".into(), ..first });
    let rx = Device::start_with_directory(config("spell-rx", 24680, 29), clock.clone(), directory)
        .await
        .unwrap();
    let feeder = Feeder::start(tx.audio());
    let io = rx.audio();
    rx.subscribe(1, "01", "spell-tx").unwrap();
    eventually("audio on rx 1", || verify(&io, 0, 0)).await;
    eventually("receiving status", || status_of(&rx, 0)).await;
    let flows = tx.tx_flows();
    assert_eq!(flows.len(), 1, "{flows:?}");

    for (channel, device) in [("Vox", "spell-tx"), ("01", "stage-box"), ("vox", "SPELL-TX")] {
        let before = io.now().unwrap() - io.latency_samples();
        rx.subscribe(1, channel, device).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(tx.tx_flows(), flows, "after re-subscribing as {channel}@{device}");
        verify_from(&io, 0, 0, before - 4800, 0)
            .unwrap_or_else(|e| panic!("after re-subscribing as {channel}@{device}: {e}"));
        status_of(&rx, 0).unwrap();
    }

    drop(feeder);
    rx.shutdown().await;
    tx.shutdown().await;
}

/// A transmitter that demands more latency than ours (Dante Virtual
/// Soundcard asks for 4 to 10 ms) is played at its latency, as Dante
/// receivers do (PROTOCOL.md §8.4), and its flow reports that latency.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transmitter_needing_more_latency_is_played_later() {
    let clock = system_clock();
    // It advertises its own 10 ms.
    let tx = Device::start(config("slow-tx", 24690, 30), clock.clone()).await.unwrap();
    let directory = directory_of(&tx);
    let mut rx_config = config("fast-rx", 24700, 31);
    rx_config.latency_ms = 1.0;
    let rx = Device::start_with_directory(rx_config, clock.clone(), directory).await.unwrap();
    let feeder = Feeder::start(tx.audio());
    let io = rx.audio();
    rx.subscribe(1, "04", "slow-tx").unwrap();
    // 10 ms at 48 kHz, where we run at 1 ms: 432 samples later.
    let delay = 480 - io.latency_samples();
    eventually("delayed audio on rx 1", || {
        let end = io.now().unwrap() - io.latency_samples();
        verify_from(&io, 0, 3, end - 4800, delay)
    })
    .await;

    let flows = client::arc_paged(
        Ipv4Addr::LOCALHOST,
        SocketAddr::from((Ipv4Addr::LOCALHOST, 24700)),
        arc::opcode::RX_FLOWS,
        arc::decode_rx_flows_response,
    )
    .await
    .unwrap();
    assert_eq!(flows.len(), 1, "{flows:?}");
    assert_eq!(flows[0].latency_ns, 10_000_000);

    drop(feeder);
    rx.shutdown().await;
    tx.shutdown().await;
}

/// A channel that loses its source forgets what its ring held. Ring slots
/// are tagged with the low 32 bits of their timestamps, so audio left there
/// would play again 2^32 samples (25 h at 48 kHz) later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsubscribed_channel_forgets_its_audio() {
    let clock = system_clock();
    let tx = Device::start(config("old-tx", 24710, 32), clock.clone()).await.unwrap();
    let directory = directory_of(&tx);
    let rx = Device::start_with_directory(config("old-rx", 24720, 33), clock.clone(), directory)
        .await
        .unwrap();
    let feeder = Feeder::start(tx.audio());
    let io = rx.audio();
    rx.subscribe(1, "01", "old-tx").unwrap();
    rx.subscribe(2, "02", "old-tx").unwrap();
    eventually("audio on rx 1 and 2", || {
        verify(&io, 0, 0)?;
        verify(&io, 1, 1)
    })
    .await;
    let ts = io.now().unwrap() - io.latency_samples() - 1000;
    assert!(io.rx[0].read_one(ts).is_some());

    rx.unsubscribe(1).unwrap();
    eventually("rx 1 to forget its audio", || match io.rx[0].read_one(ts) {
        None => Ok(()),
        Some(s) => Err(format!("{s:#x} still at {ts}")),
    })
    .await;
    assert_eq!(io.rx[0].read_one(ts + (1 << 32)), None);
    // The other channel of the flow carries on.
    verify(&io, 1, 1).unwrap();

    drop(feeder);
    rx.shutdown().await;
    tx.shutdown().await;
}

//! End-to-end tests: two devices on 127.0.0.1 talking the real protocols
//! (ARC, flow control, media packets, keepalives) with each other.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ovsc_clock::system_clock;
use ovsc_core::client;
use ovsc_core::directory::StaticDirectory;
use ovsc_core::{AudioIo, Channels, Device, DeviceConfig, Ports};
use ovsc_proto::arc::{self, SubscriptionRequest, SubscriptionStatus};
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

/// Keeps the transmit rings filled ahead of time, like an audio backend.
fn start_feeder(io: AudioIo, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let lead = io.sample_rate as u64 / 10;
        let mut written = io.now().unwrap();
        let mut buf = vec![0i32; 8192];
        while !stop.load(Ordering::Relaxed) {
            let now = io.now().unwrap();
            let until = now + lead;
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
    })
}

/// Checks that `rx_ch` of `io` carries `tx_ch`'s signal for the last 100 ms.
fn verify(io: &AudioIo, rx_ch: usize, tx_ch: usize) -> Result<(), String> {
    let now = io.now().unwrap();
    let end = now - io.latency_samples;
    let start = end - io.sample_rate as u64 / 10;
    let mut buf = vec![0i32; (end - start) as usize];
    let present = io.read_rx(rx_ch, start, &mut buf);
    if present < buf.len() * 99 / 100 {
        return Err(format!("rx {rx_ch}: only {present}/{} samples arrived", buf.len()));
    }
    for (i, &s) in buf.iter().enumerate() {
        let ts = start + i as u64;
        if io.rx[rx_ch].read_one(ts).is_some() && s != signal(ts, tx_ch) {
            return Err(format!("rx {rx_ch}: sample at {ts} is {s:#x}, want tx {tx_ch}"));
        }
    }
    Ok(())
}

async fn eventually(what: &str, mut check: impl FnMut() -> Result<(), String>) {
    let mut last = String::new();
    for _ in 0..100 {
        match check() {
            Ok(()) => return,
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}: {last}");
}

/// A controller on the device's own host reaches ARC at 127.0.0.1 (Dante
/// Controller does). Linux routes all of 127.0.0.0/8 to lo, so a device on
/// 127.0.0.2 stands in for one on a network interface.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_answers_on_127_0_0_1_too() {
    let clock = system_clock();
    let mut cfg = config("other-ip", 24480, 4);
    cfg.interface = "127.0.0.2".into();
    let dev = Device::start(cfg, clock).await.unwrap();
    for ip in [Ipv4Addr::new(127, 0, 0, 2), Ipv4Addr::LOCALHOST] {
        let resp = client::transact_ok(
            Ipv4Addr::LOCALHOST,
            SocketAddr::new(ip.into(), 24480),
            &arc::encode_simple_request(client::next_seq(), arc::opcode::CHANNEL_COUNTS),
            Duration::from_millis(500),
            3,
        )
        .await
        .unwrap_or_else(|e| panic!("channel counts from {ip}: {e}"));
        let counts = arc::ChannelCounts::decode_response(&Frame::parse(&resp).unwrap()).unwrap();
        assert_eq!((counts.tx_channels, counts.rx_channels), (4, 4), "from {ip}");
    }
    dev.shutdown().await;
}

/// Dante Controller sets the sample rate and the encoding with conmon
/// 0x0081 and 0x0083 on the settings port. A device whose owner applies
/// them passes them on; any other refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_controller_sets_the_sample_rate_and_encoding() {
    use ovsc_core::{FormatRequest, StartOptions};
    use ovsc_proto::conmon::{self, ConmonHeader, notification};

    let request = |id: u16, value: u32| {
        let header = ConmonHeader {
            start_code: conmon::START_INFO,
            seq: 7,
            process_id: 0,
            device_id: [0x52, 0x54, 0, 0, 0, 0, 0, 0],
            vendor: conmon::VENDOR_ID,
            opcode: [0x07, 0x27, (id >> 8) as u8, id as u8, 0, 0, 0, 0x64],
        };
        let mut body = 1u32.to_be_bytes().to_vec();
        body.extend_from_slice(&value.to_be_bytes());
        header.encode(&body)
    };
    let settings = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 24513);
    let send = |pkt: Vec<u8>| async move {
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(&pkt, settings).await.unwrap();
    };

    let mut cfg = config("fmt-dev", 24510, 7);
    cfg.discovery = true;
    let options = StartOptions { format_configurable: true, ..Default::default() };
    let dev = Device::start_with_options(cfg.clone(), system_clock(), options).await.unwrap();
    let mut requests = dev.format_requests();
    send(request(notification::SAMPLE_RATE_QUERY, 96_000)).await;
    tokio::time::timeout(Duration::from_secs(2), requests.changed()).await.unwrap().unwrap();
    assert_eq!(
        *requests.borrow_and_update(),
        FormatRequest { sample_rate: Some(96_000), bits_per_sample: None }
    );
    send(request(notification::ENCODING_QUERY, 32)).await;
    tokio::time::timeout(Duration::from_secs(2), requests.changed()).await.unwrap().unwrap();
    assert_eq!(
        *requests.borrow_and_update(),
        FormatRequest { sample_rate: Some(96_000), bits_per_sample: Some(32) }
    );
    // Not a rate a device runs at: ignored.
    send(request(notification::SAMPLE_RATE_QUERY, 22_050)).await;
    assert!(tokio::time::timeout(Duration::from_millis(300), requests.changed()).await.is_err());
    dev.shutdown().await;

    // A device whose format nobody applies refuses.
    let dev = Device::start(cfg, system_clock()).await.unwrap();
    let mut requests = dev.format_requests();
    send(request(notification::SAMPLE_RATE_QUERY, 96_000)).await;
    assert!(tokio::time::timeout(Duration::from_millis(300), requests.changed()).await.is_err());
    dev.shutdown().await;
}

/// Dante Controller reads the latency with 0x1100 and sets it with its
/// 0x1101 packet; the new latency is saved and used from the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_controller_sets_the_latency() {
    use ovsc_proto::arc::property;

    let dir = std::env::temp_dir().join(format!("ovsc-latency-{}", std::process::id()));
    let mut cfg = config("lat-dev", 24500, 6);
    cfg.latency_ms = 4.0;
    cfg.state_file = Some(dir.join("state.toml"));
    let dev = Device::start(cfg.clone(), system_clock()).await.unwrap();
    let arc_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 24500);
    let ask = |pkt: Vec<u8>| async move {
        client::transact(Ipv4Addr::LOCALHOST, arc_addr, &pkt, Duration::from_millis(500), 3).await
    };

    let resp = ask(arc::encode_simple_request(client::next_seq(), arc::opcode::PROPERTIES_1100))
        .await
        .unwrap();
    let props = arc::decode_properties_response(&Frame::parse(&resp).unwrap()).unwrap();
    for id in [property::CONFIGURED_LATENCY, property::RX_LATENCY] {
        assert!(props.contains(&(id, Some(4_000_000))), "{props:?}");
    }
    assert!(props.contains(&(property::MIN_LATENCY, Some(1_000_000))), "{props:?}");

    let mut requests = dev.latency_requests();
    let resp =
        ask(arc::encode_set_latency_request(client::next_seq(), 2_000_000, 16)).await.unwrap();
    let frame = Frame::parse(&resp).unwrap();
    assert_eq!(frame.header.result, ovsc_proto::frame::result::SUCCESS);
    assert!(
        arc::decode_properties_response(&frame)
            .unwrap()
            .contains(&(property::CONFIGURED_LATENCY, Some(2_000_000)))
    );
    tokio::time::timeout(Duration::from_secs(2), requests.changed()).await.unwrap().unwrap();
    assert_eq!(*requests.borrow(), Some(2_000_000));
    // Still running at 4 ms until restarted; the configured value is 2 ms.
    assert_eq!(dev.info().latency_ns, 4_000_000);

    // Out of range: refused.
    let resp = ask(arc::encode_set_latency_request(client::next_seq(), 100_000, 4)).await.unwrap();
    assert_ne!(Frame::parse(&resp).unwrap().header.result, ovsc_proto::frame::result::SUCCESS);

    eventually("the state file to hold the latency", || {
        let text = std::fs::read_to_string(dir.join("state.toml")).unwrap_or_default();
        if text.contains("latency_ns = 2000000") { Ok(()) } else { Err(text) }
    })
    .await;
    dev.shutdown().await;
    let dev = Device::start(cfg, system_clock()).await.unwrap();
    assert_eq!(dev.info().latency_ns, 2_000_000);
    dev.shutdown().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// A transmitter that grants flows but refuses to change their channels
/// (DBCP 0x0102), as a Dante AVIO did on a real network: the receiver asks
/// it for another flow instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_channel_update_gets_a_flow_of_its_own() {
    use ovsc_core::directory::ResolvedChannel;
    use ovsc_proto::dbcp::{self, FlowRequest};
    use ovsc_proto::discovery::ChannelTxt;
    use ovsc_proto::frame::{self, result};

    const FC_PORT: u16 = 24495;
    let fake = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, FC_PORT)).await.unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::<(u16, Vec<u16>)>::new()));
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
                    seen.lock()
                        .unwrap()
                        .push((h.opcode, FlowRequest::decode(&req).unwrap().channels));
                    handle += 1;
                    dbcp::encode_flow_created(&h, [0, 0, 0, 0, 0, handle])
                }
                dbcp::opcode::UPDATE_FLOW => {
                    seen.lock()
                        .unwrap()
                        .push((h.opcode, dbcp::decode_update_flow(&req).unwrap().1));
                    frame::encode(h.protocol, h.seq, h.opcode, dbcp::error::INVALID_PARAMETER, &[])
                }
                _ => frame::encode(h.protocol, h.seq, h.opcode, result::SUCCESS, &[]),
            };
            let _ = fake.send_to(&resp, src).await;
        }
    });
    let directory = StaticDirectory::new();
    for id in 1..=2u16 {
        directory.insert(ResolvedChannel {
            device: "fake-tx".into(),
            channel: format!("CH{id}"),
            addr: Ipv4Addr::LOCALHOST,
            flow_control_port: FC_PORT,
            txt: ChannelTxt {
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
            },
        });
    }
    let rx =
        Device::start_with_directory(config("rx-refused", 24490, 5), system_clock(), directory)
            .await
            .unwrap();
    let requests = |log: &[(u16, Vec<u16>)], channels: &[u16]| {
        log.iter().any(|(op, c)| *op == dbcp::opcode::REQUEST_FLOW && c == channels)
    };

    rx.subscribe(1, "CH1", "fake-tx").unwrap();
    eventually("a flow for CH1", || {
        let l = log.lock().unwrap();
        // Every slot the flow may hold, the unused ones 0.
        if requests(&l, &[1, 0, 0, 0]) { Ok(()) } else { Err(format!("{l:?}")) }
    })
    .await;
    rx.subscribe(2, "CH2", "fake-tx").unwrap();
    eventually("a flow of its own for CH2", || {
        let l = log.lock().unwrap();
        if requests(&l, &[2, 0, 0, 0]) { Ok(()) } else { Err(format!("{l:?}")) }
    })
    .await;
    let l = log.lock().unwrap().clone();
    let update =
        l.iter().position(|(op, c)| *op == dbcp::opcode::UPDATE_FLOW && c == &[1, 2, 0, 0]);
    let second =
        l.iter().position(|(op, c)| *op == dbcp::opcode::REQUEST_FLOW && c == &[2, 0, 0, 0]);
    assert!(update.is_some() && update < second, "{l:?}");
    rx.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_flows_between_two_devices() {
    let clock = system_clock();
    let tx = Device::start(config("tx-dev", 24440, 1), clock.clone()).await.unwrap();
    let directory = StaticDirectory::new();
    for entry in tx.directory_entries() {
        directory.insert(entry);
    }
    let rx = Device::start_with_directory(config("rx-dev", 24450, 2), clock.clone(), directory)
        .await
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let feeder = start_feeder(tx.audio(), stop.clone());

    // Subscribe one channel at a time, as a user clicking crosspoints does.
    // Cross-wired on purpose: rx 1 <- tx 3, then rx 2 <- tx 1.
    let io = rx.audio();
    rx.subscribe(1, "03", "tx-dev").unwrap();
    eventually("audio on rx 1", || verify(&io, 0, 2)).await;
    rx.subscribe(2, "01", "tx-dev").unwrap();
    eventually("audio on rx 1 and 2", || {
        verify(&io, 0, 2)?;
        verify(&io, 1, 0)
    })
    .await;

    // Statuses are published by the subscription manager's next pass.
    eventually("receiving status", || match rx.rx_channels()[1].status {
        SubscriptionStatus::ReceivingUnicast => Ok(()),
        other => Err(format!("{other:?}")),
    })
    .await;
    let status = rx.rx_channels();
    assert_eq!(status[0].status, SubscriptionStatus::ReceivingUnicast);
    assert_eq!(status[0].subscription, Some(("03".into(), "tx-dev".into())));
    assert_eq!(status[2].status, SubscriptionStatus::None);

    // The second channel was added to the existing flow, not a new one.
    let flows = tx.tx_flows();
    assert_eq!(flows.len(), 1, "{flows:?}");
    assert_eq!(flows[0].receiver.as_deref(), Some("rx-dev"));
    // Requested with every slot the flow may hold, as Dante receivers do.
    assert_eq!(flows[0].channels, vec![3, 1, 0, 0]);

    // A freed slot is reused for the next channel from the same device.
    rx.unsubscribe(1).unwrap();
    rx.subscribe(3, "04", "tx-dev").unwrap();
    eventually("audio on rx 3", || verify(&io, 2, 3)).await;
    verify(&io, 1, 0).unwrap();
    assert_eq!(tx.tx_flows().len(), 1);
    assert_eq!(tx.tx_flows()[0].channels, vec![4, 1, 0, 0]);

    // Unsubscribing everything stops the flow on the transmitter.
    rx.unsubscribe(2).unwrap();
    rx.unsubscribe(3).unwrap();
    eventually("transmitter to drop the flow", || {
        if tx.tx_flows().is_empty() { Ok(()) } else { Err(format!("{:?}", tx.tx_flows())) }
    })
    .await;

    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    rx.shutdown().await;
    tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_routes_over_arc_and_device_subscribes_to_itself() {
    let clock = system_clock();
    let dev = Device::start(config("loop-dev", 24460, 3), clock.clone()).await.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = start_feeder(dev.audio(), stop.clone());
    let arc_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 24460));

    // A controller asks for rx 4 <- tx 2 on the same device, using "." as
    // Dante Controller does for the local device.
    let req = arc::encode_set_subscriptions_request(
        client::next_seq(),
        &[SubscriptionRequest { rx_channel: 4, source: Some(("02".into(), ".".into())) }],
    );
    client::transact_ok(Ipv4Addr::LOCALHOST, arc_addr, &req, Duration::from_millis(500), 3)
        .await
        .unwrap();

    let io = dev.audio();
    eventually("self-subscribed audio", || verify(&io, 3, 1)).await;

    // The controller sees the subscription and its status over ARC.
    eventually("receiving status", || match dev.rx_channels()[3].status {
        SubscriptionStatus::ReceivingUnicast => Ok(()),
        other => Err(format!("{other:?}")),
    })
    .await;
    let channels = client::arc_paged(
        Ipv4Addr::LOCALHOST,
        arc_addr,
        arc::opcode::RX_CHANNELS,
        arc::decode_rx_channels_response,
    )
    .await
    .unwrap();
    assert_eq!(channels.len(), 4);
    assert_eq!(channels[3].subscription, Some(("02".into(), "loop-dev".into())));
    assert_eq!(channels[3].status, SubscriptionStatus::ReceivingUnicast);

    // Renaming a receive channel over ARC.
    let req = arc::encode_rename_rx_channels_request(client::next_seq(), &[(4, "Return")]);
    client::transact_ok(Ipv4Addr::LOCALHOST, arc_addr, &req, Duration::from_millis(500), 3)
        .await
        .unwrap();
    assert_eq!(dev.rx_channels()[3].name, "Return");

    // Device name over ARC.
    let resp = client::transact_ok(
        Ipv4Addr::LOCALHOST,
        arc_addr,
        &arc::encode_simple_request(client::next_seq(), arc::opcode::DEVICE_NAME),
        Duration::from_millis(500),
        3,
    )
    .await
    .unwrap();
    assert_eq!(
        arc::decode_device_name_response(&Frame::parse(&resp).unwrap()).unwrap(),
        "loop-dev"
    );

    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    dev.shutdown().await;
}

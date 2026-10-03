//! The codec, the configuration rules, its storage text and the offsets.

use ovsc_ipc::protocol::*;
use ovsc_ipc::region::SharedRegion;

fn names(prefix: &str, n: u32) -> Vec<String> {
    (1..=n).map(|i| format!("{prefix}{i:02}")).collect()
}

fn config() -> DriverConfig {
    DriverConfig {
        config_gen: 7,
        sample_rate: 96_000,
        input_channels: 4,
        output_channels: 2,
        input_safety_offset: 432,
        output_safety_offset: 96,
        input_latency: 0,
        output_latency: 384,
        input_read_delay: 0,
        clock_algorithm: CLOCK_ALGORITHM_SIMPLE_IIR,
        clock_domain: 3,
        input_names: names("in", 4),
        output_names: names("out", 2),
        device_name: "studio-a".to_owned(),
        debug_zts_jitter_ns: 30_000,
    }
}

fn hello() -> Hello {
    Hello {
        proto_major: PROTO_MAJOR,
        proto_minor: PROTO_MINOR,
        layout_version: 1,
        layout_hash: 0xDEAD_BEEF_0123_4567,
        plugin_version: "0.1.0".to_owned(),
        instance: u64::MAX - 1,
        pid: -5,
        applied_daemon_generation: 0x1F3A,
        applied_config_gen: 9,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
        timebase_numer: 125,
        timebase_denom: 3,
        arch: 1,
    }
}

fn to_daemon_messages() -> Vec<ToDaemon> {
    vec![
        ToDaemon::Hello(hello()),
        ToDaemon::ConfigApplied(ConfigApplied {
            daemon_generation: 42,
            config_gen: 3,
            sample_rate: 44_100,
            input_channels: 128,
            output_channels: 1,
        }),
    ]
}

fn to_plugin_messages() -> Vec<ToPlugin> {
    let region = SharedRegion::create(64 * 1024).unwrap();
    vec![
        ToPlugin::Welcome(Welcome {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            daemon_version: "ovsc 0.1.0".to_owned(),
            daemon_generation: 0x1F3A_0000_0000_0001,
            region: region.handle(),
            region_size: 64 * 1024,
            config: config(),
        }),
        ToPlugin::Reject(Reject {
            reason: RejectReason::Proto,
            proto_major: 2,
            layout_version: 1,
            layout_hash: 5,
            message: "protocol 2 required".to_owned(),
        }),
        ToPlugin::Reject(Reject {
            reason: RejectReason::Layout,
            proto_major: 1,
            layout_version: 2,
            layout_hash: 6,
            message: String::new(),
        }),
        ToPlugin::Config(config()),
        ToPlugin::Config(DriverConfig::fallback()),
        ToPlugin::Bye { reason: "shutting down".to_owned() },
    ]
}

#[test]
fn every_message_round_trips() {
    for m in to_daemon_messages() {
        assert_eq!(decode_to_daemon(&encode_to_daemon(&m)).unwrap(), m);
    }
    for m in to_plugin_messages() {
        assert_eq!(decode_to_plugin(encode_to_plugin(&m)).unwrap(), m);
    }
}

#[test]
fn keys_are_the_documented_ones() {
    let kv = encode_to_daemon(&ToDaemon::Hello(hello()));
    let keys: Vec<&str> = kv.keys().map(String::as_str).collect();
    let mut want = vec![
        "op",
        "proto_major",
        "proto_minor",
        "layout_version",
        "layout_hash",
        "plugin_version",
        "instance",
        "pid",
        "applied_daemon_generation",
        "applied_config_gen",
        "sample_rate",
        "input_channels",
        "output_channels",
        "timebase_numer",
        "timebase_denom",
        "arch",
    ];
    want.sort_unstable();
    assert_eq!(keys, want);
    assert_eq!(kv["op"], Value::Str("hello".to_owned()));
    assert_eq!(kv["pid"], Value::I64(-5));
    assert_eq!(kv["plugin_version"], Value::Str("0.1.0".to_owned()));

    let kv = encode_to_plugin(&ToPlugin::Config(config()));
    assert_eq!(kv["op"], Value::Str("config".to_owned()));
    let Value::Dict(c) = &kv["config"] else { panic!("config is not a dict") };
    let mut keys: Vec<&str> = c.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut want = vec![
        "config_gen",
        "sample_rate",
        "input_channels",
        "output_channels",
        "input_safety_offset",
        "output_safety_offset",
        "input_latency",
        "output_latency",
        "input_read_delay",
        "clock_algorithm",
        "clock_domain",
        "input_names",
        "output_names",
        "device_name",
        "debug_zts_jitter_ns",
    ];
    want.sort_unstable();
    assert_eq!(keys, want);
    assert_eq!(c["input_names"], Value::StrList(names("in", 4)));

    let welcome = to_plugin_messages().swap_remove(0);
    let kv = encode_to_plugin(&welcome);
    assert!(matches!(kv["region"], Value::Region(_)));
    assert!(matches!(kv["config"], Value::Dict(_)));
    let kv = encode_to_plugin(&ToPlugin::Reject(Reject {
        reason: RejectReason::Layout,
        proto_major: 1,
        layout_version: 1,
        layout_hash: 0,
        message: String::new(),
    }));
    assert_eq!(kv["reason"], Value::Str("layout".to_owned()));
}

#[test]
fn unknown_keys_are_ignored() {
    for m in to_daemon_messages() {
        let mut kv = encode_to_daemon(&m);
        kv.insert("from_the_future".to_owned(), Value::Bool(true));
        assert_eq!(decode_to_daemon(&kv).unwrap(), m);
    }
    for m in to_plugin_messages() {
        let mut kv = encode_to_plugin(&m);
        kv.insert("from_the_future".to_owned(), Value::StrList(vec!["x".to_owned()]));
        if let Some(Value::Dict(c)) = kv.get_mut("config") {
            c.insert("also_new".to_owned(), Value::U64(1));
        }
        assert_eq!(decode_to_plugin(kv).unwrap(), m);
    }
}

#[test]
fn optional_jitter_defaults_to_zero() {
    let mut kv = encode_to_plugin(&ToPlugin::Config(config()));
    let Some(Value::Dict(c)) = kv.get_mut("config") else { panic!() };
    c.remove("debug_zts_jitter_ns");
    let ToPlugin::Config(got) = decode_to_plugin(kv).unwrap() else { panic!() };
    assert_eq!(got, DriverConfig { debug_zts_jitter_ns: 0, ..config() });
}

#[test]
fn missing_or_mistyped_keys_are_errors() {
    // Every key of every message is required (except the optional jitter).
    for m in to_daemon_messages() {
        let kv = encode_to_daemon(&m);
        for k in kv.keys() {
            let mut bad = kv.clone();
            bad.remove(k);
            assert!(decode_to_daemon(&bad).is_err(), "{k} missing");
            let mut bad = kv.clone();
            bad.insert(k.clone(), Value::Bool(false));
            assert!(decode_to_daemon(&bad).is_err(), "{k} mistyped");
        }
    }
    for m in to_plugin_messages() {
        let kv = encode_to_plugin(&m);
        for k in kv.keys() {
            let mut bad = kv.clone();
            bad.remove(k);
            assert!(decode_to_plugin(bad).is_err(), "{k} missing");
            let mut bad = kv.clone();
            bad.insert(k.clone(), Value::Bool(false));
            assert!(decode_to_plugin(bad).is_err(), "{k} mistyped");
        }
        if let Some(Value::Dict(c)) = kv.get("config") {
            for k in c.keys().filter(|k| *k != "debug_zts_jitter_ns") {
                let mut bad = kv.clone();
                let Some(Value::Dict(c)) = bad.get_mut("config") else { unreachable!() };
                c.remove(k);
                let err = decode_to_plugin(bad);
                assert!(matches!(err, Err(ProtoError::Missing(m)) if m == k), "{k}: {err:?}");
            }
        }
    }

    let mut kv = encode_to_daemon(&ToDaemon::Hello(hello()));
    kv.insert("pid".to_owned(), Value::U64(5));
    assert_eq!(decode_to_daemon(&kv), Err(ProtoError::WrongType("pid")));
    kv.insert("pid".to_owned(), Value::I64(1 << 40));
    assert_eq!(decode_to_daemon(&kv), Err(ProtoError::OutOfRange("pid")));
    let mut kv = encode_to_daemon(&ToDaemon::Hello(hello()));
    kv.insert("sample_rate".to_owned(), Value::U64(1 << 32));
    assert_eq!(decode_to_daemon(&kv), Err(ProtoError::OutOfRange("sample_rate")));
    kv.insert("sample_rate".to_owned(), Value::I64(48_000));
    assert_eq!(decode_to_daemon(&kv), Err(ProtoError::WrongType("sample_rate")));

    let mut kv = encode_to_plugin(&ToPlugin::Bye { reason: String::new() });
    kv.insert("op".to_owned(), Value::Str("hello_v9".to_owned()));
    assert_eq!(decode_to_plugin(kv.clone()), Err(ProtoError::UnknownOp("hello_v9".to_owned())));
    kv.remove("op");
    assert_eq!(decode_to_plugin(kv), Err(ProtoError::MissingOp));

    let mut kv = encode_to_plugin(&to_plugin_messages()[1]);
    kv.insert("reason".to_owned(), Value::Str("mood".to_owned()));
    assert_eq!(decode_to_plugin(kv), Err(ProtoError::BadValue("reason")));
}

#[test]
fn fallback_is_valid_and_matches_the_48k_row() {
    let f = DriverConfig::fallback();
    f.validate().unwrap();
    assert_eq!((f.sample_rate, f.input_channels, f.output_channels), (48_000, 8, 8));
    assert_eq!((f.input_safety_offset, f.output_safety_offset, f.output_latency), (216, 55, 192));
    assert_eq!((f.input_latency, f.input_read_delay), (0, 0));
    assert_eq!(f.clock_algorithm, CLOCK_ALGORITHM_RAW);
    assert_eq!(f.input_names, names("", 8));
    assert_eq!(f.output_names, names("", 8));
    assert_eq!(f.device_name, "OpenVirtualSoundcard");
    config().validate().unwrap();
}

#[test]
fn each_validation_rule_rejects() {
    let ok = config();
    let cases: Vec<(DriverConfig, &str)> = vec![
        (DriverConfig { sample_rate: 32_000, ..ok.clone() }, "rate"),
        (DriverConfig { sample_rate: 0, ..ok.clone() }, "rate 0"),
        (DriverConfig { input_channels: 0, input_names: vec![], ..ok.clone() }, "0 in"),
        (DriverConfig { output_channels: 0, output_names: vec![], ..ok.clone() }, "0 out"),
        (DriverConfig { input_channels: 129, input_names: names("", 129), ..ok.clone() }, "129"),
        (DriverConfig { output_channels: 129, output_names: names("", 129), ..ok.clone() }, "129"),
        (DriverConfig { input_safety_offset: 16_385, ..ok.clone() }, "in safety"),
        (DriverConfig { output_safety_offset: 16_385, ..ok.clone() }, "out safety"),
        (DriverConfig { input_latency: 16_385, ..ok.clone() }, "in latency"),
        (DriverConfig { output_latency: 16_385, ..ok.clone() }, "out latency"),
        (DriverConfig { input_read_delay: 16_385, ..ok.clone() }, "read delay"),
        (DriverConfig { input_names: names("", 3), ..ok.clone() }, "too few names"),
        (DriverConfig { output_names: names("", 3), ..ok.clone() }, "too many names"),
        (DriverConfig { clock_algorithm: u32::from_be_bytes(*b"none"), ..ok.clone() }, "algo"),
        (DriverConfig { device_name: String::new(), ..ok.clone() }, "empty device name"),
    ];
    for (cfg, what) in cases {
        assert!(cfg.validate().is_err(), "{what} accepted");
    }
    for bad in ["", &"x".repeat(32), "a\0b", "line\nbreak", "cr\r"] {
        for out in [false, true] {
            let mut cfg = ok.clone();
            let list = if out { &mut cfg.output_names } else { &mut cfg.input_names };
            list[1] = bad.to_owned();
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Name { index: 1, .. })),
                "{bad:?} accepted"
            );
        }
    }
    // The bounds themselves are fine.
    let mut edge = DriverConfig {
        input_channels: 128,
        input_names: names("", 128),
        output_channels: 1,
        output_names: vec!["x".repeat(31)],
        input_safety_offset: 16_384,
        output_safety_offset: 16_384,
        input_latency: 16_384,
        output_latency: 16_384,
        input_read_delay: 16_384,
        clock_algorithm: CLOCK_ALGORITHM_RAW,
        ..ok.clone()
    };
    edge.validate().unwrap();
    for rate in SAMPLE_RATES {
        edge.sample_rate = rate;
        edge.validate().unwrap();
    }
    edge.input_names[5] = "Ünïcödé name 31 bytes ok".to_owned();
    edge.validate().unwrap();
}

#[test]
fn structural_eq_ignores_names_and_generation() {
    let a = config();
    let b = DriverConfig {
        config_gen: 99,
        input_names: names("renamed", 4),
        output_names: names("x", 2),
        device_name: "other".to_owned(),
        ..a.clone()
    };
    assert!(a.structural_eq(&b));
    let changes: Vec<DriverConfig> = vec![
        DriverConfig { sample_rate: 48_000, ..a.clone() },
        DriverConfig { input_channels: 3, ..a.clone() },
        DriverConfig { output_channels: 3, ..a.clone() },
        DriverConfig { input_safety_offset: 1, ..a.clone() },
        DriverConfig { output_safety_offset: 1, ..a.clone() },
        DriverConfig { input_latency: 1, ..a.clone() },
        DriverConfig { output_latency: 1, ..a.clone() },
        DriverConfig { input_read_delay: 1, ..a.clone() },
        DriverConfig { clock_algorithm: CLOCK_ALGORITHM_RAW, ..a.clone() },
        DriverConfig { clock_domain: 0, ..a.clone() },
        DriverConfig { debug_zts_jitter_ns: 0, ..a.clone() },
    ];
    for c in changes {
        assert!(!a.structural_eq(&c), "{c:?}");
    }
}

#[test]
fn storage_text_round_trips() {
    let mut c = config();
    c.input_names = vec![
        "with space".to_owned(),
        "100% sure".to_owned(),
        "Grüße, Ünïcode".to_owned(),
        "a=b,c%2C".to_owned(),
    ];
    c.output_names = vec![" lead and trail ".to_owned(), "🎚 fader".to_owned()];
    c.device_name = "Desk 1 (ü)".to_owned();
    let text = c.to_storage_string();
    assert!(text.is_ascii(), "{text}");
    assert!(text.contains("with space"), "spaces stay readable: {text}");
    assert_eq!(DriverConfig::from_storage_string(&text).unwrap(), c);

    let f = DriverConfig::fallback();
    assert_eq!(DriverConfig::from_storage_string(&f.to_storage_string()).unwrap(), f);

    // CRLF line ends, blank lines and unknown keys are tolerated.
    let tolerant = format!("future_key=1\n\n{}", text.replace('\n', "\r\n"));
    assert_eq!(DriverConfig::from_storage_string(&tolerant).unwrap(), c);
    // The jitter is optional.
    let without: String = text
        .lines()
        .filter(|l| !l.starts_with("debug_zts_jitter_ns="))
        .map(|l| l.to_owned() + "\n")
        .collect();
    assert_eq!(
        DriverConfig::from_storage_string(&without).unwrap(),
        DriverConfig { debug_zts_jitter_ns: 0, ..c.clone() }
    );
}

#[test]
fn bad_storage_text_is_rejected() {
    let text = config().to_storage_string();
    let replace = |key: &str, value: &str| -> String {
        text.lines()
            .map(|l| {
                if l.starts_with(&format!("{key}=")) {
                    format!("{key}={value}")
                } else {
                    l.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let missing: String =
        text.lines().filter(|l| !l.starts_with("sample_rate=")).collect::<Vec<_>>().join("\n");
    for (bad, what) in [
        (missing, "missing key"),
        (format!("{text}sample_rate=48000\n"), "duplicate key"),
        (format!("{text}garbage\n"), "line without ="),
        (replace("sample_rate", "fast"), "not a number"),
        (replace("sample_rate", "4294967296"), "out of range"),
        (replace("sample_rate", "32000"), "invalid config"),
        (replace("device_name", "bad%2"), "short escape"),
        (replace("device_name", "bad%zz"), "bad escape"),
        (replace("device_name", "%FF%FE"), "not UTF-8"),
        (replace("input_names", "a,b,c"), "wrong name count"),
        (String::new(), "empty"),
    ] {
        assert!(DriverConfig::from_storage_string(&bad).is_err(), "{what} accepted");
    }
}

fn offsets(fs: u32, latency_ms: u32, mode: InputLatencyMode) -> Offsets {
    compute_offsets(&OffsetInputs {
        sample_rate: fs,
        // round(latency_ms * fs / 1000), as ovsc-core computes it.
        latency_samples: ((latency_ms as u64 * fs as u64 * 2 + 1000) / 2000) as u32,
        tx_guard_samples: (500 * fs as u64 / 1_000_000) as u32,
        fpp_max: 32,
        input_margin_us: 500,
        output_margin_us: 1000,
        output_latency_override: None,
        latency_mode: mode,
    })
}

#[test]
fn offsets_match_the_design_table() {
    // fs, L, input safety, output safety, output latency (design section 8.4,
    // and the table in docs/MACOS.md, which must hold every row).
    let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/MACOS.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", doc_path.display()));
    let table = [
        (44_100, 176, 199, 54, 176),
        (48_000, 192, 216, 55, 192),
        (88_200, 353, 398, 89, 353),
        (96_000, 384, 432, 96, 384),
        (176_400, 706, 795, 177, 706),
        (192_000, 768, 864, 192, 768),
    ];
    for (fs, l, in_safety, out_safety, out_latency) in table {
        let o = offsets(fs, 4, InputLatencyMode::Safety);
        assert_eq!(
            o,
            Offsets {
                input_safety: in_safety,
                output_safety: out_safety,
                input_latency: 0,
                output_latency: out_latency,
                input_read_delay: 0,
            },
            "{fs} Hz"
        );
        // Latency mode moves L from the safety offset to latency and delay.
        let o = offsets(fs, 4, InputLatencyMode::Latency);
        assert_eq!(o.input_safety, in_safety - l, "{fs} Hz");
        assert_eq!((o.input_latency, o.input_read_delay), (l, l), "{fs} Hz");
        assert_eq!((o.output_safety, o.output_latency), (out_safety, out_latency), "{fs} Hz");
        let row = format!("| {fs} | {l} | {in_safety} | {out_safety} | {out_latency} |");
        assert!(doc.lines().any(|line| line == row), "docs/MACOS.md lacks the row {row}");
    }
}

#[test]
fn offsets_edge_cases() {
    let base = OffsetInputs {
        sample_rate: 48_000,
        latency_samples: 192,
        tx_guard_samples: 24,
        fpp_max: 32,
        input_margin_us: 500,
        output_margin_us: 1000,
        output_latency_override: Some(77),
        latency_mode: InputLatencyMode::Safety,
    };
    assert_eq!(compute_offsets(&base).output_latency, 77);
    // A guard longer than a packet leaves only the margin.
    let o = compute_offsets(&OffsetInputs { tx_guard_samples: 1000, ..base });
    assert_eq!(o.output_safety, 48);
    let o = compute_offsets(&OffsetInputs { fpp_max: 0, ..base });
    assert_eq!(o.output_safety, 48);
    // Margins round up.
    let o = compute_offsets(&OffsetInputs { input_margin_us: 1, output_margin_us: 1, ..base });
    assert_eq!((o.input_safety, o.output_safety), (193, 8));
    // 40 ms at 192 kHz: valid offsets would exceed the cap.
    let o = offsets(192_000, 40, InputLatencyMode::Safety);
    assert_eq!(o.input_safety, 7680 + 96);
    assert!(o.input_safety <= MAX_OFFSET_FRAMES);
}

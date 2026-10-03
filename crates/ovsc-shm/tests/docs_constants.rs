//! docs/MACOS.md restates the shared region's layout and the daemon's Mach
//! service name for readers. This test keeps it honest: every constant must
//! appear in the document exactly as the code defines it.
//!
//! The document writes byte offsets and sizes in hex the way the source
//! does (`0x0C00`, `0x0001_0000`: upper-case digits, at least four, grouped
//! by four with `_`) and counts in decimal (`128`). A constant counts as
//! documented when one line of the document holds both its name and its
//! value, each in backticks, as the tables there do.

use std::mem::size_of;
use std::path::Path;

use ovsc_shm::clock::ClockBlock;
use ovsc_shm::layout::*;
use ovsc_shm::status::{DaemonStatus, IoTraceEntry, IoTraceHeader, PluginStatus};

/// The daemon's Mach service. It is defined as `SERVICE_NAME` in
/// ovsc-ipc, which depends on this crate and so cannot be a dependency
/// of its tests; `service_name_is_documented` checks that definition in the
/// source instead.
const SERVICE_NAME: &str = "org.openvirtualsoundcard.audio";

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn doc() -> String {
    read("../../docs/MACOS.md")
}

/// `v` as the source writes offsets: `0x` and upper-case hex digits,
/// zero-padded to a multiple of four, in groups of four separated by `_`.
fn hex(v: usize) -> String {
    let digits = format!("{v:X}");
    let width = digits.len().div_ceil(4).max(1) * 4;
    let padded = format!("{digits:0>width$}");
    let groups: Vec<&str> = padded
        .as_bytes()
        .chunks(4)
        .map(|g| std::str::from_utf8(g).expect("hex digits are ASCII"))
        .collect();
    format!("0x{}", groups.join("_"))
}

/// Fails unless one line of `doc` contains every one of `needles`.
fn assert_line(doc: &str, needles: &[&str]) {
    assert!(
        doc.lines().any(|line| needles.iter().all(|n| line.contains(n))),
        "docs/MACOS.md has no line containing all of {needles:?}"
    );
}

/// Fails unless one line of `doc` names constant `name` and gives `value`,
/// both in backticks, plus any `extra` text.
fn assert_constant(doc: &str, name: &str, value: &str, extra: &[&str]) {
    let name = format!("`{name}`");
    let value = format!("`{value}`");
    let mut needles = vec![name.as_str(), value.as_str()];
    needles.extend_from_slice(extra);
    assert_line(doc, &needles);
}

#[test]
fn hex_matches_the_source_style() {
    assert_eq!(hex(0), "0x0000");
    assert_eq!(hex(0x0C00), "0x0C00");
    assert_eq!(hex(0x1080), "0x1080");
    assert_eq!(hex(0x4_0000), "0x0004_0000");
    assert_eq!(hex(0x0401_0000), "0x0401_0000");
    assert_eq!(hex(0x1_0000_0000), "0x0001_0000_0000");
}

#[test]
fn region_layout_is_documented() {
    let doc = doc();
    let blocks = [
        ("HEADER_OFFSET", HEADER_OFFSET, size_of::<Header>()),
        ("CLOCK_OFFSET", CLOCK_OFFSET, size_of::<ClockBlock>()),
        ("DAEMON_STATUS_OFFSET", DAEMON_STATUS_OFFSET, size_of::<DaemonStatus>()),
        ("PLUGIN_STATUS_OFFSET", PLUGIN_STATUS_OFFSET, size_of::<PluginStatus>()),
        ("IO_TRACE_OFFSET", IO_TRACE_OFFSET, size_of::<IoTraceHeader>()),
    ];
    for (name, offset, size) in blocks {
        assert_constant(&doc, name, &hex(offset), &[&format!("| {size} B |")]);
    }
    let entries = format!("| {IO_TRACE_ENTRIES} × {} B |", size_of::<IoTraceEntry>());
    assert_constant(&doc, "IO_TRACE_ENTRIES_OFFSET", &hex(IO_TRACE_ENTRIES_OFFSET), &[&entries]);
    let rings = format!("| {MAX_CHANNELS} × {} KiB |", RING_BYTES / 1024);
    assert_constant(&doc, "RX_OFFSET", &hex(RX_OFFSET), &[&rings]);
    assert_constant(&doc, "TX_OFFSET", &hex(TX_OFFSET), &[&rings]);
    assert_constant(&doc, "REGION_SIZE", &hex(REGION_SIZE), &["| end |"]);
}

#[test]
fn layout_constants_are_documented() {
    let doc = doc();
    assert_constant(&doc, "MAX_CHANNELS", &MAX_CHANNELS.to_string(), &[]);
    assert_constant(&doc, "RING_FRAMES", &RING_FRAMES.to_string(), &[]);
    assert_constant(&doc, "RING_BYTES", &hex(RING_BYTES), &[]);
    assert_constant(&doc, "IO_TRACE_ENTRIES", &IO_TRACE_ENTRIES.to_string(), &[]);
    assert_constant(&doc, "IO_FRAMES_CAP", &IO_FRAMES_CAP.to_string(), &[]);
    assert_constant(&doc, "LAYOUT_VERSION", &LAYOUT_VERSION.to_string(), &[]);
    // The region's size in bytes, as the daemon logs it.
    let bytes = REGION_SIZE.to_string();
    let grouped = bytes
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|c| std::str::from_utf8(c).expect("decimal digits are ASCII"))
        .collect::<Vec<_>>()
        .join(",");
    assert_constant(&doc, "REGION_SIZE", &hex(REGION_SIZE), &[&format!("{grouped} bytes")]);
    assert!(doc.contains(&format!("{bytes} bytes)")), "the daemon's log line with {bytes} bytes");
}

#[test]
fn service_name_is_documented() {
    let protocol = read("../ovsc-ipc/src/protocol.rs");
    let definition = format!("pub const SERVICE_NAME: &str = \"{SERVICE_NAME}\";");
    assert!(
        protocol.lines().any(|l| l.trim() == definition),
        "ovsc-ipc's SERVICE_NAME is no longer {SERVICE_NAME:?}: update this test and \
         docs/MACOS.md"
    );
    assert!(doc().contains(&format!("`{SERVICE_NAME}`")), "docs/MACOS.md lacks `{SERVICE_NAME}`");
}

//! Diagnostic: runs the development master (on a free-running clock) and
//! a follower on high ports of 127.0.0.1, then prints the follower's offset
//! from the master every second and reports any jump between consecutive
//! readings taken 10 ms apart.
//!
//! ```sh
//! cargo run -p ovsc-clock --example ptp_watch -- 60
//! ```

use std::net::Ipv4Addr;
use std::time::Duration;

use ovsc_clock::free_running_clock;
use ovsc_clock::ptp::master::{TestMaster, TestMasterConfig};
use ovsc_clock::ptp::{PtpConfig, PtpFollower};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(45);
    let ip = Ipv4Addr::LOCALHOST;
    let (event, general) = (32319, 32320);

    let mut mcfg = TestMasterConfig::new(ip, [0x02, 0, 0, 0, 0, 0x01]);
    mcfg.event_port = event;
    mcfg.general_port = general;
    let reference = free_running_clock();
    let master = TestMaster::start(mcfg, reference.clone()).await?;

    let mut fcfg = PtpConfig::new(ip, [0x02, 0, 0, 0, 0, 0x02]);
    fcfg.event_port = event;
    fcfg.general_port = general;
    let (follower, clock) = PtpFollower::start(fcfg).await?;

    let mut last: Option<i64> = None;
    let mut worst_jump = 0i64;
    for tick in 0..seconds * 100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (Some(ours), Some(truth)) = (clock.now_ns(), reference.now_ns()) else { continue };
        let offset = ours as i64 - truth as i64;
        if let Some(prev) = last {
            let jump = offset - prev;
            if jump.abs() > 200_000 {
                println!(
                    "{:7.2} s  JUMP {:+} µs (state {:?})",
                    tick as f64 / 100.0,
                    jump / 1000,
                    clock.status().state
                );
            }
            worst_jump = worst_jump.max(jump.abs());
        }
        last = Some(offset);
        if tick % 100 == 0 {
            let st = clock.status();
            println!(
                "{:7.2} s  offset {:+8} µs  state {:?}  freq {:+.2} ppm",
                tick as f64 / 100.0,
                offset / 1000,
                st.state,
                st.freq_offset_ppb / 1000.0
            );
        }
    }
    println!("worst 10 ms jump: {} µs", worst_jump / 1000);
    follower.shutdown();
    master.shutdown();
    Ok(())
}

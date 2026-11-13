//! Persistence of user-made changes (names, subscriptions) across restarts.
//!
//! The state file holds what a controller or the app changed: losing it
//! loses every subscription made in Dante Controller. So it is written to
//! survive a power cut at any moment:
//!
//! * each save goes to a temporary file of its own, which is flushed to the
//!   disk (`sync_all`, which is `F_FULLFSYNC` on macOS) before it replaces
//!   the state file, and the directory is flushed after the rename;
//! * the previous, readable generation stays next to it as `<file>.bak`;
//! * a file that cannot be read (empty or damaged) is never overwritten:
//!   the backup is used in its place, the problem is logged and reported
//!   (see [`SavedState::recover`]), and the save that replaces it first
//!   keeps it as `<file>.corrupt-<unix time>`.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tracing::{debug, error, warn};

use crate::info::DeviceInfo;
use crate::net::Interface;
use crate::state::{Shared, State};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedState {
    pub name: Option<String>,
    pub tx_names: Vec<String>,
    pub rx_names: Vec<String>,
    pub subscriptions: Vec<SavedSubscription>,
    /// Receive latency set by a controller or the app, ns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ns: Option<u32>,
    /// The show lock as the app set it; replaces the configuration's
    /// `lock_remote_settings`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_remote_settings: Option<bool>,
    /// The device name the device holds on the network, and on which
    /// computer. State files saved before claims were have none: the
    /// device then probes for its name, as it always did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<SavedClaim>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSubscription {
    /// 1-based receive channel.
    pub rx_channel: u16,
    pub tx_channel: String,
    pub tx_device: String,
}

/// A device name the device claimed on the network (it probed for it, RFC
/// 6762 §8.1, or took it over from the device it replaced) and announces,
/// with the MAC address of the interface it announces it on.
///
/// A new process on the same computer (the daemon started again after a
/// crash, or at a reboot) that starts the device with that name, on that
/// interface, announces the name again without probing for it, as a device
/// restarted within its process does ([`crate::StartOptions::announced`]):
/// any other device announcing the name too would answer a probe, and the
/// device would take another name, which breaks every subscription to it.
/// That other device is reported instead. On another computer, a copy of
/// the state file (a cloned disk) holds a MAC address the interface there
/// does not have, so the device there probes, as at a first start. See
/// [`SavedState::claimed_name`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// A claim missing a field (edited by hand) is read as no claim, rather
// than making the whole state file unreadable.
#[serde(default)]
pub struct SavedClaim {
    pub name: String,
    /// The interface's MAC address, as `00:1d:c1:52:79:b6`.
    pub mac: String,
}

/// `mac` as `00:1d:c1:52:79:b6`, the way `ifconfig` shows it.
fn mac_text(mac: [u8; 6]) -> String {
    mac.map(|b| format!("{b:02x}")).join(":")
}

/// The state a device starts from, as [`SavedState::recover`] found it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    pub state: SavedState,
    /// The state file itself holds `state`: nothing needs saving until it
    /// changes.
    pub intact: bool,
    /// Why the state file could not be used, for the logs and the app.
    pub problem: Option<String>,
}

/// Why a state file could not be read.
enum Unreadable {
    /// Empty or not a state file: a save cut short, or a damaged disk.
    Damaged(String),
    /// Reading failed (permissions, I/O error); the file may be fine.
    Io(io::Error),
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreadable::Damaged(why) => f.write_str(why),
            Unreadable::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Reads the state file at `path`: `None` if there is none.
fn read(path: &Path) -> Result<Option<SavedState>, Unreadable> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Unreadable::Io(e)),
    };
    let text =
        String::from_utf8(bytes).map_err(|_| Unreadable::Damaged("it is not text".to_owned()))?;
    // Every save writes at least the (possibly empty) lists, so an empty
    // file is a save the disk never finished, not "nothing saved".
    if text.trim().is_empty() {
        return Err(Unreadable::Damaged("it is empty".to_owned()));
    }
    toml::from_str(&text).map(Some).map_err(|e| Unreadable::Damaged(e.to_string()))
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Where the previous generation of the state file at `path` is kept.
pub fn backup_path(path: &Path) -> PathBuf {
    sibling(path, ".bak")
}

/// Serialises saves within the process: the device's saving task, its
/// last save on shutdown and the app's changes while no device runs. Each
/// writes a snapshot taken under the lock, so the last save is the newest.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

fn save_lock() -> MutexGuard<'static, ()> {
    SAVE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The problems [`SavedState::recover`] found in this process, by state
/// file. A device started again later (on a new network address) reads the
/// file its predecessor saved, intact by then; the warning must outlive
/// that restart, which can follow within seconds of the start (a power cut
/// also brings the network up late), or nobody sees it.
static PROBLEMS: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

/// What [`SavedState::recover`] last found wrong with the state file at
/// `path` in this process, if anything.
pub fn problem_at_start(path: &Path) -> Option<String> {
    let problems = PROBLEMS.lock().unwrap_or_else(PoisonError::into_inner);
    problems.iter().find(|(p, _)| p == path).map(|(_, problem)| problem.clone())
}

fn remember_problem(path: &Path, problem: &str) {
    let mut problems = PROBLEMS.lock().unwrap_or_else(PoisonError::into_inner);
    problems.retain(|(p, _)| p != path);
    problems.push((path.to_owned(), problem.to_owned()));
}

impl SavedState {
    /// The state saved at `path`, or in its backup if `path` cannot be
    /// read; `None` if neither holds one. Moves nothing: for showing the
    /// saved settings while no device runs.
    pub fn load(path: &Path) -> Option<Self> {
        match read(path) {
            Ok(saved) => saved,
            Err(e) => {
                warn!("cannot read the state file {}: {e}", path.display());
                read(&backup_path(path)).ok().flatten()
            }
        }
    }

    /// The state a device runs from: the one saved at `path`, if it can be
    /// read. If it cannot, the previous generation in the backup is used
    /// instead, and the problem is logged as an error and returned. No file
    /// at all is a first start.
    ///
    /// Moves nothing: a device start can still fail after this (the
    /// network not up yet) and is then retried, and the retry must find
    /// the damaged file and the backup as they were. The save that
    /// replaces a damaged file keeps it aside first (it may still be
    /// repaired by hand).
    pub fn recover(path: &Path) -> Recovered {
        let why = match read(path) {
            Ok(Some(state)) => return Recovered { state, intact: true, problem: None },
            Ok(None) => return Recovered::default(),
            Err(e) => e,
        };
        let mut problem = format!("the saved state {} could not be read ({why})", path.display());
        if let Unreadable::Damaged(_) = why {
            problem.push_str(&format!(
                "; it is kept as {}-<unix time>, never overwritten",
                sibling(path, ".corrupt").display()
            ));
        }
        let backup = backup_path(path);
        let state = match read(&backup) {
            Ok(Some(state)) => {
                problem.push_str(&format!(
                    "; restored the previous copy {}: check the names and subscriptions",
                    backup.display()
                ));
                state
            }
            _ => {
                problem.push_str(
                    "; no usable backup: starting without the saved names, subscriptions and \
                     latency",
                );
                SavedState::default()
            }
        };
        error!("{problem}");
        remember_problem(path, &problem);
        Recovered { state, intact: false, problem: Some(problem) }
    }

    /// The state of a device as it is saved: `state`, the device's shared
    /// state, and its claim of the name it announces on `info`'s interface.
    pub fn from_state(state: &State, info: &DeviceInfo) -> Self {
        Self {
            name: (state.name != info.factory_name).then(|| state.name.clone()),
            tx_names: state.tx_names.clone(),
            rx_names: state.rx_names.clone(),
            subscriptions: state
                .subscriptions
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    s.as_ref().map(|s| SavedSubscription {
                        rx_channel: i as u16 + 1,
                        tx_channel: s.tx_channel.clone(),
                        tx_device: s.tx_device.clone(),
                    })
                })
                .collect(),
            latency_ns: state.latency_override_ns,
            lock_remote_settings: state.remote_settings_lock_set,
            claim: state
                .announced
                .as_ref()
                .map(|name| SavedClaim { name: name.clone(), mac: mac_text(info.iface.mac) }),
        }
    }

    /// The device name [`SavedState::claim`] says the device holds on the
    /// network, if it claimed it on `iface`: this computer's interface, so
    /// that a device starting from this state can announce the name again
    /// without probing for it. `None` on another computer (a copy of the
    /// state file on a cloned disk) or on another interface, whose MAC
    /// address differs. An interface without a MAC address (all zeros)
    /// cannot tell computers apart, unless it is loopback, which no other
    /// computer hears.
    pub fn claimed_name(&self, iface: &Interface) -> Option<&str> {
        let claim = self.claim.as_ref()?;
        let identifies_computer = iface.mac != [0; 6] || iface.ip.is_loopback();
        let here = claim.mac.eq_ignore_ascii_case(&mac_text(iface.mac));
        (identifies_computer && here).then_some(claim.name.as_str())
    }

    /// The receive and transmit channel names of a device whose own ones
    /// are `rx` and `tx` that starts from this state: each saved name that
    /// is not empty, else the device's own.
    pub fn channel_names(&self, rx: &[String], tx: &[String]) -> (Vec<String>, Vec<String>) {
        let pick = |saved: &[String], own: &[String]| -> Vec<String> {
            own.iter()
                .enumerate()
                .map(|(i, o)| saved.get(i).filter(|s| !s.is_empty()).unwrap_or(o).clone())
                .collect()
        };
        (pick(&self.rx_names, rx), pick(&self.tx_names, tx))
    }

    /// Changes the state saved at `path` with `f`, for a device that is
    /// not running (a running one saves its own: change it instead).
    pub fn update(path: &Path, f: impl FnOnce(&mut SavedState)) -> io::Result<()> {
        let _lock = save_lock();
        let mut saved = Self::recover(path).state;
        f(&mut saved);
        saved.save_locked(path)
    }

    /// Writes this state to `path` durably (see the module documentation).
    /// The caller holds [`SAVE_LOCK`].
    fn save_locked(&self, path: &Path) -> io::Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        std::fs::create_dir_all(dir)?;
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        // A name of its own, so that no other writer (another process
        // included) can ever interleave with this one.
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = sibling(path, &format!(".tmp-{}-{n}", std::process::id()));
        let written = (|| {
            let mut f = File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            // On macOS std issues F_FULLFSYNC here, which also flushes the
            // drive's cache: plain fsync does not survive a power cut there.
            f.sync_all()?;
            drop(f);
            keep_previous(path)?;
            std::fs::rename(&tmp, path)?;
            sync_dir(dir);
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    #[cfg(test)]
    fn save(&self, path: &Path) -> io::Result<()> {
        let _lock = save_lock();
        self.save_locked(path)
    }
}

/// Keeps what the state file at `path` holds before a save replaces it. A
/// readable file becomes the backup; a damaged one never does: it is kept
/// as `<file>.corrupt-<unix time>` instead, and if that fails the save
/// fails rather than overwrite it.
fn keep_previous(path: &Path) -> io::Result<()> {
    match read(path) {
        Ok(Some(_)) => {
            // Best effort: a save without a backup is still a save.
            let backup = backup_path(path);
            if let Err(e) = keep_copy(path, &backup) {
                debug!("cannot keep {} as {}: {e}", path.display(), backup.display());
            }
            Ok(())
        }
        Err(Unreadable::Damaged(_)) => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let aside = sibling(path, &format!(".corrupt-{secs}"));
            keep_copy(path, &aside).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot keep the damaged {} aside: {e}", path.display()),
                )
            })?;
            warn!("kept the damaged state file as {}", aside.display());
            Ok(())
        }
        // Nothing there, or nothing known about it.
        Ok(None) | Err(Unreadable::Io(_)) => Ok(()),
    }
}

/// Makes `to` hold what `path` holds now, without touching `path`.
fn keep_copy(path: &Path, to: &Path) -> io::Result<()> {
    let tmp = sibling(path, &format!(".keep-tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    // A hard link shares the file already on disk: nothing to flush, and
    // `path` stays in place until the save's rename replaces it. Where
    // links are not supported, a flushed copy does the same.
    let made = std::fs::hard_link(path, &tmp).or_else(|_| {
        std::fs::copy(path, &tmp)?;
        File::open(&tmp)?.sync_all()
    });
    let kept = made.and_then(|()| std::fs::rename(&tmp, to));
    if kept.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    kept
}

/// Flushes directory `dir`, so that a rename in it survives a power cut.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
        debug!("cannot flush {}: {e}", dir.display());
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

/// Saves the state whenever names or subscriptions change. `on_disk` is
/// what the state file already holds, if it was read intact: an unchanged
/// state is not written again.
pub async fn run(shared: Arc<Shared>, path: PathBuf, on_disk: Option<SavedState>) {
    let mut watch = shared.watch();
    let mut last = on_disk;
    loop {
        let saved = SavedState::from_state(&shared.state(), &shared.info);
        if last.as_ref() != Some(&saved) {
            let (s, p) = (shared.clone(), path.clone());
            // A durable save waits for the disk, up to tens of
            // milliseconds with F_FULLFSYNC: not on the runtime's threads.
            if let Ok(written) = tokio::task::spawn_blocking(move || save(&s, &p)).await {
                last = written;
            }
        }
        if watch.changed().await.is_err() {
            return;
        }
    }
}

/// Saves the device's current state at `path`; what it saved, if it could.
pub fn save(shared: &Shared, path: &Path) -> Option<SavedState> {
    let _lock = save_lock();
    // Taken under the lock: whichever save comes last writes the newest.
    let saved = SavedState::from_state(&shared.state(), &shared.info);
    // Unchanged: a durable save would only wait for the disk.
    if matches!(read(path), Ok(Some(ref on_disk)) if *on_disk == saved) {
        return Some(saved);
    }
    match saved.save_locked(path) {
        Ok(()) => {
            debug!("saved state to {}", path.display());
            Some(saved)
        }
        Err(e) => {
            warn!("cannot save state to {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ovsc-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn named(name: &str) -> SavedState {
        SavedState {
            name: Some(name.into()),
            subscriptions: vec![SavedSubscription {
                rx_channel: 1,
                tx_channel: "Vox".into(),
                tx_device: "stagebox".into(),
            }],
            ..Default::default()
        }
    }

    /// The files in `dir` whose names start with `prefix`.
    fn files(dir: &Path, prefix: &str) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(prefix))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn round_trip_through_file() {
        let dir = dir("round-trip");
        let path = dir.join("state.toml");
        let saved = SavedState {
            name: Some("studio".into()),
            tx_names: vec!["L".into(), "R".into()],
            rx_names: vec!["In".into()],
            subscriptions: vec![SavedSubscription {
                rx_channel: 1,
                tx_channel: "01".into(),
                tx_device: "desk".into(),
            }],
            latency_ns: Some(2_000_000),
            lock_remote_settings: Some(true),
            claim: Some(SavedClaim { name: "studio".into(), mac: "00:1d:c1:52:79:b6".into() }),
        };
        saved.save(&path).unwrap();
        assert_eq!(SavedState::load(&path), Some(saved.clone()));
        assert_eq!(
            SavedState::recover(&path),
            Recovered { state: saved, intact: true, problem: None }
        );
        // Nothing but the file: no temporary files left behind.
        assert_eq!(files(&dir, ""), ["state.toml"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saved_channel_names_replace_the_devices_own() {
        let saved = SavedState {
            rx_names: vec!["Vox".into(), String::new()],
            tx_names: vec!["L".into(), "R".into(), "gone".into()],
            ..Default::default()
        };
        let own = |n: u16| (1..=n).map(|i| format!("{i:02}")).collect::<Vec<_>>();
        let (rx, tx) = saved.channel_names(&own(3), &own(2));
        assert_eq!(
            (rx, tx),
            (vec!["Vox".into(), "02".into(), "03".into()], vec!["L".into(), "R".into()])
        );
        assert_eq!(SavedState::default().channel_names(&own(2), &own(1)), (own(2), own(1)));
    }

    fn interface(ip: [u8; 4], mac: [u8; 6]) -> Interface {
        Interface {
            name: "en7".into(),
            ip: ip.into(),
            netmask: std::net::Ipv4Addr::new(255, 255, 0, 0),
            gateway: std::net::Ipv4Addr::UNSPECIFIED,
            mac,
            link_speed_mbps: 1000,
        }
    }

    fn claim(name: &str, mac: &str) -> SavedState {
        SavedState {
            claim: Some(SavedClaim { name: name.into(), mac: mac.into() }),
            ..Default::default()
        }
    }

    /// A claim holds on the computer, and the interface, it was made on: a
    /// new process there announces the name without probing for it.
    #[test]
    fn a_claim_holds_where_it_was_made() {
        let mac = [0x00, 0x1d, 0xc1, 0x52, 0x79, 0xb6];
        let saved = claim("vox", "00:1d:c1:52:79:b6");
        assert_eq!(saved.claimed_name(&interface([169, 254, 3, 4], mac)), Some("vox"));
        // A new address on the same interface, and capitals written by hand.
        assert_eq!(saved.claimed_name(&interface([10, 0, 0, 9], mac)), Some("vox"));
        let upper = claim("vox", "00:1D:C1:52:79:B6");
        assert_eq!(upper.claimed_name(&interface([10, 0, 0, 9], mac)), Some("vox"));
        // Another computer (a cloned disk), or another interface: probed for.
        let other = [0x00, 0x1d, 0xc1, 0x08, 0x12, 0x58];
        assert_eq!(saved.claimed_name(&interface([169, 254, 3, 4], other)), None);
        // No claim saved, as before claims were.
        assert_eq!(SavedState::default().claimed_name(&interface([169, 254, 3, 4], mac)), None);
        // Without a MAC address only loopback, which no other computer
        // hears, tells this computer from another.
        let none = claim("vox", "00:00:00:00:00:00");
        assert_eq!(none.claimed_name(&interface([127, 0, 0, 1], [0; 6])), Some("vox"));
        assert_eq!(none.claimed_name(&interface([169, 254, 3, 4], [0; 6])), None);
    }

    /// State files from before claims were saved, or with a claim edited by
    /// hand, are read as they are: no claim, nothing damaged.
    #[test]
    fn a_missing_or_partial_claim_is_no_claim() {
        let dir = dir("claim-compat");
        let path = dir.join("state.toml");
        let loopback = interface([127, 0, 0, 1], [0; 6]);
        std::fs::write(&path, "name = \"vox\"\ntx_names = []\nsubscriptions = []\n").unwrap();
        let r = SavedState::recover(&path);
        assert_eq!(r.problem, None);
        assert_eq!(r.state, SavedState { name: Some("vox".into()), ..Default::default() });
        std::fs::write(&path, "name = \"vox\"\n\n[claim]\nname = \"vox\"\n").unwrap();
        let r = SavedState::recover(&path);
        assert_eq!(r.problem, None);
        assert_eq!(r.state.claimed_name(&loopback), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A state file shaped like the ones the build before the name claim
    /// saved (a name, channel names, a latency and two subscriptions, with
    /// made-up names; no `[claim]` table) still loads with both
    /// subscriptions intact: the address changes of the engine and the
    /// version bump must not cost a device its routing.
    #[test]
    fn a_saved_state_with_two_subscriptions_loads_unchanged() {
        let dir = dir("two-subscriptions");
        let path = dir.join("state.toml");
        std::fs::write(
            &path,
            "name = \"stage-mac\"\ntx_names = [\"Left\", \"Right\"]\nrx_names = []\n\
             latency_ns = 5000000\n\n\
             [[subscriptions]]\nrx_channel = 1\ntx_channel = \"01\"\ntx_device = \"mixer-a1b2c3\"\n\n\
             [[subscriptions]]\nrx_channel = 2\ntx_channel = \"02\"\ntx_device = \"mixer-a1b2c3\"\n",
        )
        .unwrap();
        let r = SavedState::recover(&path);
        assert_eq!(r.problem, None);
        assert!(r.intact);
        assert_eq!(r.state.name.as_deref(), Some("stage-mac"));
        assert_eq!(
            r.state.subscriptions,
            [
                SavedSubscription {
                    rx_channel: 1,
                    tx_channel: "01".into(),
                    tx_device: "mixer-a1b2c3".into()
                },
                SavedSubscription {
                    rx_channel: 2,
                    tx_channel: "02".into(),
                    tx_device: "mixer-a1b2c3".into()
                },
            ]
        );
        assert_eq!(r.state.latency_ns, Some(5_000_000));
        assert_eq!(r.state.claim, None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn no_file_is_a_first_start() {
        let dir = dir("first");
        let path = dir.join("state.toml");
        assert_eq!(SavedState::load(&path), None);
        assert_eq!(SavedState::recover(&path), Recovered::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn each_save_keeps_the_previous_generation() {
        let dir = dir("backup");
        let path = dir.join("state.toml");
        named("one").save(&path).unwrap();
        assert!(!backup_path(&path).exists(), "nothing to keep at the first save");
        named("two").save(&path).unwrap();
        named("three").save(&path).unwrap();
        assert_eq!(SavedState::load(&path), Some(named("three")));
        assert_eq!(read(&backup_path(&path)).ok().flatten(), Some(named("two")));
        assert_eq!(files(&dir, ""), ["state.toml", "state.toml.bak"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_file_is_kept_aside_and_the_backup_used() {
        let dir = dir("empty");
        let path = dir.join("state.toml");
        named("one").save(&path).unwrap();
        named("two").save(&path).unwrap();
        // A power cut after the rename reached the disk but not the data.
        std::fs::write(&path, "").unwrap();
        // Shown from the backup, without touching anything.
        assert_eq!(SavedState::load(&path), Some(named("one")));
        assert!(path.exists());

        let r = SavedState::recover(&path);
        assert_eq!(r.state, named("one"));
        assert!(!r.intact);
        let problem = r.problem.clone().unwrap();
        assert!(problem.contains("it is empty"), "{problem}");
        assert!(problem.contains("restored the previous copy"), "{problem}");
        assert!(problem.contains("state.toml.corrupt-<unix time>"), "{problem}");
        // Nothing moved: a device start that fails after this and is
        // retried (the network not up yet) finds the same, and restores
        // the backup again instead of starting empty.
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(files(&dir, "state.toml.corrupt-").is_empty());
        assert_eq!(SavedState::recover(&path), r);

        // The save that replaces it keeps it aside first; the good backup
        // stays.
        named("three").save(&path).unwrap();
        assert_eq!(SavedState::load(&path), Some(named("three")));
        assert_eq!(read(&backup_path(&path)).ok().flatten(), Some(named("one")));
        let aside = files(&dir, "state.toml.corrupt-");
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read(dir.join(&aside[0])).unwrap(), b"");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_without_a_backup_is_kept_and_reported() {
        let dir = dir("garbage");
        let path = dir.join("state.toml");
        let garbage = b"name = \"half a wri\x00\xff";
        std::fs::write(&path, garbage).unwrap();
        let r = SavedState::recover(&path);
        assert_eq!(r.state, SavedState::default());
        assert!(!r.intact);
        assert!(r.problem.unwrap().contains("no usable backup"));
        assert_eq!(std::fs::read(&path).unwrap(), garbage);

        // Saving again keeps it aside, and does not make it the backup.
        named("fresh").save(&path).unwrap();
        let aside = files(&dir, "state.toml.corrupt-");
        assert_eq!(std::fs::read(dir.join(&aside[0])).unwrap(), garbage);
        assert!(!backup_path(&path).exists());
        named("fresher").save(&path).unwrap();
        assert_eq!(read(&backup_path(&path)).ok().flatten(), Some(named("fresh")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_that_cannot_be_kept_is_not_overwritten() {
        let dir = dir("unkeepable");
        let path = dir.join("state.toml");
        std::fs::write(&path, "[[[").unwrap();
        // Directories in the way of every name it could be kept as for the
        // next minute: keeping it fails.
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        for secs in now..now + 60 {
            let blocker = sibling(&path, &format!(".corrupt-{secs}"));
            std::fs::create_dir_all(blocker.join("x")).unwrap();
        }
        let e = named("new").save(&path).unwrap_err();
        assert!(e.to_string().contains("cannot keep the damaged"), "{e}");
        // Not overwritten, and nothing left behind.
        assert_eq!(std::fs::read(&path).unwrap(), b"[[[");
        assert_eq!(files(&dir, "state.toml").len(), 61, "{:?}", files(&dir, "state.toml"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_never_replaces_a_good_backup() {
        let dir = dir("keep-backup");
        let path = dir.join("state.toml");
        named("good").save(&path).unwrap();
        named("newer").save(&path).unwrap();
        std::fs::write(&path, "subscriptions = [[[").unwrap();
        // The app changes the name while no device runs.
        SavedState::update(&path, |s| s.name = Some("renamed".into())).unwrap();
        let now = SavedState::load(&path).unwrap();
        assert_eq!(now.name.as_deref(), Some("renamed"));
        // Built on the backup, the last good generation.
        assert_eq!(now.subscriptions, named("good").subscriptions);
        assert_eq!(read(&backup_path(&path)).ok().flatten(), Some(named("good")));
        assert_eq!(files(&dir, "state.toml.corrupt-").len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

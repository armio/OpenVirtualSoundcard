//! Persistence of user-made changes (names, subscriptions) across restarts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSubscription {
    /// 1-based receive channel.
    pub rx_channel: u16,
    pub tx_channel: String,
    pub tx_device: String,
}

impl SavedState {
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        match toml::from_str(&text) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("ignoring unreadable state file {}: {e}", path.display());
                None
            }
        }
    }

    pub fn from_state(state: &State, factory_name: &str) -> Self {
        Self {
            name: (state.name != factory_name).then(|| state.name.clone()),
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
        }
    }

    /// Changes the state saved at `path` with `f`, for a device that is
    /// not running (a running one saves its own).
    pub fn update(path: &Path, f: impl FnOnce(&mut SavedState)) -> std::io::Result<()> {
        let mut saved = Self::load(path).unwrap_or_default();
        f(&mut saved);
        saved.save(path)
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// Saves the state whenever names or subscriptions change.
pub async fn run(shared: Arc<Shared>, path: PathBuf) {
    let mut watch = shared.watch();
    let mut last = None;
    loop {
        let saved = SavedState::from_state(&shared.state(), &shared.info.factory_name);
        if last.as_ref() != Some(&saved) {
            write(&saved, &path);
            last = Some(saved);
        }
        if watch.changed().await.is_err() {
            return;
        }
    }
}

/// Saves the device's current state at `path`.
pub fn save(shared: &Shared, path: &Path) {
    write(&SavedState::from_state(&shared.state(), &shared.info.factory_name), path);
}

fn write(saved: &SavedState, path: &Path) {
    match saved.save(path) {
        Ok(()) => debug!("saved state to {}", path.display()),
        Err(e) => warn!("cannot save state to {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_file() {
        let dir = std::env::temp_dir().join(format!("ovsc-persist-{}", std::process::id()));
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
        };
        saved.save(&path).unwrap();
        assert_eq!(SavedState::load(&path), Some(saved));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

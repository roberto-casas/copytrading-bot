//! Lightweight restart-safe persistence for runtime state.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::state::BotStateSnapshot;

const BOT_STATE_FILE: &str = "bot_state.json";
const SEEN_TRADES_FILE: &str = "seen_trades.json";
const ALERT_STATE_FILE: &str = "alert_state.json";

#[derive(Debug, Clone)]
pub struct PersistenceStore {
    dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlertPersistedState {
    pub last_signature: String,
    pub last_sent_at: Option<DateTime<Utc>>,
    pub last_breaker_signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SeenTradesFile {
    pub by_whale: HashMap<String, Vec<String>>,
}

impl PersistenceStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Failed creating state dir {}", self.dir.display()))
    }

    pub fn load_bot_state(&self) -> Result<Option<BotStateSnapshot>> {
        self.read_json_if_exists(&self.path(BOT_STATE_FILE))
    }

    pub fn save_bot_state(&self, snapshot: &BotStateSnapshot) -> Result<()> {
        self.write_json_atomic(&self.path(BOT_STATE_FILE), snapshot)
    }

    pub fn load_seen_trades(&self) -> Result<HashMap<String, HashSet<String>>> {
        let file: Option<SeenTradesFile> =
            self.read_json_if_exists(&self.path(SEEN_TRADES_FILE))?;
        Ok(file
            .unwrap_or_default()
            .by_whale
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect::<HashSet<_>>()))
            .collect())
    }

    pub fn save_seen_trades(&self, seen: &HashMap<String, HashSet<String>>) -> Result<()> {
        let by_whale = seen
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect::<Vec<_>>()))
            .collect();
        self.write_json_atomic(&self.path(SEEN_TRADES_FILE), &SeenTradesFile { by_whale })
    }

    pub fn load_alert_state(&self) -> Result<Option<AlertPersistedState>> {
        self.read_json_if_exists(&self.path(ALERT_STATE_FILE))
    }

    pub fn save_alert_state(&self, state: &AlertPersistedState) -> Result<()> {
        self.write_json_atomic(&self.path(ALERT_STATE_FILE), state)
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn read_json_if_exists<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed reading {}", path.display()))?;
        match serde_json::from_str::<T>(&raw) {
            Ok(val) => Ok(Some(val)),
            Err(e) => {
                // Gracefully recover from corruption by preserving the bad file
                // for forensics and continuing with a clean state.
                let ts = Utc::now().format("%Y%m%dT%H%M%S");
                let backup = path.with_extension(format!("corrupt.{ts}.json"));
                let _ = fs::copy(path, &backup);
                tracing::warn!(
                    file = %path.display(),
                    backup = %backup.display(),
                    error = %e,
                    "State file was corrupted; moved aside and continuing with defaults"
                );
                Ok(None)
            }
        }
    }

    fn write_json_atomic<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let tmp = path.with_extension("tmp");
        let payload = serde_json::to_string_pretty(value)
            .with_context(|| format!("Failed serializing {}", path.display()))?;
        fs::write(&tmp, payload)
            .with_context(|| format!("Failed writing temp file {}", tmp.display()))?;
        if path.exists() {
            let backup = path.with_extension("bak");
            let _ = fs::copy(path, backup);
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("Failed atomically replacing {}", path.display()))?;
        Ok(())
    }
}

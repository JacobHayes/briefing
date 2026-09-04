//! On-disk copies of briefing records, so a briefing survives the process that created it.
//!
//! One JSON file per briefing under `$XDG_STATE_HOME/briefing/briefings` (override with
//! `BRIEFING_STATE_DIR`). Files are written on create, on every draft save, and on
//! completion; any `briefing` process can adopt one (`briefing await <id>`), and the sweep
//! deletes them on the same TTLs as the in-memory registry. Nothing here is long-term state.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Briefing;
use crate::hub::BriefingStatus;
use crate::response::BriefingResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredRecord {
    pub id: String,
    pub token: String,
    pub presentation: Briefing,
    pub status: BriefingStatus,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Public URL at creation time (may be dead once the serving process exits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub draft_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<BriefingResponse>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `BRIEFING_STATE_DIR`, else `$XDG_STATE_HOME/briefing/briefings`, else
    /// `~/.local/state/briefing/briefings`.
    pub fn default_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("BRIEFING_STATE_DIR") {
            return Some(PathBuf::from(dir));
        }
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
        Some(base.join("briefing/briefings"))
    }

    /// Open (creating) the default store; `None` with a warning when no directory is usable.
    pub fn open_default() -> Option<Store> {
        let dir = Self::default_dir()?;
        match Self::open(&dir) {
            Ok(store) => Some(store),
            Err(error) => {
                tracing::warn!(%error, dir = %dir.display(), "briefing state directory unusable; records stay in memory");
                None
            }
        }
    }

    pub fn open(dir: &Path) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Store { dir: dir.to_path_buf() })
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        // Ids are URL-safe base64; refuse anything else so a caller can't escape the dir.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        Some(self.dir.join(format!("{id}.json")))
    }

    pub fn save(&self, record: &StoredRecord) -> std::io::Result<()> {
        self.write(&record.id, &serde_json::to_vec(record).map_err(std::io::Error::other)?)
    }

    /// Atomic write (temp file + rename), owner-only permissions.
    pub fn write(&self, id: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.path(id).ok_or_else(|| std::io::Error::other("invalid briefing id"))?;
        let tmp = self.dir.join(format!(".{id}.{}.tmp", std::process::id()));
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            use std::io::Write;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &path)
    }

    pub fn load(&self, id: &str) -> Option<StoredRecord> {
        let path = self.path(id)?;
        let bytes = std::fs::read(path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(record) => Some(record),
            Err(error) => {
                tracing::warn!(%error, id, "ignoring unreadable briefing record");
                None
            }
        }
    }

    pub fn remove(&self, id: &str) {
        if let Some(path) = self.path(id) {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn list(&self) -> Vec<StoredRecord> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut records: Vec<StoredRecord> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let id = name.strip_suffix(".json")?;
                if id.starts_with('.') {
                    return None;
                }
                self.load(id)
            })
            .collect();
        records.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        records
    }

    pub fn find_by_token(&self, token: &str) -> Option<StoredRecord> {
        self.list().into_iter().find(|record| record.token == token)
    }

    /// Delete records past their TTL (finished ones after `finished_ttl`, unanswered ones after
    /// `active_ttl`) plus stray temp files.
    pub fn sweep(&self, finished_ttl: Duration, active_ttl: Duration) {
        let now = now_secs();
        for record in self.list() {
            let expired = match record.finished_at {
                Some(finished) => now.saturating_sub(finished) >= finished_ttl.as_secs(),
                None => now.saturating_sub(record.created_at) >= active_ttl.as_secs(),
            };
            if expired {
                self.remove(&record.id);
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let is_tmp = name.to_str().is_some_and(|n| n.starts_with('.') && n.ends_with(".tmp"));
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .is_some_and(|age| age > Duration::from_secs(60));
                if is_tmp && stale {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;

    fn record(id: &str, finished_at: Option<u64>) -> StoredRecord {
        StoredRecord {
            id: id.into(),
            token: format!("tok-{id}"),
            presentation: demo(),
            status: if finished_at.is_some() { BriefingStatus::Completed } else { BriefingStatus::Active },
            created_at: now_secs(),
            finished_at,
            source: Some("test".into()),
            url: None,
            draft_revision: 0,
            draft: None,
            result: None,
        }
    }

    #[test]
    fn save_load_list_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.save(&record("abc", None)).unwrap();
        store.save(&record("old", Some(now_secs() - 10_000))).unwrap();
        assert_eq!(store.load("abc").unwrap().token, "tok-abc");
        assert!(store.load("../etc/passwd").is_none());
        assert_eq!(store.list().len(), 2);
        assert_eq!(store.find_by_token("tok-old").unwrap().id, "old");
        store.sweep(Duration::from_secs(3600), Duration::from_secs(86_400));
        assert!(store.load("old").is_none());
        assert!(store.load("abc").is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("abc.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

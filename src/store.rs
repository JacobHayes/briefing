//! On-disk copies of a hub's briefing records, so briefings survive a hub restart.
//!
//! One JSON file per briefing under `$XDG_STATE_HOME/briefing/briefings` (override with
//! `BRIEFING_STATE_DIR`). Files are written on create, on every draft save, and on
//! completion; the hub that owns the directory reloads them at startup and deletes them when
//! its sweep expires them. Nothing here is long-term state.
//!
//! Each file records its `schemaVersion`. Opening the store upgrades every older file in place
//! (see [`crate::migrate`]), and loading one migrates it too, for files an older process wrote
//! after startup.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Briefing;
use crate::hub::{BriefingStatus, Origin};
use crate::migrate::{self, Migration};
use crate::response::BriefingResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredRecord {
    /// Shape of this file; always [`migrate::SCHEMA_VERSION`] once loaded.
    pub schema_version: u64,
    pub id: String,
    pub presentation: Briefing,
    pub status: BriefingStatus,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(flatten)]
    pub origin: Origin,
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
    /// An absolute `$<xdg_env>`, else `$HOME/<home_suffix>`. Shared by the XDG-based default
    /// locations (state dir here, config file in `main`).
    pub fn xdg_base(xdg_env: &str, home_suffix: &str) -> Option<PathBuf> {
        std::env::var_os(xdg_env)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(home_suffix)))
    }

    /// `BRIEFING_STATE_DIR`, else `$XDG_STATE_HOME/briefing/briefings`, else
    /// `~/.local/state/briefing/briefings`.
    pub fn default_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("BRIEFING_STATE_DIR") {
            return Some(PathBuf::from(dir));
        }
        Some(Self::xdg_base("XDG_STATE_HOME", ".local/state")?.join("briefing/briefings"))
    }

    pub fn open(dir: &Path) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let store = Store { dir: dir.to_path_buf() };
        // Loading upgrades and rewrites any record in an older shape.
        store.list();
        Ok(store)
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        // Ids are URL-safe base64; refuse anything else so a caller can't escape the dir.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        Some(self.dir.join(format!("{id}.json")))
    }

    pub fn save(&self, record: &StoredRecord) -> std::io::Result<()> {
        let path = self.path(&record.id).ok_or_else(|| std::io::Error::other("invalid briefing id"))?;
        write_atomic(&path, &serde_json::to_vec(record).map_err(std::io::Error::other)?)
    }

    pub fn load(&self, id: &str) -> Option<StoredRecord> {
        let path = self.path(id)?;
        let bytes = std::fs::read(path).ok()?;
        let unreadable = |error: &dyn std::fmt::Display| {
            tracing::warn!(%error, id, "ignoring unreadable briefing record");
        };
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|error| unreadable(&error)).ok()?;
        let migration = migrate::migrate(&mut value).map_err(|error| unreadable(&error)).ok()?;
        let record: StoredRecord = serde_json::from_value(value).map_err(|error| unreadable(&error)).ok()?;
        if let Migration::Upgraded { from } = migration {
            match self.save(&record) {
                Ok(()) => tracing::info!(id, from, to = migrate::SCHEMA_VERSION, "migrated briefing record"),
                Err(error) => tracing::warn!(%error, id, "could not rewrite migrated briefing record"),
            }
        }
        Some(record)
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

    /// Delete temp files left by interrupted writes. Expired records are the hub's call: it
    /// owns the directory and removes them itself.
    pub fn remove_stale_temp_files(&self) {
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

/// Atomic write (hidden temp file beside `path` + rename), owner-only permissions, so a reader
/// never sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| std::io::Error::other("no file name"))?;
    let tmp = path.with_file_name(format!(".{}.{}.tmp", name.trim_start_matches('.'), std::process::id()));
    let written = (|| {
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
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;

    fn record(id: &str, finished_at: Option<u64>) -> StoredRecord {
        StoredRecord {
            schema_version: migrate::SCHEMA_VERSION,
            id: id.into(),
            presentation: demo(),
            status: if finished_at.is_some() { BriefingStatus::Completed } else { BriefingStatus::Active },
            created_at: now_secs(),
            finished_at,
            origin: Origin::source("test"),
            draft_revision: 0,
            draft: None,
            result: None,
        }
    }

    #[test]
    fn save_load_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.save(&record("abc", None)).unwrap();
        store.save(&record("old", Some(now_secs() - 10_000))).unwrap();
        assert!(store.load("../etc/passwd").is_none());
        assert_eq!(store.list().len(), 2);
        store.remove("old");
        store.remove_stale_temp_files();
        assert!(store.load("old").is_none());
        assert!(store.load("abc").is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("abc.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn opening_the_store_upgrades_older_records_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        // A pre-versioning record: no schemaVersion, a v1 decision and a v1 result.
        let legacy = serde_json::json!({
            "id": "legacy", "token": "tok-legacy", "status": "completed", "createdAt": now_secs(), "finishedAt": now_secs(),
            "presentation": { "title": "T", "goal": "G", "chunks": [{ "title": "C", "mainPoint": "M" }],
                              "decisions": [{ "question": "Q?", "options": [{ "label": "A" }, { "label": "B" }] }] },
            "result": { "chunks": [], "decisions": [{ "question": "Q?", "selected": "A", "note": "" }], "annotations": [], "overallNote": "done" }
        });
        std::fs::write(dir.path().join("legacy.json"), legacy.to_string()).unwrap();

        let store = Store::open(dir.path()).unwrap();
        let on_disk: Value = serde_json::from_slice(&std::fs::read(dir.path().join("legacy.json")).unwrap()).unwrap();
        assert_eq!(on_disk["schemaVersion"], migrate::SCHEMA_VERSION);
        let record = store.load("legacy").unwrap();
        assert_eq!(record.presentation.questions[0].question, "Q?");
        let result = record.result.unwrap();
        assert_eq!(result.questions[0].selected, vec!["A"]);
        assert_eq!(result.notes, vec!["done"]);
    }
}

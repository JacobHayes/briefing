//! Registry of presentations awaiting briefing.
//!
//! Every presentation has two unguessable identifiers: the `id` used by the agent side
//! (CLI / MCP / hub API) and the `token` embedded in the browser URL. Records live in
//! memory and, when a [`Store`] is configured, are mirrored to disk so another process can
//! adopt them (`briefing await <id>` after the creator died) and so results survive until
//! the agent fetches them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::content::Briefing;
use crate::response::{BriefingResponse, parse_browser_result};
use crate::store::{Store, StoredRecord, now_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefingStatus {
    Active,
    Completed,
    Cancelled,
}

impl BriefingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BriefingStatus::Active => "active",
            BriefingStatus::Completed => "completed",
            BriefingStatus::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for BriefingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BriefingResponse {
    /// The status a finished briefing carries for this result.
    pub fn status(&self) -> BriefingStatus {
        if self.cancelled { BriefingStatus::Cancelled } else { BriefingStatus::Completed }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Pending,
    Done(BriefingResponse),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HubError {
    #[error("briefing not found")]
    NotFound,
    #[error("briefing already {0}")]
    AlreadyFinished(BriefingStatus),
}

/// Outcome of a draft save.
#[derive(Debug, Clone, PartialEq)]
pub enum DraftSave {
    Saved {
        revision: u64,
    },
    /// The caller's base revision is behind; here is the current draft.
    Stale {
        revision: u64,
        draft: Value,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedBriefing {
    pub id: String,
    pub token: String,
}

/// Where the user is in a briefing, derived from the saved draft.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftSummary {
    /// 1-based screen the user is on (chunks, then decisions, then the review screen).
    pub screen: u64,
    pub screens: u64,
    pub annotations: u64,
    pub section_notes: u64,
    pub decisions: u64,
    /// Unix milliseconds, as reported by the browser.
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefingInfo {
    pub id: String,
    pub title: String,
    pub status: BriefingStatus,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Where the briefing is (or was last) served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftSummary>,
    /// True the first time this process serves a briefing at a different link than the
    /// one it was last served at: the old link is dead and the new one must be shown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reopened: bool,
    /// True when the record is only on disk (listed, but not served by this process).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_disk_only: bool,
}

fn info(stored: &StoredRecord, on_disk_only: bool) -> BriefingInfo {
    BriefingInfo {
        id: stored.id.clone(),
        title: stored.presentation.title.clone(),
        status: stored.status,
        created_at: stored.created_at,
        finished_at: stored.finished_at,
        source: stored.source.clone(),
        url: stored.url.clone(),
        draft: stored.draft.as_ref().map(|draft| draft_summary(&stored.presentation, draft)),
        reopened: false,
        on_disk_only,
    }
}

pub fn draft_summary(presentation: &Briefing, draft: &Value) -> DraftSummary {
    let state = &draft["state"];
    let count_map = |value: &Value, keep: &dyn Fn(&Value) -> bool| {
        value.as_object().map(|m| m.values().filter(|v| keep(v)).count()).unwrap_or(0) as u64
    };
    let non_empty = |v: &Value, key: &str| v[key].as_str().is_some_and(|s| !s.trim().is_empty());
    DraftSummary {
        screen: draft["current"].as_u64().unwrap_or(0) + 1,
        screens: (presentation.chunks.len() + presentation.decisions.len()) as u64 + 1,
        annotations: state["annotations"].as_array().map(|a| a.len()).unwrap_or(0) as u64,
        section_notes: count_map(&state["chunks"], &|c| {
            non_empty(c, "note") || non_empty(c, "checkpoint") || c["status"].as_str() == Some("revisit")
        }),
        decisions: count_map(&state["decisions"], &|d| non_empty(d, "selected") || non_empty(d, "note")),
        updated_at: draft["updatedAt"].as_u64().unwrap_or(0),
    }
}

/// An in-memory record: the stored form plus a channel that wakes waiters on finish.
struct Record {
    stored: StoredRecord,
    status: watch::Sender<BriefingStatus>,
}

impl Record {
    fn new(stored: StoredRecord) -> Record {
        let (status, _) = watch::channel(stored.status);
        Record { stored, status }
    }

    fn is_active(&self) -> bool {
        self.stored.status == BriefingStatus::Active
    }
}

pub struct HubConfig {
    /// How long a finished briefing stays fetchable through `wait`/`status`.
    pub finished_ttl: Duration,
    /// How long an unanswered briefing stays open.
    pub active_ttl: Duration,
    /// On-disk mirror; `None` keeps everything in memory.
    pub store: Option<Store>,
}

impl HubConfig {
    pub const FINISHED_TTL: Duration = Duration::from_secs(6 * 60 * 60);
    pub const ACTIVE_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);
    /// The same defaults as CLI flag text (a test in main.rs keeps them in step).
    pub const FINISHED_TTL_TEXT: &str = "6h";
    pub const ACTIVE_TTL_TEXT: &str = "14d";

    /// Default TTLs plus the default on-disk store.
    pub fn with_default_store() -> Self {
        Self { store: Store::open_default(), ..Self::default() }
    }
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { finished_ttl: Self::FINISHED_TTL, active_ttl: Self::ACTIVE_TTL, store: None }
    }
}

pub struct Hub {
    config: HubConfig,
    records: Mutex<HashMap<String, Record>>,
    /// Unix seconds of the last sweep; sweeps are rate-limited because each one reads the
    /// whole store directory.
    last_sweep: AtomicU64,
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// How often a waiter re-reads the on-disk record, in case another process finished it.
const RECONCILE_EVERY: Duration = Duration::from_secs(2);
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// A record serialized under the lock, to be written to disk once it is released.
struct Pending(Option<(String, Vec<u8>)>);

impl Hub {
    pub fn new(config: HubConfig) -> Self {
        let hub = Self { config, records: Mutex::new(HashMap::new()), last_sweep: AtomicU64::new(0) };
        hub.sweep();
        hub
    }

    /// Serialize `record` for the store (call while holding the lock; write with `flush`).
    fn encode(&self, record: &Record) -> Pending {
        if self.config.store.is_none() {
            return Pending(None);
        }
        match serde_json::to_vec(&record.stored) {
            Ok(bytes) => Pending(Some((record.stored.id.clone(), bytes))),
            Err(error) => {
                tracing::warn!(%error, id = record.stored.id, "could not serialize briefing record");
                Pending(None)
            }
        }
    }

    fn flush(&self, pending: Pending) {
        if let (Some(store), Some((id, bytes))) = (&self.config.store, pending.0)
            && let Err(error) = store.write(&id, &bytes)
        {
            tracing::warn!(%error, id, "could not write briefing record");
        }
    }

    pub fn create(&self, presentation: Briefing, source: Option<String>) -> CreatedBriefing {
        self.sweep_if_due();
        let stored = StoredRecord {
            id: random_token(12),
            token: random_token(24),
            presentation,
            status: BriefingStatus::Active,
            created_at: now_secs(),
            finished_at: None,
            source,
            url: None,
            draft_revision: 0,
            draft: None,
            result: None,
        };
        let created = CreatedBriefing { id: stored.id.clone(), token: stored.token.clone() };
        let record = Record::new(stored);
        let pending = self.encode(&record);
        self.records.lock().unwrap().insert(created.id.clone(), record);
        self.flush(pending);
        created
    }

    /// Remember the public URL a briefing is served at (shown by `status` and the dashboard,
    /// also after the serving process is gone). Returns true when it changed.
    pub fn set_url(&self, id: &str, url: &str) -> bool {
        let pending = {
            let mut records = self.records.lock().unwrap();
            let Some(record) = records.get_mut(id) else {
                return false;
            };
            if record.stored.url.as_deref() == Some(url) {
                return false;
            }
            record.stored.url = Some(url.to_string());
            self.encode(record)
        };
        self.flush(pending);
        true
    }

    fn adopt(&self, stored: StoredRecord) {
        tracing::info!(id = stored.id, status = %stored.status, "adopted briefing record from disk");
        self.records.lock().unwrap().entry(stored.id.clone()).or_insert_with(|| Record::new(stored));
    }

    /// Load `id` from disk if this process does not know it.
    fn ensure_loaded(&self, id: &str) {
        if self.records.lock().unwrap().contains_key(id) {
            return;
        }
        if let Some(stored) = self.config.store.as_ref().and_then(|store| store.load(id)) {
            self.adopt(stored);
        }
    }

    fn id_for_token(&self, token: &str) -> Option<String> {
        let known =
            self.records.lock().unwrap().values().find(|r| r.stored.token == token).map(|r| r.stored.id.clone());
        if known.is_some() {
            return known;
        }
        let stored = self.config.store.as_ref()?.find_by_token(token)?;
        let id = stored.id.clone();
        self.adopt(stored);
        Some(id)
    }

    /// If another process finished this briefing on disk, apply that here.
    fn reconcile(&self, id: &str) {
        let Some(store) = &self.config.store else {
            return;
        };
        if !self.records.lock().unwrap().get(id).is_some_and(Record::is_active) {
            return;
        }
        if let Some(stored) = store.load(id)
            && stored.status != BriefingStatus::Active
        {
            let result = stored.result.unwrap_or_else(BriefingResponse::cancelled);
            let _ = self.finish(id, result, stored.status);
        }
    }

    /// Load (and reconcile) `id`, then read it.
    fn with_record<R>(&self, id: &str, read: impl FnOnce(&Record) -> R) -> Option<R> {
        self.ensure_loaded(id);
        self.reconcile(id);
        self.records.lock().unwrap().get(id).map(read)
    }

    /// Whether the browser token names a briefing this process can serve.
    pub fn has_token(&self, token: &str) -> bool {
        self.id_for_token(token).is_some()
    }

    /// The JSON the browser page fetches: the presentation plus id, status, and draft.
    pub fn page_payload(&self, token: &str) -> Option<Value> {
        let id = self.id_for_token(token)?;
        let records = self.records.lock().unwrap();
        let stored = &records.get(&id)?.stored;
        let mut payload = serde_json::to_value(&stored.presentation).ok()?;
        let object = payload.as_object_mut()?;
        object.insert("id".into(), Value::String(stored.id.clone()));
        object.insert("status".into(), Value::String(stored.status.to_string()));
        object.insert("draftRevision".into(), Value::from(stored.draft_revision));
        object.insert("draft".into(), stored.draft.clone().unwrap_or(Value::Null));
        Some(payload)
    }

    /// Save the browser's draft. `base` is the revision the browser last saw; a mismatch
    /// returns the newer draft instead of overwriting it.
    pub fn save_draft(&self, token: &str, base: Option<u64>, draft: Value) -> Result<DraftSave, HubError> {
        let id = self.id_for_token(token).ok_or(HubError::NotFound)?;
        let (outcome, pending) = {
            let mut records = self.records.lock().unwrap();
            let record = records.get_mut(&id).ok_or(HubError::NotFound)?;
            let stored = &mut record.stored;
            if stored.status != BriefingStatus::Active {
                return Err(HubError::AlreadyFinished(stored.status));
            }
            if let Some(base) = base
                && base != stored.draft_revision
                && let Some(existing) = &stored.draft
            {
                return Ok(DraftSave::Stale { revision: stored.draft_revision, draft: existing.clone() });
            }
            stored.draft_revision += 1;
            stored.draft = Some(draft);
            (DraftSave::Saved { revision: stored.draft_revision }, self.encode(record))
        };
        self.flush(pending);
        Ok(outcome)
    }

    fn finish(&self, id: &str, result: BriefingResponse, status: BriefingStatus) -> Result<(), HubError> {
        let pending = {
            let mut records = self.records.lock().unwrap();
            let record = records.get_mut(id).ok_or(HubError::NotFound)?;
            if !record.is_active() {
                return Err(HubError::AlreadyFinished(record.stored.status));
            }
            record.stored.result = Some(result);
            record.stored.finished_at = Some(now_secs());
            record.stored.status = status;
            record.status.send_replace(status);
            self.encode(record)
        };
        self.flush(pending);
        Ok(())
    }

    /// Browser submission (`complete` or `cancel`) for the presentation behind `token`.
    pub fn submit_by_token(&self, token: &str, body: &Value, cancelled: bool) -> Result<(), HubError> {
        let id = self.id_for_token(token).ok_or(HubError::NotFound)?;
        let result = parse_browser_result(body, cancelled);
        let status = result.status();
        self.finish(&id, result, status)
    }

    /// Agent-side cancellation. Returns false when the briefing was not active.
    pub fn cancel(&self, id: &str) -> bool {
        self.ensure_loaded(id);
        self.finish(id, BriefingResponse::cancelled(), BriefingStatus::Cancelled).is_ok()
    }

    pub fn status(&self, id: &str) -> Option<BriefingStatus> {
        self.with_record(id, |r| r.stored.status)
    }

    pub fn info(&self, id: &str) -> Option<BriefingInfo> {
        self.with_record(id, |r| info(&r.stored, false))
    }

    /// Everything this process knows plus on-disk records from other processes.
    pub fn list(&self) -> Vec<BriefingInfo> {
        let mut infos: Vec<BriefingInfo> =
            self.records.lock().unwrap().values().map(|r| info(&r.stored, false)).collect();
        if let Some(store) = &self.config.store {
            let known: HashSet<&str> = infos.iter().map(|i| i.id.as_str()).collect();
            let disk: Vec<BriefingInfo> =
                store.list().iter().filter(|s| !known.contains(s.id.as_str())).map(|s| info(s, true)).collect();
            infos.extend(disk);
        }
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        infos
    }

    pub fn token_for(&self, id: &str) -> Option<String> {
        self.ensure_loaded(id);
        self.records.lock().unwrap().get(id).map(|r| r.stored.token.clone())
    }

    fn snapshot(&self, id: &str) -> Result<(watch::Receiver<BriefingStatus>, Option<BriefingResponse>), HubError> {
        let records = self.records.lock().unwrap();
        let record = records.get(id).ok_or(HubError::NotFound)?;
        Ok((record.status.subscribe(), record.stored.result.clone()))
    }

    /// Wait up to `timeout` for the briefing to finish. Periodically re-reads the on-disk
    /// record so a submission served by another process is picked up too.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<WaitOutcome, HubError> {
        self.ensure_loaded(id);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.reconcile(id);
            let (mut rx, result) = self.snapshot(id)?;
            if *rx.borrow() != BriefingStatus::Active {
                return Ok(WaitOutcome::Done(result.unwrap_or_default()));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(WaitOutcome::Pending);
            }
            let slice = if self.config.store.is_some() { remaining.min(RECONCILE_EVERY) } else { remaining };
            match tokio::time::timeout(slice, rx.wait_for(|s| *s != BriefingStatus::Active)).await {
                Ok(Ok(_)) => {
                    let (_, result) = self.snapshot(id)?;
                    return Ok(WaitOutcome::Done(result.unwrap_or_default()));
                }
                Ok(Err(_)) => return Err(HubError::NotFound),
                Err(_) => continue,
            }
        }
    }

    fn sweep_if_due(&self) {
        if now_secs().saturating_sub(self.last_sweep.load(Ordering::Relaxed)) >= SWEEP_EVERY.as_secs() {
            self.sweep();
        }
    }

    /// Drop expired records (memory and disk). Safe to call any time.
    pub fn sweep(&self) {
        let now = now_secs();
        self.last_sweep.store(now, Ordering::Relaxed);
        self.records.lock().unwrap().retain(|_, record| {
            let stored = &record.stored;
            let keep = match stored.finished_at {
                Some(finished) => now.saturating_sub(finished) < self.config.finished_ttl.as_secs(),
                None => now.saturating_sub(stored.created_at) < self.config.active_ttl.as_secs(),
            };
            if !keep && stored.finished_at.is_none() {
                record.status.send_replace(BriefingStatus::Cancelled);
            }
            keep
        });
        if let Some(store) = &self.config.store {
            store.sweep(self.config.finished_ttl, self.config.active_ttl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;
    use serde_json::json;

    #[tokio::test]
    async fn create_submit_wait_roundtrip() {
        let hub = Hub::new(HubConfig::default());
        let created = hub.create(demo(), Some("test".into()));
        assert_eq!(hub.status(&created.id), Some(BriefingStatus::Active));

        let page = hub.page_payload(&created.token).unwrap();
        assert_eq!(page["status"], "active");
        assert_eq!(page["draft"], Value::Null);
        assert!(hub.page_payload("nope").is_none());

        assert_eq!(hub.wait(&created.id, Duration::from_millis(20)).await, Ok(WaitOutcome::Pending));

        hub.submit_by_token(&created.token, &json!({"overallNote": "great"}), false).unwrap();
        match hub.wait(&created.id, Duration::from_secs(1)).await.unwrap() {
            WaitOutcome::Done(result) => assert_eq!(result.overall_note, "great"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            hub.submit_by_token(&created.token, &json!({}), false),
            Err(HubError::AlreadyFinished(BriefingStatus::Completed))
        );
        assert_eq!(HubError::AlreadyFinished(BriefingStatus::Completed).to_string(), "briefing already completed");
        assert!(!hub.cancel(&created.id));
        assert_eq!(hub.wait("missing", Duration::from_millis(1)).await, Err(HubError::NotFound));
        assert_eq!(hub.info(&created.id).unwrap().source.as_deref(), Some("test"));
    }

    #[test]
    fn drafts_are_revisioned() {
        let hub = Hub::new(HubConfig::default());
        let created = hub.create(demo(), None);
        let draft = json!({"current": 2, "state": {"chunks": {"0": {"note": "hi", "checkpoint": "", "status": ""}}, "decisions": {}, "annotations": [{}], "overallNote": ""}, "updatedAt": 5});
        assert_eq!(hub.save_draft(&created.token, Some(0), draft.clone()), Ok(DraftSave::Saved { revision: 1 }));
        assert_eq!(hub.save_draft(&created.token, None, draft.clone()), Ok(DraftSave::Saved { revision: 2 }));
        match hub.save_draft(&created.token, Some(1), json!({})).unwrap() {
            DraftSave::Stale { revision: 2, draft: existing } => assert_eq!(existing, draft),
            other => panic!("unexpected {other:?}"),
        }
        let page = hub.page_payload(&created.token).unwrap();
        assert_eq!(page["draftRevision"], 2);
        assert_eq!(page["draft"]["current"], 2);
        let summary = hub.info(&created.id).unwrap().draft.unwrap();
        assert_eq!(summary.screen, 3);
        assert_eq!(summary.annotations, 1);
        assert_eq!(summary.section_notes, 1);
        assert_eq!(summary.screens, (demo().chunks.len() + demo().decisions.len() + 1) as u64);
        hub.cancel(&created.id);
        assert!(matches!(hub.save_draft(&created.token, None, json!({})), Err(HubError::AlreadyFinished(_))));
    }

    #[tokio::test]
    async fn wake_waiter_on_cancel() {
        let hub = std::sync::Arc::new(Hub::new(HubConfig::default()));
        let created = hub.create(demo(), None);
        let waiter = {
            let hub = hub.clone();
            let id = created.id.clone();
            tokio::spawn(async move { hub.wait(&id, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(hub.cancel(&created.id));
        match waiter.await.unwrap().unwrap() {
            WaitOutcome::Done(result) => assert!(result.cancelled),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(hub.page_payload(&created.token).unwrap()["status"], "cancelled");
    }

    #[test]
    fn sweep_expires_records() {
        let hub = Hub::new(HubConfig { finished_ttl: Duration::ZERO, active_ttl: Duration::ZERO, store: None });
        let created = hub.create(demo(), None);
        hub.sweep();
        assert!(hub.status(&created.id).is_none());
        assert!(hub.page_payload(&created.token).is_none());
    }

    #[tokio::test]
    async fn records_survive_via_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = || HubConfig { store: Some(Store::open(dir.path()).unwrap()), ..HubConfig::default() };

        // Process A creates a briefing and the user saves a draft, then A dies.
        let a = Hub::new(config());
        let created = a.create(demo(), Some("a".into()));
        assert!(a.set_url(&created.id, "http://a.example/briefing/x"));
        assert!(!a.set_url(&created.id, "http://a.example/briefing/x"));
        a.save_draft(&created.token, None, json!({"current": 1, "state": {}, "updatedAt": 1})).unwrap();
        drop(a);

        // Process B adopts it by id (agent side) and by token (browser side).
        let b = Hub::new(config());
        let info = b.info(&created.id).unwrap();
        assert_eq!(info.url.as_deref(), Some("http://a.example/briefing/x"));
        assert_eq!(info.draft.unwrap().screen, 2);
        assert!(b.has_token(&created.token));
        assert_eq!(b.page_payload(&created.token).unwrap()["draft"]["current"], 1);

        // A third process (the one the browser talks to) submits; B's waiter sees it via disk.
        let c = Hub::new(config());
        c.submit_by_token(&created.token, &json!({"overallNote": "done"}), false).unwrap();
        match b.wait(&created.id, Duration::from_secs(1)).await.unwrap() {
            WaitOutcome::Done(result) => assert_eq!(result.overall_note, "done"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(b.status(&created.id), Some(BriefingStatus::Completed));

        // A brand-new process can fetch the result with no live record at all.
        let d = Hub::new(config());
        assert!(matches!(d.wait(&created.id, Duration::from_millis(1)).await, Ok(WaitOutcome::Done(_))));
        let listed = d.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].on_disk_only);

        // Listing without adopting reports disk-only records.
        let e = Hub::new(config());
        assert!(e.list()[0].on_disk_only);
    }
}

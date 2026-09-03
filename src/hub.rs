//! In-memory registry of presentations awaiting briefing.
//!
//! Every presentation has two unguessable identifiers: the `id` used by the agent
//! side (CLI / MCP / hub API) and the `token` embedded in the browser URL. Nothing
//! is persisted to disk.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::content::Briefing;
use crate::response::{BriefingResponse, parse_browser_result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefingStatus {
    Active,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Pending,
    Done(BriefingResponse),
}

impl PartialEq for BriefingResponse {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}
impl Eq for BriefingResponse {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HubError {
    #[error("briefing not found")]
    NotFound,
    #[error("briefing already {0:?}")]
    AlreadyFinished(BriefingStatus),
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedBriefing {
    pub id: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefingInfo {
    pub id: String,
    pub title: String,
    pub status: BriefingStatus,
    pub age_secs: u64,
    /// Filled in by whoever knows the public origin (backend or hub API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

struct Record {
    id: String,
    token: String,
    presentation: Briefing,
    status: watch::Sender<BriefingStatus>,
    result: Option<BriefingResponse>,
    created: Instant,
    finished: Option<Instant>,
}

pub struct HubConfig {
    /// How long a finished briefing stays fetchable through `wait`/`status`.
    pub finished_ttl: Duration,
    /// How long an unanswered briefing stays open.
    pub active_ttl: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { finished_ttl: Duration::from_secs(60 * 60), active_ttl: Duration::from_secs(24 * 60 * 60) }
    }
}

pub struct Hub {
    config: HubConfig,
    records: Mutex<HashMap<String, Record>>,
    tokens: Mutex<HashMap<String, String>>,
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

impl Hub {
    pub fn new(config: HubConfig) -> Self {
        Self { config, records: Mutex::new(HashMap::new()), tokens: Mutex::new(HashMap::new()) }
    }

    pub fn create(&self, presentation: Briefing) -> CreatedBriefing {
        self.sweep();
        let id = random_token(12);
        let token = random_token(24);
        let (status, _) = watch::channel(BriefingStatus::Active);
        let record = Record {
            id: id.clone(),
            token: token.clone(),
            presentation,
            status,
            result: None,
            created: Instant::now(),
            finished: None,
        };
        self.records.lock().unwrap().insert(id.clone(), record);
        self.tokens.lock().unwrap().insert(token.clone(), id.clone());
        CreatedBriefing { id, token }
    }

    fn id_for_token(&self, token: &str) -> Option<String> {
        self.tokens.lock().unwrap().get(token).cloned()
    }

    /// The JSON the browser page fetches.
    pub fn page_payload(&self, token: &str) -> Option<Value> {
        let id = self.id_for_token(token)?;
        let records = self.records.lock().unwrap();
        let record = records.get(&id)?;
        let mut payload = serde_json::to_value(&record.presentation).ok()?;
        let object = payload.as_object_mut()?;
        object.insert("id".into(), Value::String(record.id.clone()));
        object.insert("status".into(), serde_json::to_value(*record.status.borrow()).ok()?);
        if object.get("mode").is_none_or(Value::is_null) {
            object.insert("mode".into(), Value::String("briefing".into()));
        }
        if !object.contains_key("decisions") {
            object.insert("decisions".into(), Value::Array(vec![]));
        }
        Some(payload)
    }

    fn finish(&self, id: &str, result: BriefingResponse, status: BriefingStatus) -> Result<(), HubError> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(id).ok_or(HubError::NotFound)?;
        let current = *record.status.borrow();
        if current != BriefingStatus::Active {
            return Err(HubError::AlreadyFinished(current));
        }
        record.result = Some(result);
        record.finished = Some(Instant::now());
        record.status.send_replace(status);
        Ok(())
    }

    /// Browser submission (`complete` or `cancel`) for the presentation behind `token`.
    pub fn submit_by_token(&self, token: &str, body: &Value, cancelled: bool) -> Result<(), HubError> {
        let id = self.id_for_token(token).ok_or(HubError::NotFound)?;
        let status = if cancelled { BriefingStatus::Cancelled } else { BriefingStatus::Completed };
        self.finish(&id, parse_browser_result(body, cancelled), status)
    }

    /// Agent-side cancellation. Returns false when the briefing was not active.
    pub fn cancel(&self, id: &str) -> bool {
        self.finish(id, parse_browser_result(&Value::Null, true), BriefingStatus::Cancelled).is_ok()
    }

    pub fn status(&self, id: &str) -> Option<BriefingStatus> {
        self.records.lock().unwrap().get(id).map(|r| *r.status.borrow())
    }

    pub fn info(&self, id: &str) -> Option<BriefingInfo> {
        self.records.lock().unwrap().get(id).map(|r| BriefingInfo {
            id: r.id.clone(),
            title: r.presentation.title.clone(),
            status: *r.status.borrow(),
            age_secs: r.created.elapsed().as_secs(),
            url: None,
        })
    }

    pub fn list(&self) -> Vec<BriefingInfo> {
        let ids: Vec<String> = self.records.lock().unwrap().keys().cloned().collect();
        ids.iter().filter_map(|id| self.info(id)).collect()
    }

    pub fn token_for(&self, id: &str) -> Option<String> {
        self.records.lock().unwrap().get(id).map(|r| r.token.clone())
    }

    fn snapshot(&self, id: &str) -> Result<(watch::Receiver<BriefingStatus>, Option<BriefingResponse>), HubError> {
        let records = self.records.lock().unwrap();
        let record = records.get(id).ok_or(HubError::NotFound)?;
        Ok((record.status.subscribe(), record.result.clone()))
    }

    /// Wait up to `timeout` for the briefing to finish.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<WaitOutcome, HubError> {
        let (mut rx, result) = self.snapshot(id)?;
        if *rx.borrow() != BriefingStatus::Active {
            return Ok(WaitOutcome::Done(result.unwrap_or_default()));
        }
        let finished = tokio::time::timeout(timeout, rx.wait_for(|s| *s != BriefingStatus::Active)).await;
        match finished {
            Ok(Ok(_)) => {
                let (_, result) = self.snapshot(id)?;
                Ok(WaitOutcome::Done(result.unwrap_or_default()))
            }
            Ok(Err(_)) => Err(HubError::NotFound),
            Err(_) => Ok(WaitOutcome::Pending),
        }
    }

    pub fn active_count(&self) -> usize {
        self.records.lock().unwrap().values().filter(|r| *r.status.borrow() == BriefingStatus::Active).count()
    }

    /// Drop expired records. Called on every create; safe to call any time.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut records = self.records.lock().unwrap();
        let mut tokens = self.tokens.lock().unwrap();
        records.retain(|_, record| {
            let keep = match record.finished {
                Some(finished) => now.duration_since(finished) < self.config.finished_ttl,
                None => now.duration_since(record.created) < self.config.active_ttl,
            };
            if !keep {
                if record.finished.is_none() {
                    record.status.send_replace(BriefingStatus::Cancelled);
                }
                tokens.remove(&record.token);
            }
            keep
        });
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
        let created = hub.create(demo());
        assert_eq!(hub.status(&created.id), Some(BriefingStatus::Active));

        let page = hub.page_payload(&created.token).unwrap();
        assert_eq!(page["status"], "active");
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
        assert!(!hub.cancel(&created.id));
        assert_eq!(hub.wait("missing", Duration::from_millis(1)).await, Err(HubError::NotFound));
    }

    #[tokio::test]
    async fn wake_waiter_on_cancel() {
        let hub = std::sync::Arc::new(Hub::new(HubConfig::default()));
        let created = hub.create(demo());
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
        let hub = Hub::new(HubConfig { finished_ttl: Duration::ZERO, active_ttl: Duration::ZERO });
        let created = hub.create(demo());
        hub.sweep();
        assert!(hub.status(&created.id).is_none());
        assert!(hub.page_payload(&created.token).is_none());
    }
}

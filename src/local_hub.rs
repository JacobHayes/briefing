//! This machine's own hub. Without `--hub`, clients talk to a `briefing serve` running here,
//! started in the background on first use. Started that way it exits once nothing has been
//! open for [`IDLE_EXIT`], and the next client starts it again. It reloads its records from the
//! state dir and listens on the same port, so briefings and their links survive both.
//!
//! A state dir has one owner: the hub holding `<state dir>/.hub.lock`, an OS lock released when
//! the process exits however it exits. The owner advertises itself in `<state dir>/.hub.json`
//! (origin, settings, a random instance id, and a control secret). The file outlives the hub, so
//! its settings carry over to the next one; a client trusts it only when `/healthz` at its
//! origin answers with its instance id. A client replaces an older hub that a client started by
//! asking it to exit (`POST /control/shutdown` with the secret), never by signalling a pid.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bind::BindMode;

/// How long a hub started on demand stays up with nothing open.
pub const IDLE_EXIT: Duration = Duration::from_secs(60);
/// Where this machine's hub listens unless told otherwise.
pub const DEFAULT_PORT: u16 = 7789;
const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `serve` waits for the lock: a replacing client may be probing it.
const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);
const FILE: &str = ".hub.json";
const LOCK: &str = ".hub.lock";
const LOG: &str = ".hub.log";

/// What a hub advertises about itself in its state dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubFile {
    /// Where clients on this machine reach it: the bound address, not a public origin.
    pub origin: String,
    pub pid: u32,
    pub version: String,
    /// Started by a client: a newer client replaces it.
    pub on_demand: bool,
    /// The bind mode and port it was started with, reused when a client restarts it.
    pub bind: String,
    pub port: u16,
    /// Random per hub process; `/healthz` answers with it.
    pub instance: String,
    /// Random per hub process; authorizes `/control/shutdown`. Only the state dir holds it.
    pub control: String,
}

impl HubFile {
    pub fn read(dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(dir.join(FILE)).ok()?).ok()
    }

    /// Atomic, so a client never reads half a file; owner-only, since it carries the control
    /// secret.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        crate::store::write_atomic(&dir.join(FILE), &serde_json::to_vec(self)?)
    }
}

/// This process's identity as a hub: what it advertises, and what `/healthz` and
/// `/control/shutdown` check against.
pub struct Identity {
    pub instance: String,
    pub control: String,
}

/// This process's hub identity, minted on first use.
pub fn identity() -> &'static Identity {
    static IDENTITY: OnceLock<Identity> = OnceLock::new();
    IDENTITY.get_or_init(|| Identity { instance: crate::hub::random_id(), control: crate::hub::random_id() })
}

/// Whether `secret` is this process's control secret, compared in constant time.
pub fn control_matches(secret: &str) -> bool {
    let expected = identity().control.as_bytes();
    let given = secret.as_bytes();
    given.len() == expected.len() && given.iter().zip(expected).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn shutdown_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

/// Ask this process's `serve` loop to shut down gracefully (from `/control/shutdown`).
pub fn request_shutdown() {
    shutdown_notify().notify_one();
}

/// Resolves once [`request_shutdown`] has been called, even if that happened first.
pub async fn shutdown_requested() {
    shutdown_notify().notified().await;
}

/// The state dir's ownership lock, held for as long as the value lives.
pub struct HubLock {
    _file: File,
}

impl HubLock {
    /// Take the lock, or `None` while another process holds it. The lock file is never deleted
    /// or replaced, so every process locks the same file.
    pub fn try_acquire(dir: &Path) -> std::io::Result<Option<Self>> {
        let path = dir.join(LOCK);
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// Take the lock, waiting briefly for a client that is only probing it.
    pub async fn acquire(dir: &Path) -> std::io::Result<Option<Self>> {
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            if let Some(lock) = Self::try_acquire(dir)? {
                return Ok(Some(lock));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// A request to a hub on this machine: always plain HTTP to its bound address.
async fn request(method: http::Method, url: &str, bearer: Option<&str>) -> anyhow::Result<(http::StatusCode, Value)> {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Empty<bytes::Bytes>>();
    let mut request = http::Request::builder().method(method).uri(url);
    if let Some(bearer) = bearer {
        request = request.header(http::header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let request = request.body(http_body_util::Empty::new())?;
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let response = client.request(request).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        anyhow::Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    })
    .await?
}

/// The hub that owns `dir`, if one is up: its file's origin answers `/healthz` with its file's
/// instance id. Only the lock holder writes the file, with an instance id minted after taking
/// the lock, so a match is that live owner; a leftover file, or another service on its port,
/// never matches.
pub async fn running(dir: &Path) -> Option<HubFile> {
    let file = HubFile::read(dir)?;
    let (status, health) = request(http::Method::GET, &format!("{}/healthz", file.origin), None).await.ok()?;
    (status.is_success() && health["instance"].as_str() == Some(file.instance.as_str())).then_some(file)
}

/// Whether version `a` is older than `b`, comparing their numeric parts (`2026.09.15.1`,
/// `0.1.0-dev`).
fn older(a: &str, b: &str) -> bool {
    let parts =
        |v: &str| -> Vec<u64> { v.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect() };
    parts(a) < parts(b)
}

/// How a client reaches, starts, and replaces this machine's hub: the state dir it owns, and
/// the bind/port this client was explicitly given (argument, environment, or config file).
pub struct LocalHub {
    dir: PathBuf,
    bind: Option<BindMode>,
    port: Option<u16>,
    warned: AtomicBool,
}

impl LocalHub {
    pub fn new(dir: PathBuf, bind: Option<BindMode>, port: Option<u16>) -> Self {
        Self { dir, bind, port, warned: AtomicBool::new(false) }
    }

    /// The hub's origin, starting it first if nothing answers. A hub that a client started
    /// and that is older than this one is replaced, so the hub is never behind its clients.
    pub async fn origin(&self) -> anyhow::Result<String> {
        if let Some(file) = running(&self.dir).await {
            if !(file.on_demand && older(&file.version, env!("BRIEFING_VERSION"))) {
                self.warn_if_unlike(&file);
                return Ok(file.origin);
            }
            if let Err(error) = self.replace(&file).await {
                tracing::warn!(error = format!("{error:#}"), "could not replace this machine's older hub; using it");
                return Ok(file.origin);
            }
        }
        self.start().await
    }

    /// A running hub wins over this client's own settings; say so once rather than silently.
    fn warn_if_unlike(&self, file: &HubFile) {
        let bind = self.bind.map(|bind| bind.to_string()).filter(|bind| *bind != file.bind);
        let port = self.port.filter(|port| *port != 0 && *port != file.port);
        if (bind.is_some() || port.is_some()) && !self.warned.swap(true, Ordering::Relaxed) {
            let wanted = format!("bind {}, port {}", bind.as_deref().unwrap_or(&file.bind), port.unwrap_or(file.port));
            tracing::warn!(
                "this machine's hub is already running with bind {}, port {} at {}; using it, not the requested {wanted}",
                file.bind,
                file.port,
                file.origin
            );
        }
    }

    /// The `serve` settings for a new hub: this client's explicit ones, else the last hub's (so
    /// its links come back), else the defaults.
    fn settings(&self) -> (String, u16) {
        let last = HubFile::read(&self.dir);
        let bind = self.bind.map(|bind| bind.to_string()).or_else(|| last.as_ref().map(|file| file.bind.clone()));
        let port = self.port.or_else(|| last.map(|file| file.port));
        (bind.unwrap_or_else(|| BindMode::default().to_string()), port.unwrap_or(DEFAULT_PORT))
    }

    async fn start(&self) -> anyhow::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let (bind, port) = self.settings();
        let log_path = self.dir.join(LOG);
        // Truncated per start, so its last line is about this one.
        let log = std::fs::File::create(&log_path)?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args(["serve", "--on-demand", "--bind", &bind, "--port", &port.to_string()])
            .args(["--idle-exit", &format!("{}s", IDLE_EXIT.as_secs())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        // Its own process group, so it outlives the client (and a harness that kills the
        // client's group when the command finishes).
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn()?;
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if let Some(file) = running(&self.dir).await {
                // Reap it whenever it exits, so a long-lived client leaves no zombie.
                std::thread::spawn(move || child.wait());
                return Ok(file.origin);
            }
            if let Some(status) = child.try_wait()? {
                // Another client may have started a hub first; this one lost the lock or port.
                if let Some(file) = running(&self.dir).await {
                    return Ok(file.origin);
                }
                let last = std::fs::read_to_string(&log_path).unwrap_or_default();
                let last = last.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").to_string();
                anyhow::bail!(
                    "this machine's hub exited ({status}) before it was ready: {last} (log: {})",
                    log_path.display()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                // Do not leave a half-started hub behind, or its zombie.
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("this machine's hub did not start within {START_TIMEOUT:?}; see {}", log_path.display());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Ask an outdated hub to exit and wait until it has released the state dir. Its records
    /// stay on disk for the replacement; in-flight requests finish first.
    async fn replace(&self, file: &HubFile) -> anyhow::Result<()> {
        tracing::info!(pid = file.pid, version = file.version, "replacing this machine's older hub");
        let url = format!("{}/control/shutdown", file.origin);
        let (status, _) = request(http::Method::POST, &url, Some(&file.control)).await?;
        anyhow::ensure!(status.is_success(), "it answered {status}");
        let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
        loop {
            if HubLock::try_acquire(&self.dir)?.is_some() {
                return Ok(());
            }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "it did not exit within {STOP_TIMEOUT:?}");
            tokio::time::sleep(POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(older("2026.09.15.1", "2026.09.17.1"));
        assert!(older("2026.09.9.1", "2026.09.15.1"));
        assert!(older("0.1.0-dev", "2026.09.15.1"));
        assert!(!older("2026.09.15.1", "2026.09.15.1"));
        assert!(!older("2026.09.17.1", "2026.09.15.1"));
    }

    fn file(port: u16) -> HubFile {
        HubFile {
            origin: format!("http://127.0.0.1:{port}"),
            pid: 1,
            version: "v".into(),
            on_demand: true,
            bind: "local".into(),
            port,
            instance: "i".into(),
            control: "c".into(),
        }
    }

    #[test]
    fn hub_file_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        file(9).write(dir.path()).unwrap();
        let read = HubFile::read(dir.path()).unwrap();
        assert_eq!((read.port, read.bind.as_str(), read.control.as_str()), (9, "local", "c"));
    }

    #[test]
    fn the_lock_has_one_holder_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let held = HubLock::try_acquire(dir.path()).unwrap().expect("free at first");
        assert!(HubLock::try_acquire(dir.path()).unwrap().is_none(), "second holder refused");
        drop(held);
        assert!(HubLock::try_acquire(dir.path()).unwrap().is_some(), "free again once dropped");
    }

    #[test]
    fn a_new_hub_takes_explicit_settings_then_the_last_hubs_then_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = LocalHub::new(dir.path().into(), None, None);
        assert_eq!(fresh.settings(), ("auto".into(), DEFAULT_PORT));
        file(9001).write(dir.path()).unwrap();
        assert_eq!(fresh.settings(), ("local".into(), 9001), "the last hub's settings come back");
        let explicit = LocalHub::new(dir.path().into(), Some(BindMode::Tailscale), Some(9002));
        assert_eq!(explicit.settings(), ("tailscale".into(), 9002), "explicit settings win");
    }

    #[test]
    fn control_secret_must_match_exactly() {
        let secret = identity().control.clone();
        assert!(control_matches(&secret));
        assert!(!control_matches(""));
        assert!(!control_matches(&format!("{secret}x")));
        assert!(!control_matches(&"a".repeat(secret.len())));
    }
}

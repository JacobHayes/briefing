//! Where presentations live: an embedded server in this process, or a remote hub.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::OnceCell;

use crate::browser;
use crate::content::Briefing;
use crate::http::{self, AppState, HttpConfig};
use crate::hub::{BriefingInfo, BriefingStatus, Hub, HubConfig, WaitOutcome};
use crate::response::BriefingResponse;
use crate::tailscale::{self, BindScope, BindTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum BindMode {
    /// Tailscale 100.x address when Tailscale is running, otherwise loopback.
    #[default]
    Auto,
    /// 127.0.0.1 only.
    Local,
    /// Require the Tailscale address; fall back to loopback with a diagnostic if unavailable.
    Tailscale,
}

impl BindMode {
    pub async fn target(self) -> BindTarget {
        match self {
            BindMode::Local => BindTarget::local(None),
            BindMode::Auto | BindMode::Tailscale => tailscale::detect_bind_target().await,
        }
    }
}

/// What a caller learns after creating a briefing.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub id: String,
    pub url: String,
    /// "local", "tailnet", or "hub".
    pub scope: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
    pub opened_browser: bool,
}

/// Best-effort machine name for the `source` label on briefings (computed once).
pub fn hostname() -> &'static str {
    static HOSTNAME: OnceLock<String> = OnceLock::new();
    HOSTNAME.get_or_init(|| {
        let from_env = std::env::var("HOSTNAME").ok();
        let from_file = || std::fs::read_to_string("/etc/hostname").ok();
        let from_command =
            || std::process::Command::new("hostname").output().ok().and_then(|o| String::from_utf8(o.stdout).ok());
        from_env
            .or_else(from_file)
            .or_else(from_command)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "local".into())
    })
}

pub struct LocalServer {
    pub state: AppState,
    pub scope: BindScope,
    pub label: String,
    pub bind_host: String,
    pub diagnostics: Option<String>,
    running: Option<http::RunningServer>,
}

/// Embedded server: starts lazily on the first presentation (or the first adopted active
/// briefing) and stays up for the life of the process.
pub struct LocalBackend {
    hub: Arc<Hub>,
    bind: BindMode,
    open_browser: bool,
    on_create: Option<String>,
    server: OnceCell<LocalServer>,
}

impl LocalBackend {
    pub fn new(bind: BindMode, open_browser: bool, on_create: Option<String>, config: HubConfig) -> Self {
        Self { hub: Arc::new(Hub::new(config)), bind, open_browser, on_create, server: OnceCell::new() }
    }

    /// Use an already-running server (hub mode) instead of starting one.
    pub fn attached(state: AppState, target: &BindTarget, open_browser: bool) -> Self {
        let hub = state.hub.clone();
        let server = LocalServer {
            state,
            scope: target.scope,
            label: target.label.clone(),
            bind_host: target.bind_host.clone(),
            diagnostics: None,
            running: None,
        };
        let cell = OnceCell::new();
        cell.set(server).ok();
        Self { hub, bind: BindMode::Local, open_browser, on_create: None, server: cell }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    async fn start(&self, target: BindTarget) -> anyhow::Result<LocalServer> {
        let listener = http::bind(&target.bind_host, 0)
            .await
            .map_err(|error| anyhow::anyhow!("{} bind failed: {error}", target.label))?;
        let port = listener.local_addr()?.port();
        let state = AppState {
            hub: self.hub.clone(),
            config: Arc::new(HttpConfig {
                public_origin: http::origin_for(&target.public_host, port),
                allowed_hosts: vec![target.public_host.clone()],
                agent_api: false,
                on_create: self.on_create.clone(),
            }),
        };
        let running = http::serve_listener(http::router(state.clone(), None), listener)?;
        Ok(LocalServer {
            state,
            scope: target.scope,
            label: target.label,
            bind_host: target.bind_host,
            diagnostics: target.diagnostics,
            running: Some(running),
        })
    }

    async fn ensure_server(&self) -> anyhow::Result<&LocalServer> {
        self.server
            .get_or_try_init(|| async {
                let preferred = self.bind.target().await;
                let fallback = (preferred.scope == BindScope::Tailnet).then(|| {
                    BindTarget::local(Some(format!(
                        "Fell back to local loopback after {} bind failed",
                        preferred.label
                    )))
                });
                match self.start(preferred).await {
                    Ok(server) => Ok(server),
                    Err(error) => match fallback {
                        Some(fallback) => {
                            tracing::warn!(%error, "falling back to loopback");
                            self.start(fallback).await
                        }
                        None => Err(error),
                    },
                }
            })
            .await
    }

    pub async fn create(&self, presentation: Briefing, source: Option<String>) -> anyhow::Result<Created> {
        let server = self.ensure_server().await?;
        let (id, url) = http::create_briefing(&server.state, presentation, source)?;
        let mut opened = false;
        if self.open_browser {
            match browser::open_url(&url).await {
                Ok(did_open) => opened = did_open,
                Err(error) => {
                    self.hub.cancel(&id);
                    return Err(error);
                }
            }
        }
        Ok(Created {
            id,
            url,
            scope: server.scope.label().to_string(),
            label: server.label.clone(),
            bind_host: Some(server.bind_host.clone()),
            diagnostics: server.diagnostics.clone(),
            opened_browser: opened,
        })
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<WaitOutcome> {
        Ok(self.hub.wait(id, timeout).await?)
    }

    pub fn cancel(&self, id: &str) -> bool {
        self.hub.cancel(id)
    }

    /// Status of a briefing. An active briefing is served by this process (starting the
    /// embedded server if needed), so the returned URL is live even for adopted records;
    /// `reopened` is set the first time that link differs from the one on record.
    pub async fn info(&self, id: &str) -> anyhow::Result<Option<BriefingInfo>> {
        let Some(mut info) = self.hub.info(id) else {
            return Ok(None);
        };
        if info.status == BriefingStatus::Active {
            let server = self.ensure_server().await?;
            if let Some(url) = http::url_for(&server.state, id) {
                info.reopened = self.hub.set_url(id, &url);
                info.url = Some(url);
            }
        }
        Ok(Some(info))
    }

    pub fn list(&self) -> Vec<BriefingInfo> {
        let mut infos = self.hub.list();
        if let Some(server) = self.server.get() {
            http::with_live_urls(&server.state, &mut infos);
        }
        infos
    }

    pub async fn shutdown(self) {
        if let Some(server) = self.server.into_inner()
            && let Some(running) = server.running
        {
            running.stop().await;
        }
    }
}

const HUB_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimal HTTP(S) client for the hub API: hyper + rustls with bundled webpki roots, so the
/// binary cross-compiles without platform TLS frameworks.
pub struct RemoteBackend {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
    base: String,
}

#[derive(serde::Deserialize)]
struct RemoteCreated {
    id: String,
    url: String,
}

#[derive(serde::Deserialize)]
struct RemoteWait {
    status: String,
    #[serde(default)]
    result: Option<BriefingResponse>,
}

#[derive(serde::Deserialize)]
struct RemoteError {
    error: String,
}

#[derive(serde::Deserialize)]
struct RemoteList {
    briefings: Vec<BriefingInfo>,
}

impl RemoteBackend {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        crate::tls::init();
        let https =
            hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots().https_or_http().enable_http1().build();
        let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
        Ok(Self { client, base: base.trim_end_matches('/').to_string() })
    }

    async fn request(
        &self,
        method: ::http::Method,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        use http_body_util::BodyExt;
        let uri: ::http::Uri = format!("{}{path}", self.base).parse()?;
        let mut request =
            ::http::Request::builder().method(method).uri(uri).header(::http::header::ACCEPT, "application/json");
        let payload = match body {
            Some(value) => {
                request = request.header(::http::header::CONTENT_TYPE, "application/json");
                serde_json::to_vec(&value)?
            }
            None => Vec::new(),
        };
        let request = request.body(http_body_util::Full::new(bytes::Bytes::from(payload)))?;
        let response = tokio::time::timeout(timeout, self.client.request(request))
            .await
            .map_err(|_| anyhow::anyhow!("hub request timed out"))??;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        let value = if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes)? };
        if status.is_success() {
            return Ok(value);
        }
        if status == ::http::StatusCode::NOT_FOUND {
            return Err(NotFound.into());
        }
        let detail =
            serde_json::from_value::<RemoteError>(value.clone()).map(|e| e.error).unwrap_or_else(|_| value.to_string());
        anyhow::bail!("hub returned {status}: {detail}")
    }

    pub async fn create(&self, presentation: Briefing, source: Option<String>) -> anyhow::Result<Created> {
        let value = self
            .request(
                ::http::Method::POST,
                "/agent/briefings",
                Some(serde_json::json!({"presentation": presentation, "source": source})),
                HUB_REQUEST_TIMEOUT,
            )
            .await?;
        let created: RemoteCreated = serde_json::from_value(value)?;
        Ok(Created {
            id: created.id,
            url: created.url,
            scope: "hub".into(),
            label: format!("hub {}", self.base),
            bind_host: None,
            diagnostics: None,
            opened_browser: false,
        })
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<WaitOutcome> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(WaitOutcome::Pending);
            }
            let slice = remaining.min(http::MAX_WAIT);
            let path = format!("/agent/briefings/{id}/wait?timeout_secs={}", slice.as_secs().max(1));
            let value = self.request(::http::Method::GET, &path, None, slice + HUB_REQUEST_TIMEOUT).await?;
            let wait: RemoteWait = serde_json::from_value(value)?;
            match (wait.status.as_str(), wait.result) {
                ("pending", _) => continue,
                (_, Some(result)) => return Ok(WaitOutcome::Done(result)),
                (status, None) => anyhow::bail!("hub returned status {status} without a result"),
            }
        }
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<bool> {
        let value = self
            .request(::http::Method::POST, &format!("/agent/briefings/{id}/cancel"), None, HUB_REQUEST_TIMEOUT)
            .await?;
        Ok(value["cancelled"].as_bool().unwrap_or(false))
    }

    pub async fn info(&self, id: &str) -> anyhow::Result<Option<BriefingInfo>> {
        match self.request(::http::Method::GET, &format!("/agent/briefings/{id}"), None, HUB_REQUEST_TIMEOUT).await {
            Ok(value) => Ok(Some(serde_json::from_value(value)?)),
            Err(error) if error.is::<NotFound>() => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<BriefingInfo>> {
        let value = self.request(::http::Method::GET, "/agent/briefings", None, HUB_REQUEST_TIMEOUT).await?;
        let list: RemoteList = serde_json::from_value(value)?;
        Ok(list.briefings)
    }
}

/// The hub answered 404.
#[derive(Debug, thiserror::Error)]
#[error("briefing not found on the hub")]
struct NotFound;

pub enum Backend {
    Local(LocalBackend),
    Remote(RemoteBackend),
}

impl Backend {
    pub async fn create(&self, presentation: Briefing, source: Option<String>) -> anyhow::Result<Created> {
        match self {
            Backend::Local(local) => local.create(presentation, source).await,
            Backend::Remote(remote) => remote.create(presentation, source).await,
        }
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<WaitOutcome> {
        match self {
            Backend::Local(local) => local.wait(id, timeout).await,
            Backend::Remote(remote) => remote.wait(id, timeout).await,
        }
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<bool> {
        match self {
            Backend::Local(local) => Ok(local.cancel(id)),
            Backend::Remote(remote) => remote.cancel(id).await,
        }
    }

    pub async fn info(&self, id: &str) -> anyhow::Result<Option<BriefingInfo>> {
        match self {
            Backend::Local(local) => local.info(id).await,
            Backend::Remote(remote) => remote.info(id).await,
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<BriefingInfo>> {
        match self {
            Backend::Local(local) => Ok(local.list()),
            Backend::Remote(remote) => remote.list().await,
        }
    }

    pub async fn shutdown(self) {
        if let Backend::Local(local) = self {
            local.shutdown().await;
        }
    }
}

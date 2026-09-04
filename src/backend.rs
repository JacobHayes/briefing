//! Where presentations live: an embedded server in this process, or a remote hub.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::browser;
use crate::content::{self, Briefing};
use crate::http::{self, HttpConfig, RunningServer};
use crate::hub::{BriefingInfo, BriefingStatus, Hub, HubConfig, Provenance};
use crate::response::Outcome;
use crate::tailscale::{self, BindScope, BindTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum BindMode {
    /// Tailscale 100.x address when Tailscale is running, otherwise loopback.
    #[default]
    Auto,
    /// 127.0.0.1 only.
    Local,
    /// The Tailscale address, or fail: never serve on loopback (for a headless box where a
    /// loopback link would be useless).
    Tailscale,
}

impl BindMode {
    pub async fn target(self) -> anyhow::Result<BindTarget> {
        match self {
            BindMode::Local => Ok(BindTarget::local(None)),
            BindMode::Auto => Ok(tailscale::detect_bind_target().await),
            BindMode::Tailscale => {
                tailscale::detect().await.map_err(|reason| anyhow::anyhow!("--bind tailscale: {reason}"))
            }
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

/// How a [`Site`] is exposed and what it does when a briefing is created.
#[derive(Debug, Clone, Default)]
pub struct SiteOptions {
    /// Serve the dashboard at `/` and the agent API under `/agent/*` (hub mode).
    pub agent_api: bool,
    /// Origin to put in briefing URLs when behind a reverse proxy; by default the bound address.
    pub public_origin: Option<String>,
    /// Try to open new briefings in this machine's browser.
    pub open_browser: bool,
    /// Shell command run when a briefing is created; receives `BRIEFING_URL`, `BRIEFING_ID`,
    /// `BRIEFING_TITLE`.
    pub on_create: Option<String>,
}

/// One briefing server: the registry, how it is reached, and the side effects of creating a
/// briefing. Every entry point (CLI, MCP over stdio or HTTP, the hub agent API) creates
/// briefings through [`Site::create`], so they all honour `--open` and `--on-create` alike.
pub struct Site {
    pub hub: Arc<Hub>,
    pub config: HttpConfig,
    pub target: BindTarget,
    open_browser: bool,
    on_create: Option<String>,
}

impl Site {
    /// Bind `target` on `port` (0 = ephemeral) and serve it. `mcp` may add routes (an MCP
    /// service under `/mcp`) once the site exists; they are mounted only with the agent API.
    pub async fn start(
        hub: Arc<Hub>,
        target: BindTarget,
        port: u16,
        options: SiteOptions,
        mcp: impl FnOnce(&Arc<Site>) -> Option<Router<Arc<Site>>>,
    ) -> anyhow::Result<(Arc<Site>, RunningServer)> {
        let listener = http::bind(&target.bind_host, port)
            .await
            .map_err(|error| anyhow::anyhow!("{} bind failed: {error}", target.label))?;
        let port = listener.local_addr()?.port();
        let public_origin = options
            .public_origin
            .map(|origin| origin.trim_end_matches('/').to_string())
            .unwrap_or_else(|| http::origin_for(&target.public_host, port));
        let site = Arc::new(Site {
            hub,
            config: HttpConfig::new(public_origin, &target.public_host, options.agent_api),
            target,
            open_browser: options.open_browser,
            on_create: options.on_create,
        });
        let running = http::serve_listener(http::router(site.clone(), mcp(&site)), listener)?;
        Ok((site, running))
    }

    /// Validate and register a presentation, remember its link, run the on-create hook, and
    /// open the browser when configured. A browser opener that fails cancels the briefing.
    pub async fn create(&self, presentation: Briefing, source: Option<String>) -> anyhow::Result<Created> {
        let validated = content::validate(&presentation)?;
        let title = validated.title.clone();
        let created = self.hub.create(validated, source);
        let url = self.config.briefing_url(&created.token);
        self.hub.set_url(&created.id, &url);
        if let Some(hook) = &self.on_create {
            run_on_create_hook(hook, &url, &created.id, &title);
        }
        let mut opened = false;
        if self.open_browser {
            match browser::open_url(&url).await {
                Ok(did_open) => opened = did_open,
                Err(error) => {
                    self.hub.cancel(&created.id);
                    return Err(error);
                }
            }
        }
        Ok(Created {
            id: created.id,
            url,
            scope: self.target.scope.label().to_string(),
            label: self.target.label.clone(),
            bind_host: Some(self.target.bind_host.clone()),
            diagnostics: self.target.diagnostics.clone(),
            opened_browser: opened,
        })
    }

    /// URL at which this site serves briefing `id` (its origin plus the record's token).
    pub fn url_for(&self, id: &str) -> Option<String> {
        let token = self.hub.token_for(id)?;
        Some(self.config.briefing_url(&token))
    }

    /// Point every record this site holds at this site's link.
    pub fn with_live_urls(&self, infos: &mut [BriefingInfo]) {
        for info in infos.iter_mut().filter(|info| info.provenance != Provenance::DiskOnly) {
            info.url = self.url_for(&info.id);
        }
    }
}

fn run_on_create_hook(command: &str, url: &str, id: &str, title: &str) {
    let command = command.to_string();
    let (url, id, title) = (url.to_string(), id.to_string(), title.to_string());
    tokio::spawn(async move {
        let result = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("BRIEFING_URL", url)
            .env("BRIEFING_ID", id)
            .env("BRIEFING_TITLE", title)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        match result {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                tracing::warn!(status = %output.status, stderr = %String::from_utf8_lossy(&output.stderr), "on-create hook failed")
            }
            Err(error) => tracing::warn!(%error, "on-create hook could not start"),
        }
    });
}

enum Server {
    /// Starts on the first presentation (or the first adopted active briefing) and stays up
    /// for the life of the process.
    Lazy { bind: BindMode, options: SiteOptions, started: OnceCell<(Arc<Site>, RunningServer)> },
    /// Hub mode: this process already serves the site.
    Attached(Arc<Site>),
}

/// Briefings served by this process.
pub struct LocalBackend {
    hub: Arc<Hub>,
    server: Server,
}

impl LocalBackend {
    pub fn new(bind: BindMode, options: SiteOptions, config: HubConfig) -> Self {
        Self { hub: Arc::new(Hub::new(config)), server: Server::Lazy { bind, options, started: OnceCell::new() } }
    }

    /// Use an already-running site (hub mode) instead of starting one.
    pub fn attached(site: Arc<Site>) -> Self {
        Self { hub: site.hub.clone(), server: Server::Attached(site) }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    fn site(&self) -> Option<&Arc<Site>> {
        match &self.server {
            Server::Lazy { started, .. } => started.get().map(|(site, _)| site),
            Server::Attached(site) => Some(site),
        }
    }

    async fn ensure_site(&self) -> anyhow::Result<&Arc<Site>> {
        let (bind, options, started) = match &self.server {
            Server::Attached(site) => return Ok(site),
            Server::Lazy { bind, options, started } => (*bind, options, started),
        };
        let start = |target| Site::start(self.hub.clone(), target, 0, options.clone(), |_| None);
        started
            .get_or_try_init(|| async {
                let preferred = bind.target().await?;
                let fallback = (bind == BindMode::Auto && preferred.scope == BindScope::Tailnet).then(|| {
                    BindTarget::local(Some(format!(
                        "Fell back to local loopback after {} bind failed",
                        preferred.label
                    )))
                });
                match start(preferred).await {
                    Ok(started) => Ok(started),
                    Err(error) => match fallback {
                        Some(fallback) => {
                            tracing::warn!(%error, "falling back to loopback");
                            start(fallback).await
                        }
                        None => Err(error),
                    },
                }
            })
            .await
            .map(|(site, _)| site)
    }

    pub async fn create(&self, presentation: Briefing, source: Option<String>) -> anyhow::Result<Created> {
        self.ensure_site().await?.create(presentation, source).await
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<Outcome> {
        Ok(self.hub.wait(id, timeout).await?)
    }

    pub fn cancel(&self, id: &str) -> bool {
        self.hub.cancel(id)
    }

    /// Status of a briefing. An active briefing is served by this process (starting the
    /// embedded server if needed), so the returned URL is live even for adopted records;
    /// the provenance is `Reopened` the first time that link differs from the one on record.
    pub async fn info(&self, id: &str) -> anyhow::Result<Option<BriefingInfo>> {
        let Some(mut info) = self.hub.info(id) else {
            return Ok(None);
        };
        if info.status == BriefingStatus::Active
            && let Some(url) = self.ensure_site().await?.url_for(id)
        {
            if self.hub.set_url(id, &url) {
                info.provenance = Provenance::Reopened;
            }
            info.url = Some(url);
        }
        Ok(Some(info))
    }

    pub fn list(&self) -> Vec<BriefingInfo> {
        let mut infos = self.hub.list();
        if let Some(site) = self.site() {
            site.with_live_urls(&mut infos);
        }
        infos
    }

    pub async fn shutdown(self) {
        if let Server::Lazy { started, .. } = self.server
            && let Some((_, running)) = started.into_inner()
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

    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<Outcome> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(Outcome::Pending);
            }
            let slice = remaining.min(http::MAX_WAIT);
            let path = format!("/agent/briefings/{id}/wait?timeout_secs={}", slice.as_secs().max(1));
            let value = self.request(::http::Method::GET, &path, None, slice + HUB_REQUEST_TIMEOUT).await?;
            match serde_json::from_value(value)? {
                Outcome::Pending => continue,
                done => return Ok(done),
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

    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<Outcome> {
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

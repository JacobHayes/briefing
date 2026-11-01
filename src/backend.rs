//! Where presentations live: always in a hub. A client reaches one over HTTP - the configured
//! `--hub`, or this machine's own [`LocalHub`] - except the hub's own `/mcp`, which uses the
//! [`Site`] it is part of.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::bind::{BindTarget, Scope};
use crate::browser;
use crate::content::{self, Briefing};
use crate::http::{self, HttpConfig, RunningServer};
use crate::hub::{BriefingInfo, Hub, Origin, SWEEP_EVERY};
use crate::images::{self, Image, LocalFiles};
use crate::local_hub::LocalHub;
use crate::response::{BriefingOutcome, Outcome};

/// What a caller learns after creating a briefing.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Created {
    #[serde(rename = "briefingId")]
    pub id: String,
    pub url: String,
    pub scope: Scope,
    #[serde(default)]
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

/// A hub's server: the registry and how it is reached. Every entry point (CLI, MCP over stdio or
/// HTTP, the agent API) creates briefings through [`Site::create`], so they all share validation.
pub struct Site {
    pub hub: Arc<Hub>,
    pub config: HttpConfig,
    pub target: BindTarget,
    /// How far its links reach: the bound address's scope, unless a public origin fronts it.
    pub scope: Scope,
}

impl Site {
    /// Bind `target` on `port` (0 = ephemeral) and serve it. `public_origin` goes in briefing
    /// URLs when a reverse proxy fronts the hub (by default the bound address). `mcp` may add
    /// routes (an MCP service under `/mcp`) once the site exists.
    pub async fn start(
        hub: Arc<Hub>,
        target: BindTarget,
        port: u16,
        public_origin: Option<String>,
        mcp: impl FnOnce(&Arc<Site>) -> Option<Router<Arc<Site>>>,
    ) -> anyhow::Result<(Arc<Site>, RunningServer)> {
        let listener = http::bind(target.host, port)
            .await
            .map_err(|error| anyhow::anyhow!("{} bind failed: {error}", target.label))?;
        let port = listener.local_addr()?.port();
        let scope = if public_origin.is_some() { Scope::Hub } else { target.scope };
        let public_origin = public_origin
            .map(|origin| origin.trim_end_matches('/').to_string())
            .unwrap_or_else(|| http::origin_for(target.host, port));
        let config = HttpConfig::new(public_origin, target.host);
        let site = Arc::new(Site { hub, config, target, scope });
        let running = http::serve_listener(http::router(site.clone(), mcp(&site)), listener)?;
        let sweeper = start_hub_sweeper(site.hub.clone(), running.shutdown.clone(), SWEEP_EVERY);
        Ok((site, running.with_background_task(sweeper)))
    }

    /// Validate and register a presentation. Opening a browser is the creating [`Backend`]'s
    /// job, not the server's.
    pub async fn create(&self, presentation: Briefing, images: Vec<Image>, origin: Origin) -> anyhow::Result<Created> {
        let validated = content::validate(&presentation)?;
        images::validate(&validated, &images)?;
        let id = self.hub.create(validated, images, origin)?;
        let url = self.config.briefing_url(&id);
        Ok(Created { id, url, scope: self.scope, opened_browser: false })
    }

    /// A briefing with its link.
    pub fn info(&self, id: &str) -> Option<BriefingInfo> {
        self.hub.info(id).map(|info| self.with_url(info))
    }

    /// Every briefing with its link.
    pub fn list(&self) -> Vec<BriefingInfo> {
        self.hub.list().into_iter().map(|info| self.with_url(info)).collect()
    }

    fn with_url(&self, mut info: BriefingInfo) -> BriefingInfo {
        info.url = Some(self.config.briefing_url(&info.id));
        info
    }
}

fn start_hub_sweeper(hub: Arc<Hub>, shutdown: CancellationToken, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(every) => hub.sweep(),
            }
        }
    })
}

const HUB_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HUB_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a configured hub may stay unreachable before a wait on it gives up: long enough to
/// ride through a restart or upgrade, short enough that a hub that is really gone surfaces as
/// an error.
const REMOTE_HUB_OUTAGE: Duration = Duration::from_secs(60);
/// How long to wait before trying a hub that went away mid-wait again.
const HUB_RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
#[error("hub request timed out")]
struct HubRequestTimeout;

/// The transport failed: nothing listening at the hub's address, or the connection dropped before
/// the hub answered (it exited or was replaced mid-request), as opposed to an error from a hub
/// that answered.
#[derive(Debug, thiserror::Error)]
#[error("hub at {0} is not reachable")]
pub struct HubUnreachable(String);

/// Minimal HTTP(S) client for the hub API: hyper + rustls with bundled webpki roots, so the
/// binary cross-compiles without platform TLS frameworks. Clones share one connection pool.
#[derive(Clone)]
pub struct RemoteBackend {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
    base: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteError {
    error: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
        let mut request = ::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(::http::header::ACCEPT, "application/json")
            .header(crate::protocol::HEADER, crate::protocol::PROTOCOL.to_string());
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
            .map_err(|_| HubRequestTimeout)?
            .map_err(|_| HubUnreachable(self.base.clone()))?;
        let status = response.status();
        // A proxy in front of a hub that is down or restarting answers for it: the hub is
        // unreachable, not speaking some other protocol. The hub itself never sends these.
        if matches!(
            status,
            ::http::StatusCode::BAD_GATEWAY
                | ::http::StatusCode::SERVICE_UNAVAILABLE
                | ::http::StatusCode::GATEWAY_TIMEOUT
        ) {
            return Err(HubUnreachable(self.base.clone()).into());
        }
        // A response in another protocol has shapes this client would misread, so stop here
        // with both versions named. A 426 carries the hub's own explanation instead.
        let hub_protocol = response.headers().get(crate::protocol::HEADER).and_then(|v| v.to_str().ok());
        if status != ::http::StatusCode::UPGRADE_REQUIRED
            && let Err(error) = crate::protocol::check_hub(hub_protocol, &self.base)
        {
            anyhow::bail!(error);
        }
        let bytes = response.into_body().collect().await.map_err(|_| HubUnreachable(self.base.clone()))?.to_bytes();
        let value = if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes)? };
        if status.is_success() {
            return Ok(value);
        }
        if status == ::http::StatusCode::NOT_FOUND {
            return Err(NotFound.into());
        }
        let detail =
            serde_json::from_value::<RemoteError>(value.clone()).map(|e| e.error).unwrap_or_else(|_| value.to_string());
        // A 400 is the hub rejecting the presentation: its message is what the caller must fix.
        if status == ::http::StatusCode::BAD_REQUEST {
            anyhow::bail!("{detail}");
        }
        anyhow::bail!("hub returned {status}: {detail}")
    }

    pub async fn create(&self, presentation: Briefing, images: Vec<Image>, origin: Origin) -> anyhow::Result<Created> {
        // Images can make this request tens of megabytes.
        let timeout = if images.is_empty() { HUB_REQUEST_TIMEOUT } else { HUB_UPLOAD_TIMEOUT };
        let value = self
            .request(
                ::http::Method::POST,
                "/agent/briefings",
                Some(serde_json::to_value(http::CreateRequest { presentation, images, origin })?),
                timeout,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Wait up to `timeout` for the briefing to finish, retrying while the hub is unreachable
    /// for up to `outage` at a time: a restarted hub reloads its records, so the wait goes on.
    pub async fn wait(&self, id: &str, timeout: Duration, outage: Duration) -> anyhow::Result<Outcome> {
        self.wait_with_limits(id, timeout, outage, http::MAX_WAIT, HUB_REQUEST_TIMEOUT).await
    }

    async fn wait_with_limits(
        &self,
        id: &str,
        timeout: Duration,
        outage: Duration,
        max_slice: Duration,
        request_timeout: Duration,
    ) -> anyhow::Result<Outcome> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut unreachable_since = None;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(Outcome::Pending);
            }
            let slice = remaining.min(max_slice);
            let path = format!("/agent/briefings/{id}/wait?timeout_secs={}", slice.as_secs().max(1));
            let value = match self.request(::http::Method::GET, &path, None, slice + request_timeout).await {
                Ok(value) => value,
                // A remote long-poll can outlive one HTTP request even though the briefing is
                // still valid on the hub. Treat transport timeouts as "still pending" and
                // reconnect instead of losing the briefing id.
                Err(error) if error.is::<HubRequestTimeout>() => continue,
                Err(error) if error.is::<HubUnreachable>() => {
                    let since = *unreachable_since.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed() >= outage {
                        return Err(error);
                    }
                    tracing::debug!("{error}; retrying");
                    tokio::time::sleep(HUB_RETRY.min(remaining)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            unreachable_since = None;
            match serde_json::from_value::<BriefingOutcome>(value)?.outcome {
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

/// How a client reaches its hub.
pub enum BackendKind {
    /// Over HTTP at a configured URL (`--hub`).
    Remote(RemoteBackend),
    /// Over HTTP to this machine's own hub, started on demand.
    Local(LocalHub),
    /// In-process: the hub's own `/mcp` uses the site it is part of.
    Site(Arc<Site>),
}

/// How a client process creates and follows briefings: a [`BackendKind`] plus what this
/// machine does once one is created. Opening the browser lives here, not on the server, so a
/// headless hub never opens one.
pub struct Backend {
    kind: BackendKind,
    open_browser: bool,
}

/// How many times in a row restarting it may fail (about 10 s): the hub it replaces can still
/// hold the port while it shuts down.
const LOCAL_HUB_RESTARTS: u32 = 20;

impl Backend {
    pub fn new(kind: BackendKind, open_browser: bool) -> Self {
        Self { kind, open_browser }
    }

    /// A connection to the hub, starting this machine's hub first if it is not running.
    async fn connect(&self) -> anyhow::Result<Conn<'_>> {
        Ok(match &self.kind {
            BackendKind::Remote(remote) => Conn::Http(remote.clone()),
            BackendKind::Local(local) => Conn::Http(RemoteBackend::new(&local.origin().await?)?),
            BackendKind::Site(site) => Conn::Site(site),
        })
    }

    /// Create a briefing and, when configured, try to open it in this machine's browser. The
    /// briefing is live either way, so a failed opener is only a warning: the caller still has
    /// the link to show. Local images in its Markdown are read from this machine, with relative
    /// paths taken from `image_dir`; the hub's own `/mcp` never reads them.
    pub async fn create(
        &self,
        mut presentation: Briefing,
        image_dir: &Path,
        origin: Origin,
    ) -> anyhow::Result<Created> {
        let files = match self.kind {
            BackendKind::Site(_) => LocalFiles::Refused,
            _ => LocalFiles::From(image_dir.to_path_buf()),
        };
        let images = images::collect(&mut presentation, &files)?;
        let mut created = match self.connect().await? {
            Conn::Http(remote) => remote.create(presentation, images, origin).await?,
            Conn::Site(site) => site.create(presentation, images, origin).await?,
        };
        if self.open_browser {
            match browser::open_url(&created.url).await {
                Ok(opened) => created.opened_browser = opened,
                Err(error) => tracing::warn!(error = format!("{error:#}"), "could not open the browser"),
            }
        }
        Ok(created)
    }

    /// Wait up to `timeout` for the briefing to finish. This machine's hub may exit or be
    /// replaced mid-wait; it reloads its records when it starts again, so keep waiting on it.
    pub async fn wait(&self, id: &str, timeout: Duration) -> anyhow::Result<Outcome> {
        let local = match &self.kind {
            BackendKind::Remote(remote) => return remote.wait(id, timeout, REMOTE_HUB_OUTAGE).await,
            BackendKind::Site(site) => return Ok(site.hub.wait(id, timeout).await?),
            BackendKind::Local(local) => local,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let mut failed_restarts = 0;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let remote = match local.origin().await.and_then(|origin| RemoteBackend::new(&origin)) {
                Ok(remote) => remote,
                Err(error) if failed_restarts < LOCAL_HUB_RESTARTS => {
                    tracing::debug!(error = format!("{error:#}"), "this machine's hub is not back yet");
                    failed_restarts += 1;
                    tokio::time::sleep(HUB_RETRY).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            failed_restarts = 0;
            // This machine's hub may need starting again, which only re-finding it does.
            match remote.wait(id, remaining, Duration::ZERO).await {
                Err(error) if error.is::<HubUnreachable>() => tokio::time::sleep(HUB_RETRY).await,
                result => return result,
            }
        }
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<bool> {
        match self.connect().await? {
            Conn::Http(remote) => remote.cancel(id).await,
            Conn::Site(site) => Ok(site.hub.cancel(id)?),
        }
    }

    pub async fn info(&self, id: &str) -> anyhow::Result<Option<BriefingInfo>> {
        match self.connect().await? {
            Conn::Http(remote) => remote.info(id).await,
            Conn::Site(site) => Ok(site.info(id)),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<BriefingInfo>> {
        match self.connect().await? {
            Conn::Http(remote) => remote.list().await,
            Conn::Site(site) => Ok(site.list()),
        }
    }
}

enum Conn<'a> {
    Http(RemoteBackend),
    Site(&'a Arc<Site>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::HubConfig;
    use serde_json::json;

    #[tokio::test]
    async fn background_sweeper_expires_records() {
        let hub =
            Arc::new(Hub::new(HubConfig { finished_ttl: Duration::ZERO, active_ttl: Duration::ZERO, store: None }));
        let created = hub.create(content::demo(), Vec::new(), Origin::default()).unwrap();
        let shutdown = CancellationToken::new();
        let task = start_hub_sweeper(hub.clone(), shutdown.clone(), Duration::from_millis(10));

        tokio::time::timeout(Duration::from_secs(1), async {
            while hub.status(&created).is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn remote_wait_retries_hub_request_timeouts() {
        crate::tls::init();
        let hub = Arc::new(Hub::new(HubConfig::default()));
        let (site, running) = Site::start(hub, BindTarget::local(None), 0, None, |_| None).await.unwrap();
        let origin = site.config.public_origin.clone();
        let remote = RemoteBackend::new(&origin).unwrap();
        let created = remote.create(content::demo(), Vec::new(), Origin::source("test")).await.unwrap();

        let (submit_origin, id) = (origin.clone(), created.id.clone());
        let submitter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            submit(&submit_origin, &id, "retried").await;
        });

        let outcome = remote
            .wait_with_limits(
                &created.id,
                Duration::from_secs(2),
                Duration::ZERO,
                Duration::from_millis(25),
                Duration::from_millis(5),
            )
            .await
            .unwrap();
        match outcome {
            Outcome::Completed { feedback } => assert_eq!(feedback.notes, vec!["retried"]),
            other => panic!("unexpected {other:?}"),
        }
        submitter.await.unwrap();
        running.stop().await;
    }

    /// Submit the demo briefing `id` through the page API with one note.
    async fn submit(origin: &str, id: &str, note: &str) {
        let response = reqwest::Client::new()
            .post(format!("{origin}/api/{id}/complete"))
            .header("origin", origin)
            .header(crate::protocol::HEADER, crate::protocol::PROTOCOL.to_string())
            .json(&{
                let mut body = crate::response::blank_submission(&content::demo());
                body["notes"] = json!([note]);
                body
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// A configured hub restarting mid-wait: first nothing listens, then a proxy answers 502 for
    /// it, then the hub is back on the same port with its records. The wait carries on.
    #[tokio::test]
    async fn remote_wait_rides_through_a_hub_restart() {
        crate::tls::init();
        let port = free_port();
        let hub = Arc::new(Hub::new(HubConfig::default()));
        let (site, running) = Site::start(hub.clone(), BindTarget::local(None), port, None, |_| None).await.unwrap();
        let origin = site.config.public_origin.clone();
        let remote = RemoteBackend::new(&origin).unwrap();
        let created = remote.create(content::demo(), Vec::new(), Origin::source("test")).await.unwrap();
        running.stop().await;

        let (restart_origin, id) = (origin.clone(), created.id.clone());
        let restarter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let listener = http::bind(std::net::Ipv4Addr::LOCALHOST.into(), port).await.unwrap();
            let router = axum::Router::new().fallback(|| async { ::http::StatusCode::BAD_GATEWAY });
            let proxy = tokio::spawn(async move { axum::serve(listener, router).await });
            tokio::time::sleep(Duration::from_millis(1200)).await;
            proxy.abort();
            let _ = proxy.await;
            let (_, running) = Site::start(hub, BindTarget::local(None), port, None, |_| None).await.unwrap();
            submit(&restart_origin, &id, "after restart").await;
            running
        });

        let outcome = remote
            .wait_with_limits(
                &created.id,
                Duration::from_secs(10),
                Duration::from_secs(5),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        match outcome {
            Outcome::Completed { feedback } => assert_eq!(feedback.notes, vec!["after restart"]),
            other => panic!("unexpected {other:?}"),
        }
        restarter.await.unwrap().stop().await;
    }

    /// A hub that stays gone past the outage allowance ends the wait with an error.
    #[tokio::test]
    async fn remote_wait_gives_up_on_a_hub_that_stays_gone() {
        crate::tls::init();
        let remote = RemoteBackend::new(&format!("http://127.0.0.1:{}", free_port())).unwrap();
        let error = remote
            .wait_with_limits(
                "gone",
                Duration::from_secs(10),
                Duration::from_millis(600),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(error.is::<HubUnreachable>(), "{error:#}");
    }
}

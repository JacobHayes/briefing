//! HTTP surface: the browser briefing page + its API, and (in hub mode) the dashboard, the
//! agent API, and an MCP endpoint used by remote harnesses.
//!
//! There is no authentication: the hub is meant to sit on a private network (a tailnet),
//! and every briefing URL carries its own capability token. Host and Origin checks guard
//! against DNS rebinding and cross-site requests.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::assets;
use crate::content::{self, Briefing};
use crate::hub::{BriefingInfo, DraftSave, Hub, HubError, WaitOutcome, random_token};

/// Browser submission cap: 500 annotations of 2 KB quote + 4 KB comment plus notes fits well inside.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_AGENT_REQUEST_BYTES: usize = content::MAX_PRESENTATION_BYTES + 64 * 1024;
pub const MAX_WAIT: Duration = Duration::from_secs(600);
pub const DEFAULT_WAIT: Duration = Duration::from_secs(30);

pub struct HttpConfig {
    /// Origin used to build briefing URLs, e.g. `http://127.0.0.1:41234` or `https://briefings.example`.
    pub public_origin: String,
    /// Host header values (with or without port) accepted by every route.
    pub allowed_hosts: Vec<String>,
    /// Serve the dashboard at `/` and the agent API under `/agent/*` (hub mode).
    pub agent_api: bool,
    /// Shell command run when a briefing is created; receives `BRIEFING_URL`, `BRIEFING_ID`,
    /// `BRIEFING_TITLE`.
    pub on_create: Option<String>,
}

/// `host` or `host:port` as it appears in a Host header, for a parsed URL.
pub fn host_with_port(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

impl HttpConfig {
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim();
        let bare = host
            .rsplit_once(':')
            .filter(|(h, p)| !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            .map(|(h, _)| h);
        self.allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host) || bare.is_some_and(|b| allowed.eq_ignore_ascii_case(b)))
    }

    pub fn origin_allowed(&self, origin: &str) -> bool {
        let Ok(url) = url::Url::parse(origin.trim()) else {
            return false;
        };
        if url.scheme() != "http" && url.scheme() != "https" {
            return false;
        }
        host_with_port(&url).is_some_and(|host| self.host_allowed(&host))
    }

    pub fn briefing_url(&self, token: &str) -> String {
        format!("{}/briefing/{token}", self.public_origin.trim_end_matches('/'))
    }
}

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub config: Arc<HttpConfig>,
}

fn text(status: StatusCode, body: &'static str) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], body).into_response()
}

fn json_response(status: StatusCode, value: Value) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], Json(value))
        .into_response()
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        let status = match self {
            HubError::NotFound => StatusCode::NOT_FOUND,
            HubError::AlreadyFinished(_) => StatusCode::CONFLICT,
        };
        json_response(status, json!({"error": self.to_string()}))
    }
}

async fn check_host(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let host = request.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !state.config.host_allowed(host) {
        return text(StatusCode::FORBIDDEN, "Forbidden");
    }
    next.run(request).await
}

async fn healthz() -> Response {
    json_response(StatusCode::OK, json!({"ok": true}))
}

async fn asset(Path(name): Path<String>) -> Response {
    match assets::asset(&name) {
        Some(bytes) => (
            StatusCode::OK,
            [
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                (header::CONTENT_TYPE, HeaderValue::from_static("application/javascript; charset=utf-8")),
                (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            ],
            bytes,
        )
            .into_response(),
        None => text(StatusCode::NOT_FOUND, "Asset not found"),
    }
}

fn html(body: String, csp: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::CONTENT_SECURITY_POLICY, csp),
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::X_FRAME_OPTIONS, "DENY".to_string()),
        ],
        body,
    )
        .into_response()
}

async fn page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    if !state.hub.has_token(&token) {
        return text(StatusCode::NOT_FOUND, "Briefing not found");
    }
    let nonce = random_token(18);
    let csp = format!(
        "default-src 'none'; script-src 'self' 'nonce-{nonce}' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; \
         connect-src 'self' http: https:; img-src 'self' data: http: https:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    );
    html(assets::render(assets::PAGE_HTML, &nonce), csp)
}

async fn dashboard() -> Response {
    let nonce = random_token(18);
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; connect-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    html(assets::render(assets::DASHBOARD_HTML, &nonce), csp)
}

async fn presentation(State(state): State<AppState>, Path(token): Path<String>) -> Result<Response, HubError> {
    let payload = state.hub.page_payload(&token).ok_or(HubError::NotFound)?;
    Ok(json_response(StatusCode::OK, payload))
}

fn origin_ok(state: &AppState, headers: &HeaderMap) -> bool {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).is_some_and(|origin| state.config.origin_allowed(origin))
}

async fn submit(state: AppState, headers: HeaderMap, token: String, body: Value, cancelled: bool) -> Response {
    if !origin_ok(&state, &headers) {
        return text(StatusCode::FORBIDDEN, "Forbidden");
    }
    match state.hub.submit_by_token(&token, &body, cancelled) {
        Ok(()) => json_response(StatusCode::OK, json!({"ok": true})),
        Err(error) => error.into_response(),
    }
}

async fn complete(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    submit(state, headers, token, body, false).await
}

async fn cancel_from_browser(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    submit(state, headers, token, body, true).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftRequest {
    /// Revision the browser last loaded or saved; omit to overwrite unconditionally.
    #[serde(default)]
    pub base_revision: Option<u64>,
    pub draft: Value,
}

/// `PUT /api/{token}/draft`: 200 `{revision}` when saved, 409 `{revision, draft}` when the
/// server has a newer draft than `baseRevision`.
async fn save_draft(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Json(body): Json<DraftRequest>,
) -> Result<Response, HubError> {
    if !origin_ok(&state, &headers) {
        return Ok(text(StatusCode::FORBIDDEN, "Forbidden"));
    }
    if !body.draft.is_object() {
        return Ok(json_response(StatusCode::BAD_REQUEST, json!({"error": "draft must be an object"})));
    }
    Ok(match state.hub.save_draft(&token, body.base_revision, body.draft)? {
        DraftSave::Saved { revision } => json_response(StatusCode::OK, json!({"revision": revision})),
        DraftSave::Stale { revision, draft } => {
            json_response(StatusCode::CONFLICT, json!({"error": "stale", "revision": revision, "draft": draft}))
        }
    })
}

// ---- Agent API (hub mode) ----

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    pub presentation: Briefing,
    /// Who created it, e.g. `claude-code@laptop`; shown on the dashboard.
    #[serde(default)]
    pub source: Option<String>,
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

/// Register a presentation and return `{ id, url }`. Shared by the HTTP agent API and
/// the in-process backends.
pub fn create_briefing(
    state: &AppState,
    input: Briefing,
    source: Option<String>,
) -> Result<(String, String), content::ValidationError> {
    let validated = content::validate(&input)?;
    let title = validated.title.clone();
    let created = state.hub.create(validated, source);
    let url = state.config.briefing_url(&created.token);
    state.hub.set_url(&created.id, &url);
    if let Some(hook) = &state.config.on_create {
        run_on_create_hook(hook, &url, &created.id, &title);
    }
    Ok((created.id, url))
}

/// URL at which this server serves briefing `id` (its origin plus the record's token).
pub fn url_for(state: &AppState, id: &str) -> Option<String> {
    let token = state.hub.token_for(id)?;
    Some(state.config.briefing_url(&token))
}

/// Point every record this server holds at this server's link.
pub fn with_live_urls(state: &AppState, infos: &mut [BriefingInfo]) {
    for info in infos.iter_mut().filter(|info| !info.on_disk_only) {
        info.url = url_for(state, &info.id);
    }
}

async fn agent_create(State(state): State<AppState>, Json(body): Json<CreateRequest>) -> Response {
    match create_briefing(&state, body.presentation, body.source) {
        Ok((id, url)) => json_response(StatusCode::CREATED, json!({"id": id, "url": url})),
        Err(error) => json_response(StatusCode::BAD_REQUEST, json!({"error": error.to_string()})),
    }
}

async fn agent_list(State(state): State<AppState>) -> Response {
    let mut briefings = state.hub.list();
    with_live_urls(&state, &mut briefings);
    json_response(StatusCode::OK, json!({"briefings": briefings}))
}

async fn agent_info(State(state): State<AppState>, Path(id): Path<String>) -> Result<Response, HubError> {
    let mut info = state.hub.info(&id).ok_or(HubError::NotFound)?;
    info.url = url_for(&state, &id);
    Ok(json_response(StatusCode::OK, serde_json::to_value(info).unwrap_or_default()))
}

#[derive(Deserialize)]
pub struct WaitQuery {
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

pub fn clamp_wait(requested: Option<u64>) -> Duration {
    requested.map(Duration::from_secs).unwrap_or(DEFAULT_WAIT).min(MAX_WAIT)
}

async fn agent_wait(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> Result<Response, HubError> {
    Ok(match state.hub.wait(&id, clamp_wait(query.timeout_secs)).await? {
        WaitOutcome::Pending => json_response(StatusCode::OK, json!({"status": "pending"})),
        WaitOutcome::Done(result) => {
            json_response(StatusCode::OK, json!({"status": result.status(), "result": result}))
        }
    })
}

async fn agent_cancel(State(state): State<AppState>, Path(id): Path<String>) -> Result<Response, HubError> {
    state.hub.status(&id).ok_or(HubError::NotFound)?;
    Ok(json_response(StatusCode::OK, json!({"ok": true, "cancelled": state.hub.cancel(&id)})))
}

/// Build the router. `mcp` is an optional router (e.g. one that nests an MCP service at
/// `/mcp`); it is only mounted when the agent API is enabled.
pub fn router(state: AppState, mcp: Option<Router<AppState>>) -> Router {
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route(&format!("{}{{name}}", assets::ASSET_PREFIX), get(asset))
        .route("/briefing/{token}", get(page))
        .route("/api/{token}/presentation", get(presentation))
        .route("/api/{token}/draft", axum::routing::put(save_draft))
        .route("/api/{token}/complete", post(complete))
        .route("/api/{token}/cancel", post(cancel_from_browser))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES));

    if state.config.agent_api {
        let mut agent = Router::new()
            .route("/", get(dashboard))
            .route("/agent/briefings", post(agent_create).get(agent_list))
            .route("/agent/briefings/{id}", get(agent_info))
            .route("/agent/briefings/{id}/wait", get(agent_wait))
            .route("/agent/briefings/{id}/cancel", post(agent_cancel))
            .layer(DefaultBodyLimit::max(MAX_AGENT_REQUEST_BYTES));
        if let Some(mcp) = mcp {
            agent = agent.merge(mcp);
        }
        app = app.merge(agent);
    }
    app.layer(middleware::from_fn_with_state(state.clone(), check_host)).with_state(state)
}

pub struct RunningServer {
    pub local_addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

/// Bind `host:port` (port 0 = ephemeral). Serve it with [`serve_listener`].
pub async fn bind(host: &str, port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind((host, port)).await
}

/// Serve `router` on an already-bound listener until the returned token is cancelled.
pub fn serve_listener(router: Router, listener: tokio::net::TcpListener) -> std::io::Result<RunningServer> {
    let local_addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    let task = tokio::spawn(async move {
        let server = axum::serve(listener, router).with_graceful_shutdown(async move { signal.cancelled().await });
        if let Err(error) = server.await {
            tracing::error!(%error, "briefing http server stopped");
        }
    });
    Ok(RunningServer { local_addr, shutdown, task })
}

pub fn origin_for(public_host: &str, port: u16) -> String {
    if public_host.contains(':') && !public_host.starts_with('[') {
        format!("http://[{public_host}]:{port}")
    } else {
        format!("http://{public_host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> HttpConfig {
        HttpConfig {
            public_origin: "http://127.0.0.1:4000".into(),
            allowed_hosts: vec!["127.0.0.1".into(), "briefings.example".into()],
            agent_api: false,
            on_create: None,
        }
    }

    #[test]
    fn host_and_origin_checks() {
        let config = config();
        assert!(config.host_allowed("127.0.0.1:4000"));
        assert!(config.host_allowed("127.0.0.1"));
        assert!(config.host_allowed("Briefings.Example"));
        assert!(!config.host_allowed("localhost:4000"));
        assert!(!config.host_allowed("evil.example:4000"));
        assert!(!config.host_allowed(""));
        assert!(config.origin_allowed("http://127.0.0.1:4000"));
        assert!(config.origin_allowed("https://briefings.example"));
        assert!(!config.origin_allowed("null"));
        assert!(!config.origin_allowed("http://attacker.example"));
        assert_eq!(config.briefing_url("abc"), "http://127.0.0.1:4000/briefing/abc");
        assert_eq!(origin_for("fd7a::1", 8), "http://[fd7a::1]:8");
        assert_eq!(clamp_wait(Some(10_000)), MAX_WAIT);
        assert_eq!(clamp_wait(None), DEFAULT_WAIT);
    }
}

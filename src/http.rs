//! HTTP surface: the browser briefing page + its API, and (optionally) the agent API and
//! an MCP endpoint used by remote harnesses when running as a hub.

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
use crate::hub::{BriefingStatus, Hub, HubError, WaitOutcome, random_token};

/// Browser submission cap: 500 annotations of 2 KB quote + 4 KB comment plus notes fits well inside.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_AGENT_REQUEST_BYTES: usize = content::MAX_PRESENTATION_BYTES + 64 * 1024;
pub const MAX_WAIT: Duration = Duration::from_secs(600);
pub const DEFAULT_WAIT: Duration = Duration::from_secs(30);

/// A shell command run when a briefing is created (hub mode); receives
/// `BRIEFING_URL`, `BRIEFING_ID`, `BRIEFING_TITLE`.
#[derive(Clone, Debug)]
pub struct OnCreateHook(pub String);

pub struct HttpConfig {
    /// Origin used to build briefing URLs, e.g. `http://127.0.0.1:41234` or `https://briefings.example`.
    pub public_origin: String,
    /// Host header values (with or without port) accepted by every route.
    pub allowed_hosts: Vec<String>,
    /// Bearer token protecting `/agent/*` and `/mcp`. `None` disables the agent API.
    pub agent_token: Option<String>,
    pub on_create: Option<OnCreateHook>,
}

impl HttpConfig {
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        let bare = host
            .rsplit_once(':')
            .filter(|(h, p)| !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            .map(|(h, _)| h.to_string());
        self.allowed_hosts.iter().any(|allowed| {
            let allowed = allowed.to_ascii_lowercase();
            allowed == host || Some(&allowed) == bare.as_ref()
        })
    }

    pub fn origin_allowed(&self, origin: &str) -> bool {
        let Ok(url) = url::Url::parse(origin.trim()) else {
            return false;
        };
        if url.scheme() != "http" && url.scheme() != "https" {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        self.host_allowed(&host)
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

fn hub_error(error: HubError) -> Response {
    match error {
        HubError::NotFound => json_response(StatusCode::NOT_FOUND, json!({"error": "Briefing not found"})),
        HubError::AlreadyFinished(status) => {
            json_response(StatusCode::CONFLICT, json!({"error": format!("Briefing already {}", status_word(status))}))
        }
    }
}

fn status_word(status: BriefingStatus) -> &'static str {
    match status {
        BriefingStatus::Active => "active",
        BriefingStatus::Completed => "completed",
        BriefingStatus::Cancelled => "cancelled",
    }
}

async fn check_host(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let host = request.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !state.config.host_allowed(host) {
        return text(StatusCode::FORBIDDEN, "Forbidden");
    }
    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_bearer(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let Some(expected) = state.config.agent_token.as_deref() else {
        return text(StatusCode::NOT_FOUND, "Not found");
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if presented.is_empty() || !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "Unauthorized").into_response();
    }
    next.run(request).await
}

async fn healthz() -> Response {
    json_response(StatusCode::OK, json!({"ok": true}))
}

async fn asset(Path(name): Path<String>) -> Response {
    match assets::asset(&format!("/briefing-assets/{name}")) {
        Some(asset) => (
            StatusCode::OK,
            [
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                (header::CONTENT_TYPE, HeaderValue::from_static(asset.content_type)),
                (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            ],
            asset.bytes,
        )
            .into_response(),
        None => text(StatusCode::NOT_FOUND, "Asset not found"),
    }
}

async fn page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    if state.hub.page_payload(&token).is_none() {
        return text(StatusCode::NOT_FOUND, "Briefing not found");
    }
    let nonce = random_token(18);
    let body = assets::render_page(&nonce);
    let csp = format!(
        "default-src 'none'; script-src 'self' 'nonce-{nonce}' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; \
         connect-src 'self' http: https:; img-src 'self' data: http: https:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    );
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

async fn presentation(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    match state.hub.page_payload(&token) {
        Some(payload) => json_response(StatusCode::OK, payload),
        None => hub_error(HubError::NotFound),
    }
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
        Err(error) => hub_error(error),
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

// ---- Agent API (hub mode) ----

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    pub presentation: Briefing,
}

pub fn run_on_create_hook(hook: &OnCreateHook, url: &str, id: &str, title: &str) {
    let command = hook.0.clone();
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
pub fn create_briefing(state: &AppState, input: Briefing) -> Result<(String, String), content::ValidationError> {
    let validated = content::validate(&input)?;
    let title = validated.title.clone();
    let created = state.hub.create(validated);
    let url = state.config.briefing_url(&created.token);
    if let Some(hook) = &state.config.on_create {
        run_on_create_hook(hook, &url, &created.id, &title);
    }
    Ok((created.id, url))
}

async fn agent_create(State(state): State<AppState>, Json(body): Json<CreateRequest>) -> Response {
    match create_briefing(&state, body.presentation) {
        Ok((id, url)) => json_response(StatusCode::CREATED, json!({"id": id, "url": url})),
        Err(error) => json_response(StatusCode::BAD_REQUEST, json!({"error": error.to_string()})),
    }
}

async fn agent_list(State(state): State<AppState>) -> Response {
    json_response(StatusCode::OK, json!({"briefings": state.hub.list()}))
}

async fn agent_info(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.hub.info(&id) {
        Some(mut info) => {
            info.url = state.hub.token_for(&id).map(|t| state.config.briefing_url(&t));
            json_response(StatusCode::OK, serde_json::to_value(info).unwrap_or_default())
        }
        None => hub_error(HubError::NotFound),
    }
}

#[derive(Deserialize)]
pub struct WaitQuery {
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

pub fn clamp_wait(requested: Option<u64>) -> Duration {
    requested.map(Duration::from_secs).unwrap_or(DEFAULT_WAIT).min(MAX_WAIT)
}

async fn agent_wait(State(state): State<AppState>, Path(id): Path<String>, Query(query): Query<WaitQuery>) -> Response {
    match state.hub.wait(&id, clamp_wait(query.timeout_secs)).await {
        Ok(WaitOutcome::Pending) => json_response(StatusCode::OK, json!({"status": "pending"})),
        Ok(WaitOutcome::Done(result)) => json_response(
            StatusCode::OK,
            json!({"status": if result.cancelled { "cancelled" } else { "completed" }, "result": result}),
        ),
        Err(error) => hub_error(error),
    }
}

async fn agent_cancel(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.hub.status(&id).is_none() {
        return hub_error(HubError::NotFound);
    }
    json_response(StatusCode::OK, json!({"ok": true, "cancelled": state.hub.cancel(&id)}))
}

/// Build the router. `mcp` is an optional router (e.g. one that nests an MCP service at
/// `/mcp`); it is placed behind the same bearer check as the agent API.
pub fn router(state: AppState, mcp: Option<Router<AppState>>) -> Router {
    let browser = Router::new()
        .route("/healthz", get(healthz))
        .route("/briefing-assets/{name}", get(asset))
        .route("/briefing/{token}", get(page))
        .route("/api/{token}/presentation", get(presentation))
        .route("/api/{token}/complete", post(complete))
        .route("/api/{token}/cancel", post(cancel_from_browser))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES));

    let mut app = browser;
    if state.config.agent_token.is_some() {
        let agent = Router::new()
            .route("/agent/briefings", post(agent_create).get(agent_list))
            .route("/agent/briefings/{id}", get(agent_info))
            .route("/agent/briefings/{id}/wait", get(agent_wait))
            .route("/agent/briefings/{id}/cancel", post(agent_cancel))
            .layer(DefaultBodyLimit::max(MAX_AGENT_REQUEST_BYTES));
        let mut protected = agent;
        if let Some(mcp) = mcp {
            protected = protected.merge(mcp);
        }
        app = app.merge(protected.layer(middleware::from_fn_with_state(state.clone(), require_bearer)));
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

/// Bind and serve in one step.
pub async fn serve(router: Router, host: &str, port: u16) -> std::io::Result<RunningServer> {
    serve_listener(router, bind(host, port).await?)
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
            agent_token: None,
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

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}

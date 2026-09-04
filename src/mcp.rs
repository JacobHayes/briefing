//! MCP server exposing `brief_user`, `await_briefing`, and `cancel_briefing`.
//!
//! The same handler serves stdio (`briefing mcp`) and streamable HTTP
//! (`briefing serve --mcp`).

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    BooleanSchema, CallToolResult, ClientResult, ElicitRequest, ElicitRequestParams, ElicitationAction,
    ElicitationSchema, ErrorData, Implementation, PrimitiveSchemaDefinition, ProgressNotificationParam,
    ServerCapabilities, ServerInfo, ServerRequest,
};
use rmcp::service::{PeerRequestOptions, RequestContext};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Created};
use crate::content::{Briefing, schema_value};
use crate::hub::{BriefingStatus, WaitOutcome};
use crate::response::BriefingResponse;

/// How to keep a long `await_briefing` call alive while the human reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum HoldMode {
    /// Pick from the MCP client's `initialize` handshake (name + capabilities); see
    /// `ClientProfile`. Never the resolved mode of a call.
    #[default]
    Auto,
    /// Send `notifications/progress` heartbeats while waiting (Claude Code resets its
    /// timeout on progress).
    Progress,
    /// Send a form elicitation ("I have submitted the briefing") and wait for it. Codex
    /// pauses its tool timeout while an elicitation is pending. Falls back to progress
    /// when the client does not advertise elicitation support.
    Elicitation,
    /// Plain wait.
    None,
}

pub const HEARTBEAT: Duration = Duration::from_secs(10);

pub const INSTRUCTIONS: &str = "\
Briefing presents complex information in a paced browser interface and returns the user's notes, inline comments, decisions, and follow-up markers.

Use brief_user proactively whenever an answer crosses a complexity threshold: substantial research with dependent findings, multi-part explanations, or decisions that need context. Keep short and simple answers as normal chat. Finish the research and reasoning first, then call brief_user once with 3-8 semantic chunks in dependency order (one main idea per chunk, 3-5 keyPoints each, focused details, stable context in tray, 2-4 distinct decision options with the recommended one first and marked). Text fields accept Markdown, GFM tables, fenced code, Mermaid fences, and Vega-Lite fences; use them only when they clarify.

Results are returned as structuredContent. brief_user returns immediately with the briefing link and a briefingId; put that exact link in your reply so the user can open it (they may be on a different machine from the agent), then call await_briefing with the briefingId; it blocks until they submit and returns their feedback. If await_briefing returns status \"pending\", call it again. If your harness moves the call to the background, stop and wait for its completion notification; do not poll. After the feedback arrives, respond only to it; do not repeat the presentation as a chat message.

Briefings outlive the process that created them (unanswered ones for two weeks, results for 6 h). If a session was interrupted, or the user gives you a briefingId, call await_briefing with it: it returns the stored feedback if they already submitted, or reopens the briefing (status \"reopened\" with a fresh link to relay) if not.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AwaitParams {
    /// The briefingId returned by brief_user.
    pub briefing_id: String,
    /// Maximum seconds to wait before returning pending again (server may cap it).
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    /// The briefingId returned by brief_user.
    pub briefing_id: String,
}

/// `brief_user` output: the briefing is open and waiting.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenOutput {
    /// Always "open".
    pub status: String,
    pub briefing_id: String,
    /// Link the user must open. Show it verbatim.
    pub url: String,
    /// Whether the server opened a browser on its own machine.
    pub opened_browser: bool,
    /// "local", "tailnet", or "hub".
    pub scope: String,
    /// What the model should do next.
    pub instructions: String,
}

/// `await_briefing` output.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AwaitOutput {
    /// "completed", "cancelled", "pending", or "reopened" (a briefing from an earlier
    /// process is being served again at `url`; relay the link, then call await_briefing again).
    pub status: String,
    pub briefing_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The user's feedback; absent while pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<BriefingResponse>,
    /// What the model should do next.
    pub instructions: String,
}

/// `cancel_briefing` output.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelOutput {
    pub briefing_id: String,
    /// False when the briefing had already finished.
    pub cancelled: bool,
}

/// structuredContent was introduced in this protocol version; older clients would only
/// see the one-line text block.
const MIN_PROTOCOL: &str = "2025-06-18";

fn output_schema<T: JsonSchema>() -> Arc<rmcp::model::JsonObject> {
    Arc::new(schema_value::<T>().as_object().cloned().expect("schema is an object"))
}

fn structured<T: Serialize>(text: String, value: &T) -> CallToolResult {
    let mut result = CallToolResult::structured(serde_json::to_value(value).expect("output serializes"));
    result.content = vec![rmcp::model::ContentBlock::text(text)];
    result
}

pub struct BriefingMcp {
    backend: Arc<Backend>,
    hold: HoldMode,
    /// Explicit budget; `None` means pick per client.
    max_wait: Option<Duration>,
    tool_router: ToolRouter<Self>,
}

/// What a known MCP client can tolerate, derived from `clientInfo.name` and the advertised
/// capabilities in `initialize` (no model involvement). See docs/harness-timeouts.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientProfile {
    pub name: &'static str,
    /// Case-insensitive substrings of `clientInfo.name` that select this profile; a
    /// leading `=` means the whole name must match.
    needles: &'static [&'static str],
    /// Preferred hold when the client supports it.
    pub hold: HoldMode,
    /// Longest a single call may block before returning `pending`.
    pub budget: Duration,
}

const HOURS_4: Duration = Duration::from_secs(4 * 60 * 60);
/// Just under Claude Code's ~28 h wall-clock cap; its idle timer is reset by heartbeats.
const HOURS_24: Duration = Duration::from_secs(24 * 60 * 60);

const fn profile(
    name: &'static str,
    needles: &'static [&'static str],
    hold: HoldMode,
    budget: Duration,
) -> ClientProfile {
    ClientProfile { name, needles, hold, budget }
}

/// First match wins; the last entry is the fallback.
static PROFILES: &[ClientProfile] = &[
    // Wall-clock 300 s default that ignores progress but pauses during an elicitation.
    profile("codex", &["codex"], HoldMode::Elicitation, Duration::from_secs(280)),
    // Idle timer resets on progress; wall-clock cap is ~28 h.
    profile("claude-code", &["claude"], HoldMode::Progress, HOURS_24),
    profile("gemini-cli", &["gemini"], HoldMode::Progress, Duration::from_secs(570)),
    profile("goose", &["goose"], HoldMode::Progress, Duration::from_secs(280)),
    // No client-side timeout.
    profile("vscode", &["vscode", "visual studio", "copilot"], HoldMode::Progress, HOURS_24),
    // 60 s, no progress reset, often not configurable.
    profile(
        "sixty-second-client",
        &["cursor", "cline", "zed", "continue", "opencode", "windsurf"],
        HoldMode::Progress,
        Duration::from_secs(50),
    ),
    profile("pi", &["=pi", "pi-mcp", "pi-coding-agent"], HoldMode::Progress, Duration::from_secs(50)),
    profile("unknown", &[], HoldMode::Progress, Duration::from_secs(50)),
];

impl ClientProfile {
    pub fn for_client(client_name: &str) -> ClientProfile {
        let name = client_name.to_ascii_lowercase();
        let matches = |needle: &str| match needle.strip_prefix('=') {
            Some(exact) => name == exact,
            None => name.contains(needle),
        };
        *PROFILES.iter().find(|p| p.needles.iter().any(|n| matches(n))).unwrap_or(&PROFILES[PROFILES.len() - 1])
    }

    /// Budget for the resolved hold: an elicitation pauses the client's timer, so the
    /// wall-clock budget can be long.
    pub fn budget_for(&self, hold: HoldMode) -> Duration {
        if hold == HoldMode::Elicitation { HOURS_4 } else { self.budget }
    }
}

/// Turn a requested mode into one this client can take (never `Auto`).
fn resolve_hold(mode: HoldMode, supports_elicitation: bool) -> HoldMode {
    match mode {
        HoldMode::Elicitation if !supports_elicitation => {
            tracing::warn!("client does not support elicitation; using progress heartbeats");
            HoldMode::Progress
        }
        HoldMode::Auto => HoldMode::Progress,
        mode => mode,
    }
}

fn internal(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

fn cancelled_by_client(id: &str) -> ErrorData {
    ErrorData::internal_error(
        format!(
            "await cancelled by the client; briefing {id} stays open, call await_briefing again or cancel_briefing"
        ),
        None,
    )
}

impl BriefingMcp {
    pub fn new(backend: Arc<Backend>, hold: HoldMode, max_wait: Option<Duration>) -> Self {
        Self { backend, hold, max_wait, tool_router: Self::tool_router() }
    }

    /// `<client>@<host>`, shown on the dashboard and in `briefing status`.
    fn source(ctx: &RequestContext<RoleServer>) -> String {
        let client = Self::client_name(ctx);
        let client = if client.trim().is_empty() { "mcp".to_string() } else { client };
        format!("{client}@{}", crate::backend::hostname())
    }

    fn client_name(ctx: &RequestContext<RoleServer>) -> String {
        ctx.peer.peer_info().map(|info| info.client_info.name.clone()).unwrap_or_default()
    }

    /// Hold + budget for this call: explicit flags win, otherwise the client profile.
    fn plan(&self, ctx: &RequestContext<RoleServer>) -> (HoldMode, Duration) {
        let supports_elicitation = ctx.peer.peer_info().is_some_and(|info| info.capabilities.elicitation.is_some());
        let profile = ClientProfile::for_client(&Self::client_name(ctx));
        let hold =
            resolve_hold(if self.hold == HoldMode::Auto { profile.hold } else { self.hold }, supports_elicitation);
        let budget = self.max_wait.unwrap_or_else(|| profile.budget_for(hold));
        tracing::debug!(client = %Self::client_name(ctx), profile = profile.name, ?hold, ?budget, "planned wait");
        (hold, budget)
    }

    async fn heartbeat(&self, ctx: &RequestContext<RoleServer>, progress: f64, message: String) {
        let Some(token) = ctx.meta.get_progress_token() else {
            return;
        };
        let mut param = ProgressNotificationParam::new(token, progress);
        param.message = Some(message);
        if let Err(error) = ctx.peer.notify_progress(param).await {
            tracing::debug!(%error, "progress notification failed");
        }
    }

    /// Wait for the briefing with progress heartbeats; honours client cancellation.
    async fn wait_with_progress(
        &self,
        id: &str,
        url: &str,
        ctx: &RequestContext<RoleServer>,
        max_wait: Duration,
        heartbeat: bool,
    ) -> Result<WaitOutcome, ErrorData> {
        let started = tokio::time::Instant::now();
        let deadline = started + max_wait;
        let mut tick: u64 = 0;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(WaitOutcome::Pending);
            }
            let slice = if heartbeat { remaining.min(HEARTBEAT) } else { remaining };
            tokio::select! {
                outcome = self.backend.wait(id, slice) => {
                    match outcome.map_err(internal)? {
                        WaitOutcome::Pending => {
                            tick += 1;
                            if heartbeat {
                                let elapsed = started.elapsed().as_secs();
                                self.heartbeat(ctx, tick as f64, format!("Waiting for the briefing ({elapsed}s): {url}")).await;
                            }
                        }
                        done => return Ok(done),
                    }
                }
                _ = ctx.ct.cancelled() => return Err(cancelled_by_client(id)),
            }
        }
    }

    /// Hold the tool call open with an elicitation so Codex pauses its tool timeout.
    async fn wait_with_elicitation(
        &self,
        id: &str,
        url: &str,
        ctx: &RequestContext<RoleServer>,
        max_wait: Duration,
    ) -> Result<WaitOutcome, ErrorData> {
        let schema = ElicitationSchema::builder()
            .required_property(
                "submitted",
                PrimitiveSchemaDefinition::Boolean(
                    BooleanSchema::new().title("I have submitted the briefing in the browser").with_default(true),
                ),
            )
            .build()
            .map_err(internal)?;
        let message = format!(
            "Briefing is open at {url}\n\nReview it in the browser, press Submit there, then accept this prompt. Decline to cancel the briefing."
        );
        let params = ElicitRequestParams::FormElicitationParams { meta: None, message, requested_schema: schema };
        let mut handle = ctx
            .peer
            .send_cancellable_request(
                ServerRequest::ElicitRequest(ElicitRequest::new(params)),
                PeerRequestOptions::no_options(),
            )
            .await
            .map_err(internal)?;

        tokio::select! {
            response = &mut handle.rx => {
                match response {
                    Ok(Ok(ClientResult::ElicitResult(result))) => match result.action {
                        ElicitationAction::Accept => {
                            // The user says they submitted; give the submission a moment to land,
                            // then fall back to a plain wait if it has not.
                            match self.backend.wait(id, Duration::from_secs(5)).await.map_err(internal)? {
                                WaitOutcome::Pending => self.wait_with_progress(id, url, ctx, max_wait, true).await,
                                done => Ok(done),
                            }
                        }
                        _ => {
                            let _ = self.backend.cancel(id).await;
                            Ok(WaitOutcome::Done(BriefingResponse::cancelled()))
                        }
                    },
                    other => {
                        tracing::debug!(?other, "elicitation did not complete; falling back to progress wait");
                        self.wait_with_progress(id, url, ctx, max_wait, true).await
                    }
                }
            }
            outcome = self.backend.wait(id, max_wait) => {
                if let Err(error) = handle.cancel(Some("briefing finished".into())).await {
                    tracing::debug!(%error, "could not cancel elicitation");
                }
                outcome.map_err(internal)
            }
            _ = ctx.ct.cancelled() => {
                let _ = handle.cancel(Some("await cancelled".into())).await;
                Err(cancelled_by_client(id))
            }
        }
    }

    async fn wait_for(
        &self,
        id: &str,
        url: &str,
        ctx: &RequestContext<RoleServer>,
        hold: HoldMode,
        max_wait: Duration,
    ) -> Result<WaitOutcome, ErrorData> {
        match hold {
            HoldMode::Elicitation => self.wait_with_elicitation(id, url, ctx, max_wait).await,
            HoldMode::Progress | HoldMode::Auto => self.wait_with_progress(id, url, ctx, max_wait, true).await,
            HoldMode::None => self.wait_with_progress(id, url, ctx, max_wait, false).await,
        }
    }

    fn outcome_result(id: &str, url: &str, outcome: WaitOutcome) -> CallToolResult {
        match outcome {
            WaitOutcome::Pending => structured(
                format!("Briefing {id} still open at {url}"),
                &AwaitOutput {
                    status: "pending".into(),
                    briefing_id: id.into(),
                    url: Some(url.into()),
                    feedback: None,
                    instructions: format!(
                        "The user has not submitted yet. Call await_briefing again with briefingId \"{id}\" to keep waiting (remind the user of the link {url} if they seem stuck), or cancel_briefing to stop."
                    ),
                },
            ),
            WaitOutcome::Done(response) => {
                let status = response.status();
                let instructions = if response.cancelled {
                    "The user cancelled the briefing without submitting. Ask how they would like to proceed; do not reopen it unasked."
                } else {
                    "Respond only to this feedback: answer checkpoint answers, act on decisions, address each comment (location + quoted passage + comment), follow up on sections flagged revisit. Do not repeat the presentation."
                };
                let mut result = structured(
                    format!("Briefing {id} {status}: {}", response.counts()),
                    &AwaitOutput {
                        status: status.to_string(),
                        briefing_id: id.into(),
                        url: None,
                        feedback: Some(response),
                        instructions: instructions.into(),
                    },
                );
                result.is_error = Some(status == BriefingStatus::Cancelled);
                result
            }
        }
    }

    fn require_structured_content(ctx: &RequestContext<RoleServer>) -> Result<(), ErrorData> {
        let version = ctx.peer.peer_info().map(|info| info.protocol_version.to_string()).unwrap_or_default();
        if !version.is_empty() && version.as_str() < MIN_PROTOCOL {
            return Err(ErrorData::internal_error(
                format!(
                    "briefing needs MCP {MIN_PROTOCOL} or newer for structuredContent; this client negotiated {version}"
                ),
                None,
            ));
        }
        Ok(())
    }
}

#[tool_router]
impl BriefingMcp {
    /// Open a paced browser briefing for the user. Returns the link and a briefingId immediately; put the link in your reply, then call await_briefing to collect their notes, comments, decisions, and follow-up markers.
    #[tool(name = "brief_user", output_schema = output_schema::<OpenOutput>())]
    async fn brief_user(
        &self,
        Parameters(input): Parameters<Briefing>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_structured_content(&ctx)?;
        let created: Created = self
            .backend
            .create(input, Some(Self::source(&ctx)))
            .await
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let opened = if created.opened_browser {
            "A browser was opened on the server's machine, but still show the user the link."
        } else {
            "No browser was opened; the user must open the link themselves (they may be on another machine)."
        };
        Ok(structured(
            format!("Briefing {} open at {}", created.id, created.url),
            &OpenOutput {
                status: "open".into(),
                instructions: format!(
                    "Put this exact link in your reply so the user can open it: {url}. {opened} Then call await_briefing with briefingId \"{id}\"; it blocks until the user submits and returns their feedback. If the call is moved to the background, wait for its completion notification instead of polling. The briefing survives this session; await_briefing with the same briefingId recovers it later.",
                    url = created.url,
                    id = created.id,
                ),
                briefing_id: created.id,
                url: created.url,
                opened_browser: created.opened_browser,
                scope: created.scope,
            },
        ))
    }

    /// Wait for the user to submit a briefing opened by brief_user (this session or an earlier one). Blocks until they submit; may return "pending" (call again) or "reopened" (relay the fresh link, then call again). Do not poll if the harness backgrounds it.
    #[tool(name = "await_briefing", output_schema = output_schema::<AwaitOutput>())]
    async fn await_briefing(
        &self,
        Parameters(input): Parameters<AwaitParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = input.briefing_id;
        let info = self
            .backend
            .info(&id)
            .await
            .map_err(internal)?
            .ok_or_else(|| ErrorData::invalid_params(format!("unknown briefingId {id}"), None))?;
        let url = info.url.clone().unwrap_or_else(|| format!("(briefing {})", info.title));
        if info.reopened {
            return Ok(structured(
                format!("Briefing {id} reopened at {url}"),
                &AwaitOutput {
                    status: "reopened".into(),
                    briefing_id: id.clone(),
                    url: Some(url.clone()),
                    feedback: None,
                    instructions: format!(
                        "This briefing was created by an earlier process and is now served again at {url}; earlier links are dead. Put this exact link in your reply (the user's draft is preserved), then call await_briefing with briefingId \"{id}\" to wait for their feedback."
                    ),
                },
            ));
        }
        let (hold, budget) = self.plan(&ctx);
        let max_wait = input.wait_seconds.map(Duration::from_secs).unwrap_or(budget).min(budget);
        let outcome = self.wait_for(&id, &url, &ctx, hold, max_wait).await?;
        Ok(Self::outcome_result(&id, &url, outcome))
    }

    /// Cancel an open briefing.
    #[tool(name = "cancel_briefing", output_schema = output_schema::<CancelOutput>())]
    async fn cancel_briefing(&self, Parameters(input): Parameters<CancelParams>) -> Result<CallToolResult, ErrorData> {
        let cancelled = self.backend.cancel(&input.briefing_id).await.map_err(internal)?;
        Ok(structured(
            format!("Briefing {} {}.", input.briefing_id, if cancelled { "cancelled" } else { "was already finished" }),
            &CancelOutput { briefing_id: input.briefing_id, cancelled },
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BriefingMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
            .with_server_info(Implementation::new("briefing", env!("CARGO_PKG_VERSION")).with_title("Briefing"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_profiles() {
        assert_eq!(ClientProfile::for_client("codex-mcp-client").hold, HoldMode::Elicitation);
        assert_eq!(ClientProfile::for_client("codex").budget_for(HoldMode::Elicitation), HOURS_4);
        assert_eq!(ClientProfile::for_client("codex").budget_for(HoldMode::Progress), Duration::from_secs(280));
        assert_eq!(ClientProfile::for_client("claude-code").budget, HOURS_24);
        assert_eq!(ClientProfile::for_client("Cursor").budget, Duration::from_secs(50));
        assert_eq!(ClientProfile::for_client("pi").name, "pi");
        assert_eq!(ClientProfile::for_client("copilot").name, "vscode");
        assert_eq!(ClientProfile::for_client("mcp-inspector").name, "unknown");
        assert_eq!(resolve_hold(HoldMode::Elicitation, false), HoldMode::Progress);
        assert_eq!(resolve_hold(HoldMode::Auto, true), HoldMode::Progress);
    }

    #[test]
    fn pending_and_done_results() {
        let pending = BriefingMcp::outcome_result("r1", "http://x", WaitOutcome::Pending);
        assert_eq!(pending.structured_content.as_ref().unwrap()["status"], "pending");
        assert!(output_schema::<AwaitOutput>()["properties"].get("feedback").is_some());
        let done = BriefingMcp::outcome_result("r1", "http://x", WaitOutcome::Done(BriefingResponse::default()));
        assert_eq!(done.structured_content.as_ref().unwrap()["status"], "completed");
        assert_eq!(done.is_error, Some(false));
        let cancelled = BriefingMcp::outcome_result("r1", "http://x", WaitOutcome::Done(BriefingResponse::cancelled()));
        assert_eq!(cancelled.is_error, Some(true));
    }
}

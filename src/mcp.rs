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
    ServerCapabilities, ServerConfig, ServerRequest,
};
use rmcp::service::{PeerRequestOptions, RequestContext};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Created};
use crate::bind::Scope;
use crate::content::{Briefing, schema_value};
use crate::guidance::show_link;
use crate::hub::{HarnessSession, Origin};
use crate::response::Outcome;

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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AwaitParams {
    /// The briefingId returned by brief_user.
    pub briefing_id: String,
    /// Maximum seconds to wait before returning pending again (server may cap it).
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelParams {
    /// The briefingId returned by brief_user.
    pub briefing_id: String,
}

/// `brief_user` output: the briefing is open and waiting.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenOutput {
    /// Always "active": the same word `briefing status` and the hub API use for an open briefing.
    pub status: String,
    pub briefing_id: String,
    /// Link the user must open. Show it verbatim.
    pub url: String,
    /// Whether this MCP server process opened a browser on its own machine.
    pub opened_browser: bool,
    /// How far the link reaches; `explicit` implies no network trust.
    pub scope: Scope,
    /// What the model should do next.
    pub instructions: String,
}

/// `await_briefing` output.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AwaitOutput {
    pub briefing_id: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    /// The link, while the briefing is still open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
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
    /// The harness session this server runs under; `Some` only when the harness started it
    /// (stdio), so it is on the client's machine.
    harness_session: Option<HarnessSession>,
    tool_router: ToolRouter<Self>,
}

/// The harness behind an MCP client: the name its CLI side uses for the harnesses we know,
/// else the client's own name. Not the timeout profile, which groups unrelated clients.
fn harness_name(client: &str) -> Option<String> {
    let known = match client.to_ascii_lowercase().as_str() {
        "claude-code" => Some("claude-code"),
        "codex" | "codex-mcp-client" => Some("codex"),
        "pi" => Some("pi"),
        _ => None,
    };
    let name = known.map(str::to_string).unwrap_or_else(|| client.trim().to_string());
    (!name.is_empty()).then_some(name)
}

/// What a known MCP client can tolerate, derived from `clientInfo.name` and the advertised
/// capabilities in `initialize` (no model involvement). `PROFILES` below is the source of
/// truth for per-client behaviour; the README summarises it.
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

/// First match wins; the last entry is the fallback. Budgets sit just under each client's
/// tool-call timeout (research as of 2026-09; the MCP spec only says clients MAY reset their
/// timer on `notifications/progress` and SHOULD enforce a maximum).
static PROFILES: &[ClientProfile] = &[
    // 300 s wall clock (openai/codex#28234; docs still say 60), `tool_timeout_sec` to raise.
    // Progress does not reset it, but the timer pauses while an elicitation is outstanding
    // (openai/codex#17566), hence the form hold.
    profile("codex", &["codex"], HoldMode::Elicitation, Duration::from_secs(280)),
    // ~28 h wall clock (`MCP_TOOL_TIMEOUT`) plus a 30 min stdio idle timer that progress
    // resets; calls over 2 min are moved to a background task and the model is notified.
    profile("claude-code", &["claude"], HoldMode::Progress, HOURS_24),
    // 600 s `timeout`, no progress reset, no elicitation (google-gemini/gemini-cli#22249).
    profile("gemini-cli", &["gemini"], HoldMode::Progress, Duration::from_secs(570)),
    // 300 s extension `timeout`; its own elicitation dialog times out at 5 min.
    profile("goose", &["goose"], HoldMode::Progress, Duration::from_secs(280)),
    // No client-side timeout (microsoft/vscode-copilot-release#14130).
    profile("vscode", &["vscode", "visual studio", "copilot"], HoldMode::Progress, HOURS_24),
    // 60 s SDK default, no progress reset (Cursor staff on forum.cursor.com/t/160548), often
    // not configurable (Cursor) or only per server (Cline, Zed, Continue, OpenCode).
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
        Self { backend, hold, max_wait, harness_session: None, tool_router: Self::tool_router() }
    }

    /// Mark this server as one its harness started (stdio), so it runs on the client's machine:
    /// briefings get this machine's hostname in their source, and `session` when a call does not
    /// carry one (Claude Code gives it `CLAUDE_CODE_SESSION_ID` but sends nothing per call).
    pub fn started_by_harness(mut self, session: HarnessSession) -> Self {
        self.harness_session = Some(session);
        self
    }

    /// Who is creating a briefing. The session is the user's explicit `BRIEFING_SESSION`, else
    /// the call's `_meta` (Codex sends `threadId`), else this server's harness session; the
    /// harness is the MCP client unless the user named one.
    fn origin(&self, ctx: &RequestContext<RoleServer>) -> Origin {
        let client = Self::client_name(ctx);
        let env = self.harness_session.clone().unwrap_or_default();
        let thread = ctx.meta.get("threadId").and_then(|value| value.as_str()).map(str::to_string);
        let (session, named) = if env.explicit { (env.id, env.harness) } else { (thread.or(env.id), None) };
        let harness = named.or_else(|| harness_name(&client));
        Origin { source: Some(self.source(client)), harness, session }
    }

    /// `<client>@<host>` from a server on the client's machine; just `<client>` from a hub's
    /// `/mcp`, which cannot know where the client runs. Shown on the dashboard and in `status`.
    fn source(&self, client: String) -> String {
        let client = if client.trim().is_empty() { "mcp".to_string() } else { client };
        if self.harness_session.is_some() { format!("{client}@{}", crate::backend::hostname()) } else { client }
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
    ) -> Result<Outcome, ErrorData> {
        let started = tokio::time::Instant::now();
        let deadline = started + max_wait;
        let mut tick: u64 = 0;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(Outcome::Pending);
            }
            let slice = if heartbeat { remaining.min(HEARTBEAT) } else { remaining };
            tokio::select! {
                outcome = self.backend.wait(id, slice) => {
                    match outcome.map_err(internal)? {
                        Outcome::Pending => {
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
    ) -> Result<Outcome, ErrorData> {
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
                                Outcome::Pending => self.wait_with_progress(id, url, ctx, max_wait, true).await,
                                done => Ok(done),
                            }
                        }
                        _ => {
                            let _ = self.backend.cancel(id).await;
                            Ok(Outcome::cancelled())
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
    ) -> Result<Outcome, ErrorData> {
        match hold {
            HoldMode::Elicitation => self.wait_with_elicitation(id, url, ctx, max_wait).await,
            HoldMode::Progress | HoldMode::Auto => self.wait_with_progress(id, url, ctx, max_wait, true).await,
            HoldMode::None => self.wait_with_progress(id, url, ctx, max_wait, false).await,
        }
    }

    /// Every outcome is a successful call, cancelled included (as with the CLI's exit 0); the
    /// `status` says which it was.
    fn outcome_result(id: &str, url: &str, outcome: Outcome) -> CallToolResult {
        let (text, url, instructions) = match &outcome {
            Outcome::Pending => (
                format!("Briefing {id} still open at {url}"),
                Some(url.to_string()),
                format!(
                    "The user has not submitted yet. Call await_briefing again with briefingId \"{id}\" to keep waiting (remind the user of the link {url} if they seem stuck), or cancel_briefing to stop."
                ),
            ),
            Outcome::Completed { feedback } => (
                format!("Briefing {id} completed: {}", feedback.counts()),
                None,
                "Respond only to this feedback: act on question answers, treat unresolved questions as still open (not approval), address each comment (location + quoted passage + comment) and each note. Do not repeat the presentation.".to_string(),
            ),
            Outcome::Cancelled { feedback } => (
                format!("Briefing {id} cancelled: {}", feedback.counts()),
                None,
                "The user cancelled the briefing without submitting. Ask how they would like to proceed; do not reopen it unasked.".to_string(),
            ),
        };
        structured(text, &AwaitOutput { briefing_id: id.into(), outcome, url, instructions })
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
    /// Open a paced browser briefing for the user. Returns the link and a briefingId immediately; put the link in your reply, then call await_briefing to collect their notes, comments, and question answers.
    #[tool(name = "brief_user", output_schema = output_schema::<OpenOutput>())]
    async fn brief_user(
        &self,
        Parameters(input): Parameters<Briefing>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_structured_content(&ctx)?;
        let created: Created = self
            .backend
            .create(input, self.origin(&ctx))
            .await
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let opened = if created.opened_browser {
            "A browser was opened on this MCP server's machine, but still show the user the link."
        } else {
            "No browser was opened; the user must open the link themselves (they may be on another machine)."
        };
        Ok(structured(
            format!("{} (briefing {})", show_link(&created.url), created.id),
            &OpenOutput {
                status: "active".into(),
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

    /// Wait for the user to submit a briefing opened by brief_user (this session or an earlier one). Blocks until they submit; may return "pending" (call again). Do not poll if the harness backgrounds it.
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
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(crate::guidance::mcp_guidance())
            .with_server_info(Implementation::new("briefing", env!("BRIEFING_VERSION")).with_title("Briefing"))
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
        let pending = BriefingMcp::outcome_result("r1", "http://x", Outcome::Pending);
        let content = pending.structured_content.as_ref().unwrap();
        assert_eq!(content["status"], "pending");
        assert_eq!(content["url"], "http://x");
        assert!(content.get("feedback").is_none());
        let done = BriefingMcp::outcome_result("r1", "http://x", Outcome::Completed { feedback: Default::default() });
        let content = done.structured_content.as_ref().unwrap();
        assert_eq!(content["status"], "completed");
        assert_eq!(content["feedback"]["annotations"], serde_json::json!([]));
        assert!(content.get("url").is_none());
        assert_eq!(done.is_error, Some(false));
        let cancelled = BriefingMcp::outcome_result("r1", "http://x", Outcome::cancelled());
        assert_eq!(cancelled.structured_content.as_ref().unwrap()["status"], "cancelled");
        assert_eq!(cancelled.is_error, Some(false), "a cancellation is an outcome, not a failed call");
    }

    /// The output schema names every status the tool can return.
    #[test]
    fn await_output_schema_lists_statuses() {
        let schema = serde_json::to_string(&*output_schema::<AwaitOutput>()).unwrap();
        for status in ["pending", "completed", "cancelled"] {
            assert!(schema.contains(&format!("\"{status}\"")), "{status} missing from {schema}");
        }
        assert!(schema.contains("briefingId"));
        assert!(schema.contains("feedback"));
    }
}

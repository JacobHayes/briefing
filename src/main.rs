use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use briefing::backend::{Backend, BindMode, Created, LocalBackend, RemoteBackend};
use briefing::content::{self, Briefing};
use briefing::http::{self, AppState, HttpConfig};
use briefing::hub::{BriefingInfo, BriefingStatus, Hub, HubConfig, Provenance, WaitOutcome};
use briefing::mcp::{BriefingMcp, HoldMode};
use briefing::store::Store;
use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;

const EXIT_CANCELLED: i32 = 2;
const EXIT_PENDING: i32 = 3;
const EXIT_INTERRUPTED: i32 = 130;

/// Paced browser briefings for coding agents (Pi, Claude Code, Codex, ...).
#[derive(Parser)]
#[command(name = "briefing", version, about)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Common {
    /// Use a remote hub instead of an embedded server.
    #[arg(long, env = "BRIEFING_HUB", global = true)]
    hub: Option<String>,
    /// Address to bind the embedded server to.
    #[arg(long, env = "BRIEFING_BIND", global = true, value_enum, default_value_t = BindMode::Auto)]
    bind: BindMode,
    /// Shell command run when a briefing is created (gets BRIEFING_URL, BRIEFING_ID,
    /// BRIEFING_TITLE); use it to push the link to your phone from a headless box.
    #[arg(long, env = "BRIEFING_ON_CREATE", global = true)]
    on_create: Option<String>,
    /// Never try to open the system browser.
    #[arg(
        long,
        env = "BRIEFING_NO_OPEN",
        global = true,
        action = clap::ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    no_open: bool,
}

#[derive(Args, Clone)]
struct HoldArgs {
    /// How to keep long tool calls alive. `auto` picks per MCP client from its
    /// initialize handshake (elicitation for Codex, progress otherwise).
    #[arg(long, env = "BRIEFING_HOLD", value_enum, default_value_t = HoldMode::Auto)]
    hold: HoldMode,
    /// Longest a single brief_user/await_briefing call blocks before returning pending.
    /// Default is chosen per MCP client (50 s for 60-second clients such as Cursor, 280 s
    /// Codex without elicitation, hours for Claude Code and VS Code).
    #[arg(long, env = "BRIEFING_MAX_WAIT_SECS")]
    max_wait_secs: Option<u64>,
}

/// How to wait for and report a briefing's result.
#[derive(Args, Clone)]
struct WaitArgs {
    /// Emit JSON events on stderr and the JSON result on stdout.
    #[arg(long)]
    json: bool,
    /// Return after this many seconds even if the briefing is still open.
    #[arg(long)]
    wait_seconds: Option<u64>,
}

#[derive(Args)]
struct ServeArgs {
    /// Port to listen on.
    #[arg(long, env = "BRIEFING_PORT", default_value_t = 7789)]
    port: u16,
    /// Origin to put in briefing URLs when behind a reverse proxy (e.g. https://briefings.example).
    #[arg(long, env = "BRIEFING_PUBLIC_ORIGIN")]
    public_origin: Option<String>,
    /// How long finished briefings stay fetchable (e.g. 6h, 90m, 2d).
    #[arg(long, env = "BRIEFING_FINISHED_TTL", default_value = HubConfig::FINISHED_TTL_TEXT, value_parser = parse_duration)]
    finished_ttl: Duration,
    /// How long unanswered briefings stay open.
    #[arg(long, env = "BRIEFING_ACTIVE_TTL", default_value = HubConfig::ACTIVE_TTL_TEXT, value_parser = parse_duration)]
    active_ttl: Duration,
    /// Also serve MCP (streamable HTTP) at /mcp.
    #[arg(long)]
    mcp: bool,
    /// Try to open new briefings in this machine's browser.
    #[arg(long)]
    open: bool,
    #[command(flatten)]
    hold: HoldArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Present a JSON presentation (file or stdin), wait, and print the result.
    Present {
        /// Path to the presentation JSON ("-" or omitted = stdin).
        file: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Open the bundled demo presentation.
    Demo {
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Run the MCP server over stdio.
    Mcp {
        #[command(flatten)]
        hold: HoldArgs,
    },
    /// Run a long-lived hub: browser pages, an agent API, and optionally MCP over HTTP.
    Serve(ServeArgs),
    /// Wait for a briefing created earlier, in this or another process, and print its result.
    /// Prints a fresh link first when the briefing is still open.
    Await {
        briefing_id: String,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Cancel an open briefing.
    Cancel { briefing_id: String },
    /// Show one briefing's status, or list every known briefing.
    Status {
        briefing_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the brief_user JSON Schema.
    Schema,
    /// Print the model-facing guidelines as JSON (shared rules plus the MCP instructions).
    Guidelines,
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("BRIEFING_LOG").unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

/// `90s`, `15m`, `6h`, `2d` (bare numbers are seconds).
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let n: u64 = digits.parse().map_err(|_| format!("invalid duration {text:?}"))?;
    let secs = match unit.trim() {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        other => return Err(format!("unknown duration unit {other:?} (use s, m, h, d)")),
    };
    Ok(Duration::from_secs(secs))
}

fn backend(common: &Common) -> anyhow::Result<Backend> {
    match &common.hub {
        Some(hub) => Ok(Backend::Remote(RemoteBackend::new(hub)?)),
        None => Ok(Backend::Local(LocalBackend::new(
            common.bind,
            !common.no_open,
            common.on_create.clone(),
            HubConfig::with_default_store(),
        ))),
    }
}

fn cli_source() -> String {
    format!("cli@{}", briefing::backend::hostname())
}

fn read_presentation(path: Option<&str>) -> anyhow::Result<Briefing> {
    let mut text = String::new();
    match path {
        Some(path) if path != "-" => text = std::fs::read_to_string(path)?,
        _ => {
            if std::io::stdin().is_terminal() {
                anyhow::bail!("no presentation given: pass a file path or pipe JSON on stdin");
            }
            std::io::stdin().read_to_string(&mut text)?;
        }
    }
    Ok(serde_json::from_str(&text)?)
}

struct Reporter {
    json: bool,
}

impl Reporter {
    fn event(&self, value: serde_json::Value, human: String) {
        let mut err = std::io::stderr().lock();
        if self.json {
            let _ = writeln!(err, "{value}");
        } else {
            let _ = writeln!(err, "{human}");
        }
    }

    fn ready(&self, created: &Created) {
        let mut lines = vec![
            format!("Open briefing ({}): {}", created.scope, created.url),
            format!("Briefing id {} (recover later with `briefing await {}`)", created.id, created.id),
        ];
        if let Some(host) = &created.bind_host {
            lines.push(format!("Listening on {} ({host})", created.label));
        }
        if !created.opened_browser {
            lines.push("Browser not opened automatically; open the URL manually".into());
        }
        if let Some(diag) = &created.diagnostics {
            lines.push(diag.clone());
        }
        let mut value = serde_json::to_value(created).unwrap_or_default();
        value["event"] = json!("ready");
        self.event(value, lines.join("\n"));
    }
}

async fn wait_and_print(backend: &Backend, id: &str, args: &WaitArgs) -> anyhow::Result<i32> {
    let reporter = Reporter { json: args.json };
    let timeout = args.wait_seconds.map(Duration::from_secs).unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
    let outcome = tokio::select! {
        outcome = backend.wait(id, timeout) => outcome?,
        _ = shutdown_signal() => {
            reporter.event(json!({"event": "interrupted", "id": id}), "Interrupted; cancelling the briefing".into());
            let _ = backend.cancel(id).await;
            return Ok(EXIT_INTERRUPTED);
        }
    };
    let mut out = std::io::stdout().lock();
    match outcome {
        WaitOutcome::Pending => {
            reporter.event(json!({"event": "pending", "id": id}), format!("Briefing {id} is still open"));
            if reporter.json {
                writeln!(out, "{}", json!({"status": "pending", "briefingId": id}))?;
            }
            Ok(EXIT_PENDING)
        }
        WaitOutcome::Done(result) => {
            let status = result.status();
            reporter.event(json!({"event": status, "id": id}), format!("Briefing {status}"));
            if reporter.json {
                writeln!(out, "{}", json!({"status": status, "briefingId": id, "result": result}))?;
            } else {
                writeln!(out, "{}", result.format_text())?;
            }
            Ok(if result.cancelled { EXIT_CANCELLED } else { 0 })
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn present(common: &Common, presentation: Briefing, args: &WaitArgs) -> anyhow::Result<i32> {
    let backend = backend(common)?;
    let created = backend.create(presentation, Some(cli_source())).await?;
    Reporter { json: args.json }.ready(&created);
    let code = wait_and_print(&backend, &created.id, args).await?;
    backend.shutdown().await;
    Ok(code)
}

async fn run_mcp_stdio(common: &Common, hold: HoldArgs) -> anyhow::Result<()> {
    let backend = Arc::new(backend(common)?);
    let handler = BriefingMcp::new(backend, hold.hold, hold.max_wait_secs.map(Duration::from_secs));
    let service = handler.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn serve(common: &Common, args: ServeArgs) -> anyhow::Result<()> {
    let target = common.bind.target().await;
    let listener = http::bind(&target.bind_host, args.port).await?;
    let bound_port = listener.local_addr()?.port();
    let origin = args
        .public_origin
        .map(|o| o.trim_end_matches('/').to_string())
        .unwrap_or_else(|| http::origin_for(&target.public_host, bound_port));
    let mut allowed_hosts = vec![target.public_host.clone()];
    if let Some(host) = url::Url::parse(&origin).ok().as_ref().and_then(http::host_with_port) {
        allowed_hosts.push(host);
    }
    let hub = Arc::new(Hub::new(HubConfig {
        finished_ttl: args.finished_ttl,
        active_ttl: args.active_ttl,
        ..HubConfig::with_default_store()
    }));
    let state = AppState {
        hub,
        config: Arc::new(HttpConfig {
            public_origin: origin.clone(),
            allowed_hosts: allowed_hosts.clone(),
            agent_api: true,
            on_create: common.on_create.clone(),
        }),
    };

    let mcp_router = if args.mcp {
        let backend = Arc::new(Backend::Local(LocalBackend::attached(state.clone(), &target, args.open)));
        let hold = args.hold.hold;
        let max_wait = args.hold.max_wait_secs.map(Duration::from_secs);
        let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
        let service = StreamableHttpService::new(
            move || Ok(BriefingMcp::new(backend.clone(), hold, max_wait)),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        Some(axum::Router::new().nest_service("/mcp", service))
    } else {
        None
    };

    let running = http::serve_listener(http::router(state, mcp_router), listener)?;
    eprintln!("briefing hub listening on {} ({})", running.local_addr, target.label);
    eprintln!("briefing URLs use origin {origin}");
    eprintln!(
        "dashboard: {origin}/  agent API: {origin}/agent/briefings{}",
        if args.mcp { format!("  MCP: {origin}/mcp") } else { String::new() }
    );
    if let Some(dir) = Store::default_dir() {
        eprintln!("records: {}", dir.display());
    }
    if let Some(diag) = &target.diagnostics {
        eprintln!("{diag}");
    }
    shutdown_signal().await;
    eprintln!("shutting down");
    running.stop().await;
    Ok(())
}

fn print_status_table(infos: &[BriefingInfo]) {
    if infos.is_empty() {
        println!("no briefings");
        return;
    }
    let now = briefing::store::now_secs();
    for info in infos {
        let status = if info.status == BriefingStatus::Active { "waiting" } else { info.status.as_str() };
        let age = now.saturating_sub(info.created_at);
        let age =
            if age < 3600 { format!("{}m", age / 60) } else { format!("{}h{:02}m", age / 3600, (age % 3600) / 60) };
        let mut extras = Vec::new();
        if let Some(source) = &info.source {
            extras.push(source.clone());
        }
        if let Some(draft) = &info.draft {
            extras.push(format!("screen {}/{}, {} comments", draft.screen, draft.screens, draft.annotations));
        }
        if info.provenance == Provenance::DiskOnly && info.status == BriefingStatus::Active {
            extras.push(format!(
                "served by another process; `briefing await {}` re-serves it if that one is gone",
                info.id
            ));
        }
        println!("{:<9} {:<18} {:>7}  {}", status, info.id, age, info.title);
        if !extras.is_empty() {
            println!("{:<9} {}", "", extras.join(" · "));
        }
        if let Some(url) = info.url.as_ref().filter(|_| info.status == BriefingStatus::Active) {
            println!("{:<9} {url}", "");
        }
    }
}

#[tokio::main]
async fn main() {
    briefing::tls::init();
    init_tracing();
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Command::Present { file, wait } => {
            let presentation = read_presentation(file.as_deref())?;
            present(&cli.common, presentation, &wait).await
        }
        Command::Demo { wait } => present(&cli.common, content::demo(), &wait).await,
        Command::Mcp { hold } => {
            run_mcp_stdio(&cli.common, hold).await?;
            Ok(0)
        }
        Command::Serve(args) => {
            serve(&cli.common, args).await?;
            Ok(0)
        }
        Command::Await { briefing_id, wait } => {
            let backend = backend(&cli.common)?;
            let Some(info) = backend.info(&briefing_id).await? else {
                anyhow::bail!("briefing {briefing_id} not found (records expire a few hours after they finish)");
            };
            if info.status == BriefingStatus::Active {
                let url = info.url.clone().unwrap_or_default();
                let mut value = serde_json::to_value(&info)?;
                value["event"] = json!("ready");
                Reporter { json: wait.json }.event(value, format!("Open briefing: {url}"));
            }
            let code = wait_and_print(&backend, &briefing_id, &wait).await?;
            backend.shutdown().await;
            Ok(code)
        }
        Command::Cancel { briefing_id } => {
            let backend = backend(&cli.common)?;
            let cancelled = backend.cancel(&briefing_id).await?;
            println!("{}", json!({"briefingId": briefing_id, "cancelled": cancelled}));
            Ok(0)
        }
        Command::Status { briefing_id, json } => {
            let backend = backend(&cli.common)?;
            match briefing_id {
                Some(id) => match backend.info(&id).await? {
                    Some(info) if json => println!("{}", serde_json::to_string_pretty(&info)?),
                    Some(info) => print_status_table(&[info]),
                    None => anyhow::bail!("briefing {id} not found"),
                },
                None => {
                    let infos = backend.list().await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&infos)?);
                    } else {
                        print_status_table(&infos);
                    }
                }
            }
            Ok(0)
        }
        Command::Schema => {
            println!("{}", serde_json::to_string_pretty(&content::json_schema())?);
            Ok(0)
        }
        Command::Guidelines => {
            println!("{}", serde_json::to_string_pretty(&briefing::guidance::json())?);
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_flag_defaults_match_hub_defaults() {
        assert_eq!(parse_duration(HubConfig::FINISHED_TTL_TEXT).unwrap(), HubConfig::FINISHED_TTL);
        assert_eq!(parse_duration(HubConfig::ACTIVE_TTL_TEXT).unwrap(), HubConfig::ACTIVE_TTL);
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("5w").is_err());
    }
}

use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use briefing::backend::{Backend, BindMode, Created, LocalBackend, RemoteBackend};
use briefing::content::{self, Briefing};
use briefing::http::{self, AppState, HttpConfig, OnCreateHook};
use briefing::hub::{Hub, HubConfig, WaitOutcome};
use briefing::mcp::{BriefingMcp, HoldMode};
use briefing::tailscale::{self, BindTarget};
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
    /// Bearer token for the hub.
    #[arg(long, env = "BRIEFING_HUB_TOKEN", global = true, hide_env_values = true)]
    hub_token: Option<String>,
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

#[derive(Subcommand)]
enum Command {
    /// Present a JSON presentation (file or stdin), wait, and print the result.
    Present {
        /// Path to the presentation JSON ("-" or omitted = stdin).
        file: Option<String>,
        /// Emit JSON events on stderr and the JSON result on stdout.
        #[arg(long)]
        json: bool,
        /// Return after this many seconds even if the briefing is still open.
        #[arg(long)]
        wait_seconds: Option<u64>,
    },
    /// Open the bundled demo presentation.
    Demo {
        #[arg(long)]
        json: bool,
        /// Return after this many seconds even if the briefing is still open.
        #[arg(long)]
        wait_seconds: Option<u64>,
    },
    /// Run the MCP server over stdio.
    Mcp {
        #[command(flatten)]
        hold: HoldArgs,
    },
    /// Run a long-lived hub: browser pages, an agent API, and optionally MCP over HTTP.
    Serve {
        /// Port to listen on.
        #[arg(long, env = "BRIEFING_PORT", default_value_t = 7789)]
        port: u16,
        /// Origin to put in briefing URLs when behind a reverse proxy (e.g. https://briefings.example).
        #[arg(long, env = "BRIEFING_PUBLIC_ORIGIN")]
        public_origin: Option<String>,
        /// Bearer token for /agent and /mcp.
        #[arg(long, env = "BRIEFING_HUB_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Also serve MCP (streamable HTTP) at /mcp.
        #[arg(long)]
        mcp: bool,
        /// Try to open new briefings in this machine's browser.
        #[arg(long)]
        open: bool,
        #[command(flatten)]
        hold: HoldArgs,
    },
    /// Wait for a briefing created earlier (hub mode) and print its result.
    Await {
        briefing_id: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        wait_seconds: Option<u64>,
    },
    /// Cancel an open briefing (hub mode).
    Cancel { briefing_id: String },
    /// Show a briefing's status (hub mode).
    Status { briefing_id: String },
    /// Print the brief_user JSON Schema.
    Schema,
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("BRIEFING_LOG").unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

fn backend(common: &Common) -> anyhow::Result<Backend> {
    match &common.hub {
        Some(hub) => Ok(Backend::Remote(RemoteBackend::new(hub, common.hub_token.as_deref().unwrap_or(""))?)),
        None => Ok(Backend::Local(LocalBackend::new(
            common.bind,
            !common.no_open,
            common.on_create.clone().map(OnCreateHook),
        ))),
    }
}

fn require_hub(backend: &Backend, verb: &str) -> anyhow::Result<()> {
    if matches!(backend, Backend::Local(_)) {
        anyhow::bail!(
            "{verb} needs a hub (--hub / BRIEFING_HUB); embedded briefings live only inside the process that created them"
        );
    }
    Ok(())
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
        let mut lines = vec![format!("Open briefing ({}): {}", created.scope, created.url)];
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

async fn wait_and_print(
    backend: &Backend,
    id: &str,
    wait_seconds: Option<u64>,
    reporter: &Reporter,
) -> anyhow::Result<i32> {
    let timeout = wait_seconds.map(Duration::from_secs).unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
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
            let status = if result.cancelled { "cancelled" } else { "completed" };
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

async fn present(
    common: &Common,
    presentation: Briefing,
    json: bool,
    wait_seconds: Option<u64>,
) -> anyhow::Result<i32> {
    content::validate(&presentation)?;
    let reporter = Reporter { json };
    let backend = backend(common)?;
    let created = backend.create(presentation).await?;
    reporter.ready(&created);
    let code = wait_and_print(&backend, &created.id, wait_seconds, &reporter).await?;
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

struct ServeArgs {
    port: u16,
    public_origin: Option<String>,
    token: Option<String>,
    mcp: bool,
    open: bool,
    hold: HoldArgs,
}

async fn serve(common: &Common, args: ServeArgs) -> anyhow::Result<()> {
    let token = args
        .token
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("--token (or BRIEFING_HUB_TOKEN) is required for the hub"))?;
    let target: BindTarget = match common.bind {
        BindMode::Local => BindTarget::local(None),
        BindMode::Auto | BindMode::Tailscale => tailscale::detect_bind_target().await,
    };
    let listener = http::bind(&target.bind_host, args.port).await?;
    let bound_port = listener.local_addr()?.port();
    let origin = args
        .public_origin
        .map(|o| o.trim_end_matches('/').to_string())
        .unwrap_or_else(|| http::origin_for(&target.public_host, bound_port));
    let mut allowed_hosts = vec![target.public_host.clone()];
    if let Ok(url) = url::Url::parse(&origin)
        && let Some(host) = url.host_str()
    {
        allowed_hosts.push(match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        });
    }
    let hub = Arc::new(Hub::new(HubConfig::default()));
    let state = AppState {
        hub,
        config: Arc::new(HttpConfig {
            public_origin: origin.clone(),
            allowed_hosts: allowed_hosts.clone(),
            agent_token: Some(token),
            on_create: common.on_create.clone().map(OnCreateHook),
        }),
    };

    let mcp_router = if args.mcp {
        let backend = Arc::new(Backend::Local(LocalBackend::attached(
            state.clone(),
            target.scope,
            target.label.clone(),
            target.bind_host.clone(),
            args.open,
        )));
        let hold = args.hold.hold;
        let max_wait = args.hold.max_wait_secs.map(Duration::from_secs);
        let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts.clone());
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
        "agent API: {origin}/agent/briefings (bearer token){}",
        if args.mcp { format!("; MCP: {origin}/mcp") } else { String::new() }
    );
    if let Some(diag) = &target.diagnostics {
        eprintln!("{diag}");
    }
    shutdown_signal().await;
    eprintln!("shutting down");
    running.stop().await;
    Ok(())
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
        Command::Present { file, json, wait_seconds } => {
            let presentation = read_presentation(file.as_deref())?;
            present(&cli.common, presentation, json, wait_seconds).await
        }
        Command::Demo { json, wait_seconds } => present(&cli.common, content::demo(), json, wait_seconds).await,
        Command::Mcp { hold } => {
            run_mcp_stdio(&cli.common, hold).await?;
            Ok(0)
        }
        Command::Serve { port, public_origin, token, mcp, open, hold } => {
            serve(&cli.common, ServeArgs { port, public_origin, token, mcp, open, hold }).await?;
            Ok(0)
        }
        Command::Await { briefing_id, json, wait_seconds } => {
            let backend = backend(&cli.common)?;
            require_hub(&backend, "await")?;
            wait_and_print(&backend, &briefing_id, wait_seconds, &Reporter { json }).await
        }
        Command::Cancel { briefing_id } => {
            let backend = backend(&cli.common)?;
            require_hub(&backend, "cancel")?;
            let cancelled = backend.cancel(&briefing_id).await?;
            println!("{}", json!({"briefingId": briefing_id, "cancelled": cancelled}));
            Ok(0)
        }
        Command::Status { briefing_id } => {
            let backend = backend(&cli.common)?;
            require_hub(&backend, "status")?;
            match backend.info(&briefing_id).await? {
                Some(info) => println!("{}", serde_json::to_string_pretty(&info)?),
                None => anyhow::bail!("briefing {briefing_id} not found"),
            }
            Ok(0)
        }
        Command::Schema => {
            println!("{}", serde_json::to_string_pretty(&content::json_schema())?);
            Ok(0)
        }
    }
}

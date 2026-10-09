use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use briefing::backend::{Backend, BackendKind, Created, RemoteBackend, Site};
use briefing::bind::{self, BindMode};
use briefing::content::{self, Briefing};
use briefing::guidance::show_link;
use briefing::hub::{BriefingInfo, BriefingStatus, HarnessSession, Hub, HubConfig, Origin};
use briefing::local_hub::{self, HubFile, HubLock, LocalHub};
use briefing::mcp::{BriefingMcp, HoldMode};
use briefing::response::{BriefingOutcome, Outcome};
use briefing::store::Store;
use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use serde_json::json;

const EXIT_INTERRUPTED: i32 = 130;
/// How often an idle-exiting hub checks whether anything is open.
const IDLE_CHECK: Duration = Duration::from_secs(1);

/// Paced browser briefings for coding agents (Pi, Claude Code, Codex, ...).
#[derive(Parser)]
#[command(name = "briefing", version = env!("BRIEFING_VERSION"), about)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Common {
    /// Use this hub instead of this machine's own (which starts on demand).
    #[arg(long, env = "BRIEFING_HUB", global = true)]
    hub: Option<String>,
    /// Port this machine's hub listens on: `serve`, and the hub a client starts on demand.
    #[arg(long, env = "BRIEFING_PORT", global = true)]
    port: Option<u16>,
    // Help text is built from `bind::ACCEPTED` so it cannot drift from the parser.
    #[arg(
        long,
        env = "BRIEFING_BIND",
        global = true,
        value_name = "MODE_OR_IP",
        help = format!("Where to listen: {}. Explicit addresses never fall back", bind::ACCEPTED)
    )]
    bind: Option<BindMode>,
    /// Open new briefings in this machine's system browser (`--open false` to suppress). Left
    /// unset, the config file then a built-in `true` decide. When using `--hub`, the client opens
    /// the hub URL locally; `serve` ignores it because the hub itself stays headless.
    #[arg(long, global = true, env = "BRIEFING_OPEN")]
    open: Option<bool>,
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

/// How `present` reports the briefing it created.
#[derive(Args, Clone)]
struct PresentArgs {
    /// Print the result as JSON.
    #[arg(long)]
    json: bool,
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
    /// Exit once nothing has been open this long (e.g. 60s). Clients pass it to the hub they
    /// start on demand; a hub you run yourself stays up.
    #[arg(long, value_parser = parse_duration)]
    idle_exit: Option<Duration>,
    /// Started by a client for this machine: a newer client may replace it.
    #[arg(long, hide = true)]
    on_demand: bool,
    #[command(flatten)]
    hold: HoldArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Create a briefing from a JSON presentation (file or stdin) and print its link. Collect
    /// the feedback with `await`.
    Present {
        /// Path to the presentation JSON ("-" or omitted = stdin).
        file: Option<String>,
        #[command(flatten)]
        args: PresentArgs,
    },
    /// Open the bundled demo presentation.
    Demo {
        #[command(flatten)]
        args: PresentArgs,
    },
    /// Run the MCP server over stdio.
    Mcp {
        #[command(flatten)]
        hold: HoldArgs,
    },
    /// Run a long-lived hub: browser pages, an agent API, and optionally MCP over HTTP.
    Serve(ServeArgs),
    /// Wait for a briefing's feedback and print it. Prints its link first while it is open.
    Await {
        briefing_id: String,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Cancel an open briefing.
    Cancel { briefing_id: String },
    /// Show one briefing's status, or list this agent session's briefings (this machine's when
    /// the session is unknown).
    Status {
        briefing_id: Option<String>,
        #[arg(long)]
        json: bool,
        /// List every known briefing, from any session or machine.
        #[arg(long)]
        all: bool,
    },
    /// Print the presentation JSON Schema.
    Schema,
    /// Print model-facing guidance for an integration surface.
    Guidance {
        #[command(subcommand)]
        target: GuidanceTarget,
    },
}

#[derive(Subcommand)]
enum GuidanceTarget {
    /// Print agent-facing CLI workflow guidance.
    Cli,
    /// Print Pi extension guidance as JSON.
    Pi,
    /// Print MCP server instructions text.
    Mcp,
    /// Print the CLI-focused Agent Skill markdown.
    Skill,
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

/// Per-machine defaults from `config.toml`, each sitting below the matching environment variable
/// and CLI argument. Every field is optional so an unset key leaves the higher layers untouched.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    bind: Option<BindMode>,
    hub: Option<String>,
    open: Option<bool>,
}

/// The per-machine settings file, or defaults when there is none. The path is `BRIEFING_CONFIG`
/// when set - explicit, so a missing file is an error - else `$XDG_CONFIG_HOME/briefing/config.toml`
/// or `~/.config/briefing/config.toml`, where a missing file just means no settings. Environment
/// variables and CLI arguments are higher-priority layers.
fn load_file_config() -> anyhow::Result<Settings> {
    let (path, required) = match std::env::var_os("BRIEFING_CONFIG") {
        Some(path) if path.is_empty() => anyhow::bail!("BRIEFING_CONFIG is empty"),
        Some(path) => (PathBuf::from(path), true),
        None => match Store::xdg_base("XDG_CONFIG_HOME", ".config") {
            Some(base) => (base.join("briefing/config.toml"), false),
            None => return Ok(Settings::default()),
        },
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(Settings::default());
        }
        Err(error) => anyhow::bail!("could not read {}: {error}", path.display()),
    };
    toml::from_str(&text).map_err(|error| anyhow::anyhow!("invalid {}: {error}", path.display()))
}

impl Settings {
    /// Fill in each `common` field that neither an argument nor an environment variable supplied
    /// (clap has already resolved argument over environment into these `Option`s).
    fn overlay(self, common: &mut Common) {
        common.bind = common.bind.or(self.bind);
        common.hub = common.hub.take().or(self.hub);
        common.open = common.open.or(self.open);
    }
}

impl Common {
    fn bind_mode(&self) -> BindMode {
        self.bind.unwrap_or_default()
    }

    /// Whether creating a briefing should open it on this machine: `--open`/`BRIEFING_OPEN`, else
    /// the config file, else a built-in `true`.
    fn open_browser(&self) -> bool {
        self.open.unwrap_or(true)
    }

    fn port(&self) -> u16 {
        self.port.unwrap_or(local_hub::DEFAULT_PORT)
    }
}

/// The client side of a CLI or stdio-MCP run: the configured hub, else this machine's own, plus
/// this machine's `open` preference.
fn backend(common: &Common) -> anyhow::Result<Backend> {
    let kind = match &common.hub {
        Some(hub) => BackendKind::Remote(RemoteBackend::new(hub)?),
        // Only settings this client was given: a hub it starts otherwise reuses the last one's.
        None => BackendKind::Local(LocalHub::new(state_dir()?, common.bind, common.port)),
    };
    Ok(Backend::new(kind, common.open_browser()))
}

/// Where this machine's hub keeps its records and advertises itself.
fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    Store::default_dir()
        .ok_or_else(|| anyhow::anyhow!("no state directory for this machine's hub; set BRIEFING_STATE_DIR"))
}

fn cli_source() -> String {
    format!("cli@{}", briefing::backend::hostname())
}

fn cli_origin() -> Origin {
    let session = HarnessSession::from_env();
    Origin { source: Some(cli_source()), harness: session.harness, session: session.id }
}

/// Whether `status` lists a briefing by default: same agent session when this one is known -
/// the same harness too when that is known, since ids are only unique per harness - otherwise
/// created on this machine.
fn in_scope(origin: &Origin, session: &HarnessSession, host: &str) -> bool {
    match &session.id {
        Some(id) => {
            origin.session.as_deref() == Some(id.as_str())
                && session.harness.as_ref().is_none_or(|harness| origin.harness.as_ref() == Some(harness))
        }
        None => origin.source.as_deref().and_then(|source| source.rsplit_once('@')).is_some_and(|(_, h)| h == host),
    }
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
}

/// What `present` prints: the link to relay, and how to collect the feedback.
fn print_created(created: &Created, json: bool) -> anyhow::Result<()> {
    let id = &created.id;
    if json {
        let mut value = serde_json::to_value(created)?;
        value["status"] = json!("active");
        value["instructions"] = json!(format!(
            "{}. Then run `briefing await {id} --json` (in the background if your harness supports it) to collect \
             their feedback.",
            show_link(&created.url)
        ));
        println!("{value}");
    } else {
        println!("{}", show_link(&created.url));
        println!("Collect the feedback with `briefing await {id}`");
    }
    Ok(())
}

async fn wait_and_print(backend: &Backend, id: &str, args: &WaitArgs) -> anyhow::Result<i32> {
    let reporter = Reporter { json: args.json };
    let timeout = args.wait_seconds.map(Duration::from_secs).unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
    let outcome = tokio::select! {
        outcome = backend.wait(id, timeout) => outcome?,
        // The briefing lives in the hub, not this process: stop waiting, leave it open.
        _ = shutdown_signal() => {
            let human = format!("Stopped waiting; the briefing stays open (`briefing await {id}` resumes)");
            reporter.event(json!({"event": "interrupted", "briefingId": id}), human);
            return Ok(EXIT_INTERRUPTED);
        }
    };
    // Every outcome the briefing can reach exits 0; which one it was lives in the result's
    // `status`, so agents don't mistake "still open" or "cancelled" for a failed command.
    let event = match &outcome {
        Outcome::Pending => "pending",
        Outcome::Completed { .. } => "completed",
        Outcome::Cancelled { .. } => "cancelled",
    };
    let human =
        if outcome == Outcome::Pending { format!("Briefing {id} is still open") } else { format!("Briefing {event}") };
    reporter.event(json!({"event": event, "briefingId": id}), human);
    let mut out = std::io::stdout().lock();
    if reporter.json {
        let result = BriefingOutcome { briefing_id: id.to_string(), outcome };
        writeln!(out, "{}", serde_json::to_string(&result)?)?;
    } else if outcome != Outcome::Pending {
        writeln!(out, "{}", outcome.format_text())?;
    }
    Ok(0)
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

async fn present(common: &Common, presentation: Briefing, args: &PresentArgs) -> anyhow::Result<i32> {
    let created = backend(common)?.create(presentation, cli_origin()).await?;
    print_created(&created, args.json)?;
    Ok(0)
}

async fn run_mcp_stdio(common: &Common, hold: HoldArgs) -> anyhow::Result<()> {
    let backend = Arc::new(backend(common)?);
    let handler = BriefingMcp::new(backend, hold.hold, hold.max_wait_secs.map(Duration::from_secs))
        .started_by_harness(HarnessSession::from_env());
    let service = handler.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// MCP over streamable HTTP at `/mcp`, backed by the site this process serves.
fn mcp_router(site: &Arc<Site>, hold: &HoldArgs) -> axum::Router<Arc<Site>> {
    // The hub is headless: briefings created over its `/mcp` never open a browser here; the
    // agent hands the link to the user instead.
    let backend = Arc::new(Backend::new(BackendKind::Site(site.clone()), false));
    let (hold, max_wait) = (hold.hold, hold.max_wait_secs.map(Duration::from_secs));
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(site.config.allowed_hosts());
    let service = StreamableHttpService::new(
        move || Ok(BriefingMcp::new(backend.clone(), hold, max_wait)),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    axum::Router::new().nest_service("/mcp", service)
}

async fn serve(common: &Common, args: ServeArgs) -> anyhow::Result<()> {
    // A hub owns its state dir: without one there is nothing to own, so no memory-only fallback.
    let dir = state_dir()?;
    let store = Store::open(&dir).map_err(|error| anyhow::anyhow!("state directory {}: {error}", dir.display()))?;
    // Held until this function returns, so no other hub loads, sweeps, or writes these records.
    let Some(_lock) = HubLock::acquire(&dir).await? else {
        let owner = HubFile::read(&dir).map(|file| format!(" at {} (pid {})", file.origin, file.pid));
        anyhow::bail!("another hub already owns {}{}", dir.display(), owner.unwrap_or_default());
    };
    let hub = Arc::new(Hub::new(HubConfig {
        finished_ttl: args.finished_ttl,
        active_ttl: args.active_ttl,
        store: Some(store),
    }));
    let bind = common.bind_mode();
    let preferred = bind.target().await?;
    let fallback = bind.fallback(&preferred);
    let start = |target| {
        Site::start(hub.clone(), target, common.port(), args.public_origin.clone(), |site| {
            args.mcp.then(|| mcp_router(site, &args.hold))
        })
    };
    let (site, running) = match (start(preferred).await, fallback) {
        (Ok(started), _) => started,
        (Err(error), Some(fallback)) => {
            tracing::warn!(%error, "falling back to loopback");
            start(fallback).await?
        }
        (Err(error), None) => return Err(error),
    };
    let origin = &site.config.public_origin;
    eprintln!("briefing hub listening on {} ({})", running.local_addr, site.target.label);
    eprintln!("briefing URLs use origin {origin}");
    eprintln!(
        "dashboard: {origin}/  agent API: {origin}/agent/briefings{}",
        if args.mcp { format!("  MCP: {origin}/mcp") } else { String::new() }
    );
    if let Some(diag) = &site.target.diagnostics {
        eprintln!("{diag}");
    }
    eprintln!("records: {}", dir.display());
    // Advertised only once listening; it stays after exit so the next hub reuses these settings.
    let identity = local_hub::identity();
    let port = running.local_addr.port();
    let file = HubFile {
        origin: briefing::http::origin_for(site.target.host, port),
        pid: std::process::id(),
        version: env!("BRIEFING_VERSION").into(),
        on_demand: args.on_demand,
        bind: bind.to_string(),
        port,
        instance: identity.instance.clone(),
        control: identity.control.clone(),
    };
    if let Err(error) = file.write(&dir) {
        running.stop().await;
        anyhow::bail!("could not advertise this hub in {}: {error}", dir.display());
    }
    tokio::select! {
        _ = shutdown_signal() => eprintln!("shutting down"),
        _ = idle(&site.hub, args.idle_exit) => eprintln!("nothing open; shutting down"),
        _ = local_hub::shutdown_requested() => eprintln!("replaced by a newer hub; shutting down"),
    }
    running.stop().await;
    Ok(())
}

/// Resolves once `hub` has had nothing open for `limit`; never without a limit.
async fn idle(hub: &Hub, limit: Option<Duration>) {
    let Some(limit) = limit else { return std::future::pending().await };
    let mut idle_since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(IDLE_CHECK.min(limit)).await;
        if hub.active_count() > 0 {
            idle_since = tokio::time::Instant::now();
        } else if idle_since.elapsed() >= limit {
            return;
        }
    }
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
        let age = match age {
            ..3600 => format!("{}m", age / 60),
            3600..86_400 => format!("{}h{:02}m", age / 3600, (age % 3600) / 60),
            _ => format!("{}d{:02}h", age / 86_400, (age % 86_400) / 3600),
        };
        let mut extras = Vec::new();
        let origin = &info.origin;
        if let Some(source) = &origin.source {
            extras.push(source.clone());
        }
        if let Some(session) = &origin.session {
            extras.push(format!("{} session {session}", origin.harness.as_deref().unwrap_or("agent")));
        }
        if let Some(draft) = &info.draft {
            let position = if draft.review {
                "on review".to_string()
            } else {
                format!("screen {}/{}", draft.screen, draft.screens)
            };
            extras.push(format!("{position}, {} comments", draft.annotations));
        }
        println!("{:<9} {:<22} {:>7}  {}", status, info.id, age, info.title);
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

async fn run(mut cli: Cli) -> anyhow::Result<i32> {
    // Only an explicit argument/environment `--open true` is worth reporting to a `serve` run; a
    // config-file value is this machine's default for client-created briefings, not a hub
    // instruction.
    let open_requested = cli.common.open == Some(true);
    // The config file sits below the argument/environment layers, filling only their holes.
    let settings = load_file_config()?;
    settings.overlay(&mut cli.common);

    match cli.command {
        Command::Present { file, args } => {
            let presentation = read_presentation(file.as_deref())?;
            present(&cli.common, presentation, &args).await
        }
        Command::Demo { args } => present(&cli.common, content::demo(), &args).await,
        Command::Mcp { hold } => {
            run_mcp_stdio(&cli.common, hold).await?;
            Ok(0)
        }
        Command::Serve(args) => {
            if open_requested {
                tracing::warn!("--open/BRIEFING_OPEN is ignored by `serve`: a hub never opens a browser itself");
            }
            serve(&cli.common, args).await?;
            Ok(0)
        }
        Command::Await { briefing_id, wait } => {
            let backend = backend(&cli.common)?;
            let Some(info) = backend.info(&briefing_id).await? else {
                anyhow::bail!("briefing {briefing_id} not found (it may have expired)");
            };
            if info.status == BriefingStatus::Active {
                let url = info.url.clone().unwrap_or_default();
                let mut value = serde_json::to_value(&info)?;
                value["event"] = json!("ready");
                value["instruction"] = json!(show_link(&url));
                Reporter { json: wait.json }.event(value, show_link(&url));
            }
            wait_and_print(&backend, &briefing_id, &wait).await
        }
        Command::Cancel { briefing_id } => {
            let backend = backend(&cli.common)?;
            let cancelled = backend.cancel(&briefing_id).await?;
            println!("{}", json!({"briefingId": briefing_id, "cancelled": cancelled}));
            Ok(0)
        }
        Command::Status { briefing_id, json, all } => {
            let backend = backend(&cli.common)?;
            match briefing_id {
                Some(id) => match backend.info(&id).await? {
                    Some(info) if json => println!("{}", serde_json::to_string_pretty(&info)?),
                    Some(info) => print_status_table(&[info]),
                    None => anyhow::bail!("briefing {id} not found"),
                },
                None => {
                    let mut infos = backend.list().await?;
                    if !all {
                        let session = HarnessSession::from_env();
                        let host = briefing::backend::hostname();
                        infos.retain(|info| in_scope(&info.origin, &session, host));
                    }
                    if json {
                        println!("{}", serde_json::to_string_pretty(&infos)?);
                    } else if infos.is_empty() && !all {
                        println!("no briefings from this session (`briefing status --all` lists every briefing)");
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
        Command::Guidance { target } => {
            match target {
                GuidanceTarget::Cli => print!("{}", briefing::guidance::cli_guidance()),
                GuidanceTarget::Pi => println!("{}", serde_json::to_string_pretty(&briefing::guidance::pi_guidance())?),
                GuidanceTarget::Mcp => println!("{}", briefing::guidance::mcp_guidance()),
                GuidanceTarget::Skill => print!("{}", briefing::guidance::skill_guidance()),
            }
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

    #[test]
    fn file_config_accepts_known_fields_and_rejects_unknown() {
        let config: Settings = toml::from_str(
            r#"
            bind = "local"
            hub = "https://hub.example"
            open = false
            "#,
        )
        .unwrap();
        assert_eq!(config.bind, Some(BindMode::Local));
        assert_eq!(config.hub.as_deref(), Some("https://hub.example"));
        assert_eq!(config.open, Some(false));

        assert!(toml::from_str::<Settings>(r#"binding = "local""#).is_err());
        for value in ["123", "false", "[]", "{}"] {
            assert!(toml::from_str::<Settings>(&format!("bind = {value}")).is_err());
        }
    }

    #[test]
    fn status_scopes_to_the_session_else_this_machine() {
        let briefing = |source: Option<&str>, session: Option<&str>| Origin {
            source: source.map(Into::into),
            harness: None,
            session: session.map(Into::into),
        };
        let session = |harness: Option<&str>, id: Option<&str>| HarnessSession {
            harness: harness.map(Into::into),
            id: id.map(Into::into),
            explicit: false,
        };
        let mine = Origin { harness: Some("codex".into()), ..briefing(Some("cli@laptop"), Some("s1")) };
        let other_session = briefing(Some("cli@laptop"), Some("s2"));
        let other_host = briefing(Some("claude-code@workspace"), None);
        assert!(in_scope(&mine, &session(Some("codex"), Some("s1")), "laptop"));
        assert!(!in_scope(&other_session, &session(Some("codex"), Some("s1")), "laptop"));
        // Ids are only unique per harness: the same id from another harness is someone else's.
        assert!(!in_scope(&mine, &session(Some("claude-code"), Some("s1")), "laptop"));
        // An unnamed session (a bare `BRIEFING_SESSION`) matches by id alone.
        assert!(in_scope(&mine, &session(None, Some("s1")), "laptop"));
        // Without a known session, anything created on this machine counts, from any client.
        let unknown = HarnessSession::default();
        assert!(in_scope(&other_session, &unknown, "laptop"));
        assert!(!in_scope(&other_host, &unknown, "laptop"));
        assert!(!in_scope(&briefing(None, None), &unknown, "laptop"));
    }

    /// A `Common` with everything unset, as clap leaves it before the file overlay.
    fn bare_common() -> Common {
        Common { hub: None, port: None, bind: None, open: None }
    }

    #[test]
    fn overlay_fills_only_unset_fields() {
        // A file value wins only where clap left the field unset; an argument/env value stands.
        // Every field is set on both layers so a forgotten `overlay` line cannot hide here.
        let mut common = Common { bind: Some(BindMode::Tailscale), open: Some(true), hub: None, port: None };
        Settings { bind: Some(BindMode::Local), hub: Some("https://hub.example".into()), open: Some(false) }
            .overlay(&mut common);
        assert_eq!(common.bind, Some(BindMode::Tailscale)); // set on CLI, file ignored
        assert!(common.open_browser()); // set on CLI, file ignored
        assert_eq!(common.hub.as_deref(), Some("https://hub.example")); // filled from file

        // With nothing on the CLI the file fills every hole.
        let mut common = bare_common();
        Settings { hub: Some("file".into()), open: Some(false), ..Settings::default() }.overlay(&mut common);
        assert_eq!(common.hub.as_deref(), Some("file"));
        assert!(!common.open_browser());

        // Absent everywhere, the fallbacks are the built-in defaults.
        assert_eq!(bare_common().bind_mode(), BindMode::Auto);
        assert!(bare_common().open_browser());
    }

    #[test]
    fn open_flag_takes_an_explicit_value() {
        let cli = Cli::try_parse_from(["briefing", "demo", "--open", "false"]).unwrap();
        assert!(!cli.common.open_browser());
        let cli = Cli::try_parse_from(["briefing", "demo", "--open", "true"]).unwrap();
        assert!(cli.common.open_browser());
    }
}

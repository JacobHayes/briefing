//! Shared subprocess setup for the integration tests.

use std::path::Path;
use std::process::Command;

/// The briefing binary with every `BRIEFING_*` variable dropped from the inherited environment,
/// so a developer's own settings or hub never reach a test subprocess. Callers add back
/// the variables they are exercising.
#[allow(dead_code)]
pub fn briefing_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_briefing"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BRIEFING_") {
            command.env_remove(key);
        }
    }
    command
}

/// An HTTP client that speaks the current protocol, as the page and `briefing` itself do.
#[allow(dead_code)]
pub fn client() -> reqwest::Client {
    reqwest::Client::builder().default_headers(protocol_headers()).build().unwrap()
}

#[allow(dead_code)]
pub fn blocking_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().default_headers(protocol_headers()).build().unwrap()
}

fn protocol_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(briefing::protocol::HEADER, briefing::protocol::PROTOCOL.to_string().parse().unwrap());
    headers
}

/// A complete submission for the demo that answers nothing, with these notes.
#[allow(dead_code)]
pub fn demo_submission(notes: &[&str]) -> serde_json::Value {
    let mut body = briefing::response::blank_submission(&briefing::content::demo());
    body["notes"] = serde_json::json!(notes);
    body
}

/// Stop the hub that owns `state_dir`, if one is running, the way a replacing client does: a
/// control request with its secret, then wait for it to release the state dir. Plain TCP, so it
/// works from sync and async tests alike.
pub fn stop_hub(state_dir: &Path) {
    use std::io::{Read, Write};
    let Some(file) = briefing::local_hub::HubFile::read(state_dir) else { return };
    let Ok(url) = url::Url::parse(&file.origin) else { return };
    let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let Some(addr) = url.socket_addrs(|| Some(80)).ok().and_then(|addrs| addrs.into_iter().next()) else { return };
    let Ok(mut stream) = std::net::TcpStream::connect(addr) else { return };
    let request = format!(
        "POST /control/shutdown HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        file.control
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return;
    }
    let _ = stream.read_to_end(&mut Vec::new());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if briefing::local_hub::HubLock::try_acquire(state_dir).is_ok_and(|lock| lock.is_some()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A private machine for one test: its own state dir (so its own on-demand hub), an empty
/// config home, and a port. The hub its clients start is stopped on drop.
#[allow(dead_code)]
pub struct Machine {
    pub state: tempfile::TempDir,
    config: tempfile::TempDir,
    pub port: u16,
}

#[allow(dead_code)]
impl Machine {
    /// Any free port: links change if the hub restarts.
    pub fn new() -> Self {
        Self::on_port(0)
    }

    /// A fixed port that is free now, so links survive a hub restart.
    pub fn with_fixed_port() -> Self {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        Self::on_port(port)
    }

    fn on_port(port: u16) -> Self {
        Self { state: tempfile::tempdir().unwrap(), config: tempfile::tempdir().unwrap(), port }
    }

    /// `briefing` running on this machine, its hub on loopback, never opening a browser.
    pub fn command(&self) -> Command {
        let mut command = briefing_command();
        command
            .env("BRIEFING_STATE_DIR", self.state.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("BRIEFING_PORT", self.port.to_string())
            .env("BRIEFING_BIND", "local")
            .env("BRIEFING_OPEN", "false");
        command
    }

    pub fn stop_hub(&self) {
        stop_hub(self.state.path());
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.stop_hub();
    }
}

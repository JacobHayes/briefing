//! Shared subprocess setup for the integration tests.

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

//! Bind values reaching the one parser through the real CLI, environment, and per-machine TOML,
//! and the listener the winning layer actually produces. The exhaustive value tables are unit
//! tests in `src/bind.rs`; these representatives only prove each layer is wired up.

use std::process::Output;

use serde_json::Value;

mod common;

/// The three surfaces a bind value can arrive on, as `(file, env, cli)` arguments to [`run`].
fn surfaces(bind: &str) -> [(Option<&str>, Option<&str>, Option<&str>); 3] {
    [(Some(bind), None, None), (None, Some(bind), None), (None, None, Some(bind))]
}

async fn run(file_bind: Option<&str>, env_bind: Option<&str>, cli_bind: Option<&str>, args: &[&str]) -> Output {
    // A private machine, any free port for its hub, and no bind but the layers under test.
    let machine = common::Machine::new();
    let config = machine.state.path().join("config.toml");
    std::fs::write(&config, file_bind.map(|bind| format!("bind = {bind:?}\n")).unwrap_or_default()).unwrap();
    let mut command = machine.command();
    command.env_remove("BRIEFING_BIND").env("BRIEFING_CONFIG", config);
    command.args(args).args(["--open", "false"]);
    if let Some(bind) = env_bind {
        command.env("BRIEFING_BIND", bind);
    }
    if let Some(bind) = cli_bind {
        command.args(["--bind", bind]);
    }
    // `machine` stops its hub once the output is in hand.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::from(command).kill_on_drop(true).output(),
    )
    .await
    .expect("briefing command timed out (unexpected listener fallback?)")
    .unwrap()
}

#[tokio::test]
async fn every_surface_accepts_a_named_mode_and_a_literal_address() {
    for bind in ["local", "192.0.2.10", "2001:db8::1"] {
        for (file, env, cli) in surfaces(bind) {
            let output = run(file, env, cli, &["schema"]).await;
            assert!(output.status.success(), "{file:?}/{env:?}/{cli:?}: {}", String::from_utf8_lossy(&output.stderr));
        }
    }
}

#[tokio::test]
async fn every_surface_rejects_an_invalid_value() {
    for bind in ["localhost", "127.0.0.1:7789"] {
        for (file, env, cli) in surfaces(bind) {
            let output = run(file, env, cli, &["schema"]).await;
            assert!(!output.status.success(), "accepted {file:?}/{env:?}/{cli:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("bind") && error.contains("literal IP"), "{error}");
        }
    }
    // Strict file validation still runs even when a higher layer would override it.
    assert!(!run(Some("localhost"), Some("local"), Some("local"), &["schema"]).await.status.success());
    assert!(!run(Some("123"), None, None, &["schema"]).await.status.success());
}

/// The briefing `demo --json` creates on the hub it starts for these settings.
async fn created(file: Option<&str>, env: Option<&str>, cli: Option<&str>) -> Value {
    let output = run(file, env, cli, &["demo", "--json"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).expect("created briefing JSON")
}

#[tokio::test]
async fn bind_precedence_selects_the_actual_listener_without_fallback() {
    // A lower-priority address is deliberately unavailable; success cannot be a fallback.
    for (file, env, cli) in [
        (Some("127.0.0.1"), None, None),
        (Some("192.0.2.1"), Some("127.0.0.1"), None),
        (Some("192.0.2.1"), Some("192.0.2.2"), Some("127.0.0.1")),
    ] {
        let briefing = created(file, env, cli).await;
        assert_eq!(briefing["scope"], "explicit");
        assert!(briefing["url"].as_str().unwrap().starts_with("http://127.0.0.1:"));
    }
    let briefing = created(Some("127.0.0.1"), Some("local"), None).await;
    assert_eq!(briefing["scope"], "local", "named mode overrides literal file setting");
    // clap validates the parent's environment before seeing global flags after a subcommand.
    let output = run(Some("local"), Some("not-an-ip"), Some("127.0.0.1"), &["schema"]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid bind value"));
}

#[tokio::test]
async fn bind_unavailable_address_fails_for_a_client_and_the_hub() {
    // A client reports why the hub it started could not listen.
    for args in [&["demo", "--json"][..], &["serve"][..]] {
        let output = run(Some("192.0.2.1"), None, None, args).await;
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("192.0.2.1") && stderr.contains("bind failed"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
}

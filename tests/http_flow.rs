//! End-to-end HTTP behaviour: embedded server, browser API, host/origin checks,
//! and the hub agent API with bearer auth.

use std::sync::Arc;
use std::time::Duration;

use briefing::backend::{BindMode, LocalBackend};
use briefing::content::demo;
use briefing::http::{self, AppState, HttpConfig};
use briefing::hub::{Hub, HubConfig, WaitOutcome};
use serde_json::{Value, json};

#[tokio::test]
async fn embedded_server_roundtrip() {
    briefing::tls::init();
    let backend = LocalBackend::new(BindMode::Local, false, None);
    let created = backend.create(demo()).await.unwrap();
    assert!(created.url.starts_with("http://127.0.0.1:"));
    assert_eq!(created.scope, "local");
    assert!(!created.opened_browser);

    let client = reqwest::Client::new();
    let origin = created.url.rsplit_once("/briefing/").unwrap().0.to_string();
    let token = created.url.rsplit('/').next().unwrap().to_string();

    // Page + assets + presentation JSON.
    let page = client.get(&created.url).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let csp = page.headers().get("content-security-policy").unwrap().to_str().unwrap().to_string();
    let html = page.text().await.unwrap();
    assert!(csp.contains("'nonce-"));
    assert!(html.contains("/briefing-assets/mermaid.min.js"));
    let nonce = csp.split("'nonce-").nth(1).unwrap().split('\'').next().unwrap();
    assert!(html.contains(&format!("nonce=\"{nonce}\"")));

    let asset = client.get(format!("{origin}/briefing-assets/purify.min.js")).send().await.unwrap();
    assert_eq!(asset.status(), 200);
    assert!(asset.headers().get("content-type").unwrap().to_str().unwrap().starts_with("application/javascript"));
    assert_eq!(client.get(format!("{origin}/briefing-assets/nope.js")).send().await.unwrap().status(), 404);

    let presentation: Value =
        client.get(format!("{origin}/api/{token}/presentation")).send().await.unwrap().json().await.unwrap();
    assert_eq!(presentation["status"], "active");
    assert_eq!(presentation["chunks"].as_array().unwrap().len(), 2);
    assert_eq!(client.get(format!("{origin}/api/bogus/presentation")).send().await.unwrap().status(), 404);
    assert_eq!(client.get(format!("{origin}/briefing/bogus")).send().await.unwrap().status(), 404);

    // Wrong Host header -> 403 everywhere. Wrong/missing Origin on POST -> 403.
    let bad_host = client.get(format!("{origin}/healthz")).header("host", "evil.example").send().await.unwrap();
    assert_eq!(bad_host.status(), 403);
    let no_origin = client.post(format!("{origin}/api/{token}/complete")).json(&json!({})).send().await.unwrap();
    assert_eq!(no_origin.status(), 403);
    let bad_origin = client
        .post(format!("{origin}/api/{token}/complete"))
        .header("origin", "http://attacker.example")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_origin.status(), 403);

    // Agent API is not exposed in embedded mode.
    assert_eq!(client.get(format!("{origin}/agent/briefings")).send().await.unwrap().status(), 404);

    // Wait in the background, then submit from the "browser".
    let hub = backend.hub().clone();
    let id = created.id.clone();
    let waiter = tokio::spawn(async move { hub.wait(&id, Duration::from_secs(5)).await.unwrap() });
    let ok = client
        .post(format!("{origin}/api/{token}/complete"))
        .header("origin", &origin)
        .json(&json!({
            "chunks": [{"title": "One idea at a time", "status": "revisit", "note": "more please", "checkpoint": ""}],
            "decisions": [{"question": "How should briefing be triggered by default?", "selected": "Always proactive", "note": ""}],
            "annotations": [{"location": "One idea at a time", "quote": "Use Next and Back", "comment": "nice"}],
            "overallNote": "ship it"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    match waiter.await.unwrap() {
        WaitOutcome::Done(result) => {
            assert!(!result.cancelled);
            assert_eq!(result.overall_note, "ship it");
            assert_eq!(result.annotations.len(), 1);
            let text = result.format_text();
            assert!(text.contains("Decision - How should briefing be triggered by default?: Always proactive"));
        }
        other => panic!("unexpected {other:?}"),
    }
    // Second submission conflicts; the page now reports completed.
    let again = client
        .post(format!("{origin}/api/{token}/complete"))
        .header("origin", &origin)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
    let presentation: Value =
        client.get(format!("{origin}/api/{token}/presentation")).send().await.unwrap().json().await.unwrap();
    assert_eq!(presentation["status"], "completed");

    backend.shutdown().await;
}

#[tokio::test]
async fn hub_agent_api_with_bearer() {
    briefing::tls::init();
    let listener = http::bind("127.0.0.1", 0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let state = AppState {
        hub: Arc::new(Hub::new(HubConfig::default())),
        config: Arc::new(HttpConfig {
            public_origin: origin.clone(),
            allowed_hosts: vec!["127.0.0.1".into()],
            agent_token: Some("s3cret".into()),
            on_create: None,
        }),
    };
    let running = http::serve_listener(http::router(state, None), listener).unwrap();
    let client = reqwest::Client::new();

    assert_eq!(client.get(format!("{origin}/agent/briefings")).send().await.unwrap().status(), 401);
    assert_eq!(
        client.get(format!("{origin}/agent/briefings")).bearer_auth("wrong").send().await.unwrap().status(),
        401
    );

    let bad = client
        .post(format!("{origin}/agent/briefings"))
        .bearer_auth("s3cret")
        .json(&json!({"presentation": {"title": " ", "goal": "g", "chunks": [{"title": "t", "mainPoint": "m"}]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    let created: Value = client
        .post(format!("{origin}/agent/briefings"))
        .bearer_auth("s3cret")
        .json(&json!({"presentation": demo()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let url = created["url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("{origin}/briefing/")));

    let info: Value = client
        .get(format!("{origin}/agent/briefings/{id}"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["status"], "active");
    assert_eq!(info["url"], url);

    let pending: Value = client
        .get(format!("{origin}/agent/briefings/{id}/wait?timeout_secs=0"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");

    // Browser cancels -> wait reports cancelled.
    let token = url.rsplit('/').next().unwrap();
    let cancel = client
        .post(format!("{origin}/api/{token}/cancel"))
        .header("origin", &origin)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    let done: Value = client
        .get(format!("{origin}/agent/briefings/{id}/wait"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["result"]["cancelled"], true);

    let listed: Value = client
        .get(format!("{origin}/agent/briefings"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["briefings"].as_array().unwrap().len(), 1);
    assert_eq!(
        client
            .post(format!("{origin}/agent/briefings/nope/cancel"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    running.stop().await;
}

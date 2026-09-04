//! End-to-end HTTP behaviour: embedded server, browser API, drafts, host/origin checks,
//! the hub agent API and dashboard, and recovery of a briefing from another process.

use std::sync::Arc;
use std::time::Duration;

use briefing::backend::{BindMode, LocalBackend};
use briefing::content::demo;
use briefing::http::{self, AppState, HttpConfig};
use briefing::hub::{Hub, HubConfig, WaitOutcome};
use briefing::store::Store;
use serde_json::{Value, json};

#[tokio::test]
async fn embedded_server_roundtrip() {
    briefing::tls::init();
    let backend = LocalBackend::new(BindMode::Local, false, None, HubConfig::default());
    let created = backend.create(demo(), Some("test".into())).await.unwrap();
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

    // Agent API and dashboard are not exposed in embedded mode.
    assert_eq!(client.get(format!("{origin}/agent/briefings")).send().await.unwrap().status(), 404);
    assert_eq!(client.get(format!("{origin}/")).send().await.unwrap().status(), 404);

    // Drafts: saved with a revision, stale saves return the newer draft, page payload carries it.
    let draft = |current: u64, note: &str| json!({"current": current, "state": {"chunks": {"0": {"note": note, "checkpoint": "", "status": ""}}, "decisions": {}, "annotations": [], "overallNote": ""}, "disclosures": {}, "updatedAt": 1});
    let put = |body: Value| client.put(format!("{origin}/api/{token}/draft")).header("origin", &origin).json(&body);
    assert_eq!(
        client.put(format!("{origin}/api/{token}/draft")).json(&json!({"draft": {}})).send().await.unwrap().status(),
        403
    );
    let saved: Value =
        put(json!({"baseRevision": 0, "draft": draft(1, "one")})).send().await.unwrap().json().await.unwrap();
    assert_eq!(saved["revision"], 1);
    let stale = put(json!({"baseRevision": 0, "draft": draft(0, "zero")})).send().await.unwrap();
    assert_eq!(stale.status(), 409);
    let stale: Value = stale.json().await.unwrap();
    assert_eq!(stale["revision"], 1);
    assert_eq!(stale["draft"]["state"]["chunks"]["0"]["note"], "one");
    let saved: Value = put(json!({"draft": draft(2, "two")})).send().await.unwrap().json().await.unwrap();
    assert_eq!(saved["revision"], 2);
    let presentation: Value =
        client.get(format!("{origin}/api/{token}/presentation")).send().await.unwrap().json().await.unwrap();
    assert_eq!(presentation["draftRevision"], 2);
    assert_eq!(presentation["draft"]["current"], 2);
    let info = backend.info(&created.id).await.unwrap().unwrap();
    assert_eq!(info.draft.unwrap().screen, 3);
    assert_eq!(info.source.as_deref(), Some("test"));

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
    assert_eq!(put(json!({"draft": draft(3, "late")})).send().await.unwrap().status(), 409);

    backend.shutdown().await;
}

/// A briefing created by one process is re-served by another with its draft intact, and a
/// submission made against the second process reaches a waiter in a third.
#[tokio::test]
async fn briefing_recovered_by_another_process() {
    briefing::tls::init();
    let dir = tempfile::tempdir().unwrap();
    let config = || HubConfig { store: Some(Store::open(dir.path()).unwrap()), ..HubConfig::default() };
    let client = reqwest::Client::new();

    let first = LocalBackend::new(BindMode::Local, false, None, config());
    let created = first.create(demo(), Some("first".into())).await.unwrap();
    let (origin1, token) = created.url.rsplit_once("/briefing/").unwrap();
    let draft = json!({"current": 1, "state": {"chunks": {}, "decisions": {}, "annotations": [], "overallNote": ""}, "updatedAt": 7});
    let saved = client
        .put(format!("{origin1}/api/{token}/draft"))
        .header("origin", origin1)
        .json(&json!({"draft": draft}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200);
    first.shutdown().await;

    // Nothing running: status still lists it from disk, without a live URL.
    let idle = LocalBackend::new(BindMode::Local, false, None, config());
    let listed = idle.list();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].on_disk_only);
    assert_eq!(listed[0].source.as_deref(), Some("first"));

    // Second process adopts it on `info`, starts serving it, and the page carries the draft.
    let second = LocalBackend::new(BindMode::Local, false, None, config());
    let info = second.info(&created.id).await.unwrap().unwrap();
    assert!(info.adopted);
    let url2 = info.url.clone().unwrap();
    assert_ne!(url2, created.url);
    assert!(url2.ends_with(&format!("/briefing/{token}")));
    let origin2 = url2.rsplit_once("/briefing/").unwrap().0.to_string();
    let page: Value =
        client.get(format!("{origin2}/api/{token}/presentation")).send().await.unwrap().json().await.unwrap();
    assert_eq!(page["draft"]["current"], 1);
    assert_eq!(page["draftRevision"], 1);

    // Third process waits; the browser submits to the second; the third sees it via disk.
    let third = LocalBackend::new(BindMode::Local, false, None, config());
    let waiter = {
        let hub = third.hub().clone();
        let id = created.id.clone();
        tokio::spawn(async move { hub.wait(&id, Duration::from_secs(10)).await.unwrap() })
    };
    let ok = client
        .post(format!("{origin2}/api/{token}/complete"))
        .header("origin", &origin2)
        .json(&json!({"overallNote": "recovered"}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    match waiter.await.unwrap() {
        WaitOutcome::Done(result) => assert_eq!(result.overall_note, "recovered"),
        other => panic!("unexpected {other:?}"),
    }
    // And a fourth, brand-new process gets the stored result immediately, no server needed.
    let fourth = LocalBackend::new(BindMode::Local, false, None, config());
    let info = fourth.info(&created.id).await.unwrap().unwrap();
    assert_eq!(info.status, briefing::hub::BriefingStatus::Completed);
    assert!(matches!(fourth.hub().wait(&created.id, Duration::from_millis(1)).await.unwrap(), WaitOutcome::Done(_)));
    second.shutdown().await;
}

#[tokio::test]
async fn hub_agent_api_and_dashboard() {
    briefing::tls::init();
    let listener = http::bind("127.0.0.1", 0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let state = AppState {
        hub: Arc::new(Hub::new(HubConfig::default())),
        config: Arc::new(HttpConfig {
            public_origin: origin.clone(),
            allowed_hosts: vec!["127.0.0.1".into()],
            agent_api: true,
            on_create: None,
        }),
    };
    let running = http::serve_listener(http::router(state, None), listener).unwrap();
    let client = reqwest::Client::new();

    let dashboard = client.get(format!("{origin}/")).send().await.unwrap();
    assert_eq!(dashboard.status(), 200);
    let csp = dashboard.headers().get("content-security-policy").unwrap().to_str().unwrap().to_string();
    let html = dashboard.text().await.unwrap();
    assert!(html.contains("Awaiting feedback"));
    let nonce = csp.split("'nonce-").nth(1).unwrap().split('\'').next().unwrap();
    assert!(html.contains(&format!("nonce=\"{nonce}\"")));

    let bad = client
        .post(format!("{origin}/agent/briefings"))
        .json(&json!({"presentation": {"title": " ", "goal": "g", "chunks": [{"title": "t", "mainPoint": "m"}]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    let created: Value = client
        .post(format!("{origin}/agent/briefings"))
        .json(&json!({"presentation": demo(), "source": "codex@laptop"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let url = created["url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("{origin}/briefing/")));

    let info: Value = client.get(format!("{origin}/agent/briefings/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["status"], "active");
    assert_eq!(info["url"], url);
    assert_eq!(info["source"], "codex@laptop");

    let pending: Value = client
        .get(format!("{origin}/agent/briefings/{id}/wait?timeout_secs=0"))
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
    let done: Value =
        client.get(format!("{origin}/agent/briefings/{id}/wait")).send().await.unwrap().json().await.unwrap();
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["result"]["cancelled"], true);

    let listed: Value = client.get(format!("{origin}/agent/briefings")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["briefings"].as_array().unwrap().len(), 1);
    assert_eq!(listed["briefings"][0]["url"], url);
    assert_eq!(listed["briefings"][0]["status"], "cancelled");
    assert_eq!(client.post(format!("{origin}/agent/briefings/nope/cancel")).send().await.unwrap().status(), 404);

    running.stop().await;
}

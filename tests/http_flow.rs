//! End-to-end HTTP behaviour: a client against a hub, the browser API, drafts, host/origin
//! checks, the agent API and dashboard, and a hub restart that keeps its briefings and links.

mod common;

use std::sync::Arc;
use std::time::Duration;

use briefing::backend::{Backend, BackendKind, RemoteBackend, Site};
use briefing::bind::{BindTarget, Scope};
use briefing::content::demo;
use briefing::http::RunningServer;
use briefing::hub::{Hub, HubConfig, Origin};
use briefing::response::Outcome;
use briefing::store::Store;
use serde_json::{Value, json};

/// A hub on loopback, on `port` (0 = any).
async fn start_hub(config: HubConfig, port: u16) -> (Arc<Site>, RunningServer) {
    Site::start(Arc::new(Hub::new(config)), BindTarget::local(None), port, None, |_| None).await.unwrap()
}

/// A client of the hub at `origin`, as the CLI and stdio MCP are.
fn client_of(origin: &str) -> Backend {
    Backend::new(BackendKind::Remote(RemoteBackend::new(origin).unwrap()), false)
}

#[tokio::test]
async fn client_and_browser_roundtrip() {
    briefing::tls::init();
    let (site, running) = start_hub(HubConfig::default(), 0).await;
    let backend = client_of(&site.config.public_origin);
    let created = backend.create(demo(), Origin::source("test")).await.unwrap();
    assert!(created.url.starts_with("http://127.0.0.1:"));
    assert_eq!(created.scope, Scope::Local);
    assert!(!created.opened_browser);
    assert_eq!(backend.info(&created.id).await.unwrap().unwrap().url.as_deref(), Some(created.url.as_str()));

    let client = common::client();
    let origin = created.url.rsplit_once("/briefing/").unwrap().0.to_string();
    let token = created.id.clone();

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

    // Drafts: saved with a revision, stale saves return the newer draft, page payload carries it.
    let draft = |current: u64, note: &str| json!({"current": current, "state": {"questions": {"c0-1": {"selected": [], "answer": note}}, "annotations": [], "notes": []}, "disclosures": {}, "updatedAt": 1});
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
    assert_eq!(stale["draft"]["state"]["questions"]["c0-1"]["answer"], "one");
    let saved: Value = put(json!({"draft": draft(2, "two")})).send().await.unwrap().json().await.unwrap();
    assert_eq!(saved["revision"], 2);
    let presentation: Value =
        client.get(format!("{origin}/api/{token}/presentation")).send().await.unwrap().json().await.unwrap();
    assert_eq!(presentation["draftRevision"], 2);
    assert_eq!(presentation["draft"]["current"], 2);
    let info = backend.info(&created.id).await.unwrap().unwrap();
    // The demo has two chunks, so current = 2 is the review screen.
    assert!(info.draft.unwrap().review);
    assert_eq!(info.source.as_deref(), Some("test"));

    // Wait in the background, then submit from the "browser".
    let id = created.id.clone();
    let waiter_origin = origin.clone();
    let waiter =
        tokio::spawn(async move { client_of(&waiter_origin).wait(&id, Duration::from_secs(5)).await.unwrap() });
    let ok = client
        .post(format!("{origin}/api/{token}/complete"))
        .header("origin", &origin)
        .json(&{
            let mut body = common::demo_submission(&["ship it"]);
            body["questions"][0]["selected"] = json!(["Paced, one chunk per screen"]);
            body["annotations"] =
                json!([{"location": "One idea at a time", "quote": "Use Next and Back", "comment": "nice"}]);
            body
        })
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let outcome = waiter.await.unwrap();
    match &outcome {
        Outcome::Completed { feedback } => {
            assert_eq!(feedback.notes, vec!["ship it"]);
            assert_eq!(feedback.annotations.len(), 1);
        }
        other => panic!("unexpected {other:?}"),
    }
    let text = outcome.format_text();
    assert!(text.contains(
        "Question (One idea at a time): Which reading mode should a briefing open in?\nSelected: Paced, one chunk per screen"
    ));
    assert!(text.contains("Question (whole briefing): How should briefing be triggered by default?\nUnresolved"));
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

    running.stop().await;
}

/// A hub restarted on the same port serves its briefings at the same links, drafts intact,
/// and a submission after the restart reaches a client that waited across it.
#[tokio::test]
async fn a_restarted_hub_keeps_briefings_and_links() {
    briefing::tls::init();
    let dir = tempfile::tempdir().unwrap();
    let config = || HubConfig { store: Some(Store::open(dir.path()).unwrap()), ..HubConfig::default() };
    let client = common::client();

    let (site, running) = start_hub(config(), 0).await;
    let port = running.local_addr.port();
    let origin = site.config.public_origin.clone();
    let created = client_of(&origin).create(demo(), Origin::source("first")).await.unwrap();
    let draft = json!({"current": 1, "state": {"questions": {}, "annotations": [], "notes": []}, "updatedAt": 7});
    let saved = client
        .put(format!("{origin}/api/{}/draft", created.id))
        .header("origin", &origin)
        .json(&json!({"draft": draft}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200);
    running.stop().await;

    // Nothing listening: the client sees an unreachable hub, not a missing briefing.
    let error = RemoteBackend::new(&origin).unwrap().info(&created.id).await.unwrap_err();
    assert!(error.is::<briefing::backend::HubUnreachable>(), "{error}");

    let (site, running) = start_hub(config(), port).await;
    let info = client_of(&origin).info(&created.id).await.unwrap().unwrap();
    assert_eq!(info.url.as_deref(), Some(created.url.as_str()), "same link after the restart");
    assert_eq!(info.source.as_deref(), Some("first"));
    let page: Value =
        client.get(format!("{origin}/api/{}/presentation", created.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(page["draft"]["current"], 1);
    assert_eq!(page["draftRevision"], 1);

    let waiter = {
        let (origin, id) = (origin.clone(), created.id.clone());
        tokio::spawn(async move { client_of(&origin).wait(&id, Duration::from_secs(10)).await.unwrap() })
    };
    let ok = client
        .post(format!("{origin}/api/{}/complete", created.id))
        .header("origin", &origin)
        .json(&common::demo_submission(&["after restart"]))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    match waiter.await.unwrap() {
        Outcome::Completed { feedback } => assert_eq!(feedback.notes, vec!["after restart"]),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(site.hub.active_count(), 0);
    running.stop().await;
}

/// The hub agent API creates briefings through the same path as the CLI and MCP.
#[tokio::test]
async fn hub_agent_api_and_dashboard() {
    briefing::tls::init();
    let (site, running) = start_hub(HubConfig::default(), 0).await;
    let origin = site.config.public_origin.clone();
    assert_eq!(origin, format!("http://127.0.0.1:{}", running.local_addr.port()));
    let client = common::client();

    let dashboard = client.get(format!("{origin}/")).send().await.unwrap();
    assert_eq!(dashboard.status(), 200);
    let csp = dashboard.headers().get("content-security-policy").unwrap().to_str().unwrap().to_string();
    let html = dashboard.text().await.unwrap();
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
    let id = created["briefingId"].as_str().unwrap().to_string();
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
    assert_eq!(pending, json!({"briefingId": id, "status": "pending"}));

    // Browser cancels -> wait reports cancelled.
    let cancel = client
        .post(format!("{origin}/api/{id}/cancel"))
        .header("origin", &origin)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    let done: Value =
        client.get(format!("{origin}/agent/briefings/{id}/wait")).send().await.unwrap().json().await.unwrap();
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["briefingId"], id);
    assert_eq!(done["feedback"]["notes"], json!([]));
    assert!(done.get("result").is_none());

    let listed: Value = client.get(format!("{origin}/agent/briefings")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["briefings"].as_array().unwrap().len(), 1);
    assert_eq!(listed["briefings"][0]["url"], url);
    assert_eq!(listed["briefings"][0]["status"], "cancelled");

    let second: Value = client
        .post(format!("{origin}/agent/briefings"))
        .json(&json!({"presentation": demo()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second_id = second["briefingId"].as_str().unwrap();
    assert_eq!(listed["briefings"][0]["briefingId"], id, "listings name the id like every other result");
    // Another site cannot create or cancel through a browser; the hub's own dashboard can.
    let cross_site = client
        .post(format!("{origin}/agent/briefings"))
        .header("origin", "http://127.0.0.1:1")
        .json(&json!({"presentation": demo()}))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), 403);
    let cross_cancel =
        client.post(format!("{origin}/agent/briefings/{second_id}/cancel")).header("origin", "http://attacker.example");
    assert_eq!(cross_cancel.send().await.unwrap().status(), 403);
    let agent_cancel: Value =
        client.post(format!("{origin}/agent/briefings/{second_id}/cancel")).send().await.unwrap().json().await.unwrap();
    assert_eq!(agent_cancel, json!({"ok": true, "cancelled": true}));
    let agent_cancel_again: Value =
        client.post(format!("{origin}/agent/briefings/{second_id}/cancel")).send().await.unwrap().json().await.unwrap();
    assert_eq!(agent_cancel_again, json!({"ok": true, "cancelled": false}));
    assert_eq!(client.post(format!("{origin}/agent/briefings/nope/cancel")).send().await.unwrap().status(), 404);

    running.stop().await;
}

/// Clients one protocol behind keep working: the hub answers them in their shapes and their
/// protocol. Anything older or newer is refused with both versions named, while pages, the
/// dashboard, and `/healthz` stay reachable without a protocol header. Unknown fields are
/// rejected, not dropped.
#[tokio::test]
async fn previous_protocol_clients_and_version_negotiation() {
    briefing::tls::init();
    let (site, running) = start_hub(HubConfig::default(), 0).await;
    let origin = site.config.public_origin.clone();
    let previous = (briefing::protocol::PROTOCOL - 1).to_string();
    let old = {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(briefing::protocol::HEADER, previous.parse().unwrap());
        reqwest::Client::builder().default_headers(headers).build().unwrap()
    };
    let current = common::client();

    // A protocol 2 client reads exactly `{id, url}` back, in protocol 2.
    let created = old
        .post(format!("{origin}/agent/briefings"))
        .json(&json!({ "presentation": demo(), "source": "old" }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    // Answered in the protocol it speaks, not the hub's.
    assert_eq!(created.headers()[briefing::protocol::HEADER], previous.as_str());
    let created: Value = created.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created, json!({ "id": id, "url": format!("{origin}/briefing/{id}") }));

    // Summaries call the id `id` for it, `briefingId` for current clients.
    let info: Value = old.get(format!("{origin}/agent/briefings/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["id"], id);
    assert!(info.get("briefingId").is_none());
    let listed: Value = old.get(format!("{origin}/agent/briefings")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["briefings"][0]["id"], id);
    let info: Value = current.get(format!("{origin}/agent/briefings/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["briefingId"], id);
    assert!(info.get("id").is_none());

    // Wait results have the same shape in both.
    let pending: Value = old
        .get(format!("{origin}/agent/briefings/{id}/wait?timeout_secs=0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending, json!({ "briefingId": id, "status": "pending" }));

    // Unversioned clients (protocol 1) are now two behind: refused, told to upgrade.
    let unversioned = reqwest::Client::new();
    let refused = unversioned.get(format!("{origin}/agent/briefings")).send().await.unwrap();
    assert_eq!(refused.status(), 426);
    let range = format!("{}-{}", briefing::protocol::OLDEST_SUPPORTED, briefing::protocol::PROTOCOL);
    assert!(refused.text().await.unwrap().contains(&range), "names the protocols the hub speaks");
    // Browsers and hub discovery send no header and are not part of the negotiation.
    for path in [format!("/briefing/{id}"), "/".to_string(), "/healthz".to_string()] {
        assert_eq!(unversioned.get(format!("{origin}{path}")).send().await.unwrap().status(), 200, "{path}");
    }

    // A protocol newer than the hub gets a 426 naming both versions, in the hub's protocol.
    let newer = current
        .get(format!("{origin}/agent/briefings"))
        .header(briefing::protocol::HEADER, (briefing::protocol::PROTOCOL + 1).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(newer.status(), 426);
    assert_eq!(newer.headers()[briefing::protocol::HEADER], briefing::protocol::PROTOCOL.to_string());
    assert!(newer.text().await.unwrap().contains("upgrade the hub"));

    // Current clients get strict parsing: an unknown field is an error, not silently dropped.
    let unknown = current
        .post(format!("{origin}/agent/briefings"))
        .json(&json!({ "presentation": { "title": "T", "goal": "g", "chunks": [{ "title": "c", "mainPoint": "m", "decision": {} }] } }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400);
    assert!(unknown.text().await.unwrap().contains("unknown field `decision`"));

    running.stop().await;
}

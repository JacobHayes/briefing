//! Explicit bind targets: actual sockets, URL authorities, and proxy Host/Origin handling.

use std::net::IpAddr;
use std::sync::Arc;

use briefing::backend::{Site, SiteOptions};
use briefing::bind::{BindMode, Scope};
use briefing::content::demo;
use briefing::hub::{Hub, HubConfig};
use serde_json::json;

fn bind_mode(ip: &str) -> BindMode {
    ip.parse().unwrap()
}

#[tokio::test]
async fn bind_target_literals_are_exact_and_not_assumed_private() {
    for ip in
        ["192.0.2.10", "100.64.0.1", "8.8.8.8", "127.0.0.1", "0.0.0.0", "::", "::1", "2001:0db8::1", "::ffff:192.0.2.1"]
    {
        let target = bind_mode(ip).target().await.unwrap();
        let canonical = ip.parse::<IpAddr>().unwrap();
        assert_eq!(target.host, canonical);
        assert_eq!(target.scope, Scope::Explicit);
        assert_eq!(target.label, format!("explicit IP {canonical}"));
        assert_eq!(target.diagnostics, None);
    }
}

async fn status(request: reqwest::RequestBuilder) -> u16 {
    request.send().await.unwrap().status().as_u16()
}

/// Serve on `ip` and exercise the briefing page, the Host guard, and a browser write, both
/// direct and behind a `--public-origin` proxy.
async fn roundtrip(ip: &str) {
    for public_origin in [None, Some("https://briefings.example")] {
        serve_and_check(ip, public_origin).await;
    }
}

async fn serve_and_check(ip: &str, public_origin: Option<&str>) {
    briefing::tls::init();
    let target = bind_mode(ip).target().await.unwrap();
    let options = SiteOptions { agent_api: true, public_origin: public_origin.map(str::to_string) };
    let (site, running) =
        Site::start(Arc::new(Hub::new(HubConfig::default())), target, 0, options, |_| None).await.unwrap();
    assert_eq!(running.local_addr.ip(), ip.parse::<IpAddr>().unwrap());
    let origin = format!("http://{}", running.local_addr);
    let canonical_origin = url::Url::parse(&origin).unwrap().origin().ascii_serialization();
    assert_eq!(site.config.public_origin, public_origin.unwrap_or(&canonical_origin));
    let created = site.create(demo(), None).await.unwrap();
    assert_eq!(created.url.rsplit_once("/briefing/").unwrap().0, site.config.public_origin);
    let token = created.url.rsplit('/').next().unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let health = || client.get(format!("{origin}/healthz"));
    assert_eq!(status(health()).await, 200);
    // Explicit Host headers preserve std's dotted-tail IPv6 spelling, unlike URL parsing.
    assert_eq!(status(health().header("Host", running.local_addr.to_string())).await, 200);
    assert_eq!(status(health().header("Host", "evil.example")).await, 403);
    assert_eq!(status(client.get(format!("{origin}/briefing/{token}"))).await, 200);
    let complete = || client.post(format!("{origin}/api/{token}/complete")).json(&json!({}));
    assert_eq!(status(complete().header("Origin", "http://evil.example")).await, 403);
    let mut submit = complete().header("Origin", &site.config.public_origin);
    if public_origin.is_some() {
        // The trusted proxy may preserve its public Host and the browser's Origin.
        submit = submit.header("Host", "briefings.example");
    }
    assert_eq!(status(submit).await, 200);
    running.stop().await;
}

#[tokio::test]
async fn bind_target_ipv4_listener_roundtrip() {
    roundtrip("127.0.0.1").await;
}

#[tokio::test]
async fn bind_target_ipv6_listener_roundtrip() {
    roundtrip("::1").await;
}

#[tokio::test]
async fn bind_target_ipv4_mapped_ipv6_listener_roundtrip() {
    roundtrip("::ffff:127.0.0.1").await;
}

#[tokio::test]
async fn bind_target_occupied_port_fails_without_fallback() {
    let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = occupied.local_addr().unwrap().port();
    let result = Site::start(
        Arc::new(Hub::new(HubConfig::default())),
        bind_mode("127.0.0.1").target().await.unwrap(),
        port,
        SiteOptions::default(),
        |_| None,
    )
    .await;
    let Err(error) = result else { panic!("bound an occupied address instead of failing") };
    assert!(error.to_string().contains("explicit IP 127.0.0.1 bind failed"), "{error}");
}

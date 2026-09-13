//! Detecting this node's Tailscale 100.x address. The bind abstraction it feeds is in
//! [`crate::bind`].

use std::net::Ipv4Addr;
use std::time::Duration;

use serde_json::Value;

use crate::bind::BindTarget;

const TAILSCALE_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

fn tailscale_ipv4(value: &str) -> Option<Ipv4Addr> {
    let ip = value.parse::<Ipv4Addr>().ok()?;
    let [a, b, _, _] = ip.octets();
    (a == 100 && (64..=127).contains(&b)).then_some(ip)
}

fn strings(value: Option<&Value>) -> Vec<&str> {
    value.and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn compact(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
}

/// Parse `tailscale status --json`. `Err` carries the reason we fall back to loopback.
pub fn parse_status_json(stdout: &str) -> Result<BindTarget, String> {
    let status: Value =
        serde_json::from_str(stdout).map_err(|e| format!("Tailscale status output was not valid JSON: {e}"))?;
    let status = status.as_object().ok_or("Tailscale status output was not an object")?;

    if let Some(state) = status.get("BackendState").and_then(Value::as_str)
        && state != "Running"
    {
        return Err(format!("Tailscale backend is {state}"));
    }
    let this_node = status.get("Self").and_then(Value::as_object);
    if this_node.and_then(|n| n.get("Online")).and_then(Value::as_bool) == Some(false) {
        return Err("Tailscale self node is offline".into());
    }
    let mut ips = strings(this_node.and_then(|n| n.get("TailscaleIPs")));
    ips.extend(strings(status.get("TailscaleIPs")));
    let ip = ips.into_iter().find_map(tailscale_ipv4).ok_or("Tailscale did not report a usable 100.x IPv4 address")?;

    let dns_name = this_node
        .and_then(|n| n.get("DNSName"))
        .and_then(Value::as_str)
        .map(|n| n.trim_end_matches('.').to_string())
        .filter(|n| !n.is_empty());
    let host_name = this_node.and_then(|n| n.get("HostName")).and_then(Value::as_str).map(str::to_string);
    let node_name = dns_name.or(host_name);

    Ok(BindTarget::tailnet(ip.into(), node_name))
}

/// Run `tailscale status --json` and pick this node's Tailscale address. `Err` carries the
/// reason there is none.
pub async fn detect() -> Result<BindTarget, String> {
    let command = tokio::process::Command::new("tailscale")
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .output();
    let output = match tokio::time::timeout(TAILSCALE_STATUS_TIMEOUT, command).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("Tailscale check failed: {error}")),
        Err(_) => return Err("Tailscale check timed out".into()),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = compact(if stderr.trim().is_empty() { &stdout } else { &stderr });
        let detail = if detail.is_empty() { format!("exit {}", output.status) } else { detail };
        return Err(format!("Tailscale unavailable: {detail}"));
    }
    parse_status_json(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_tailscale_ipv4_and_node_name() {
        let target = parse_status_json(
            r#"{"BackendState":"Running","Self":{"Online":true,"TailscaleIPs":["fd7a::1","100.101.102.103"],"DNSName":"box.tail.ts.net."}}"#,
        )
        .unwrap();
        assert_eq!(target.host, "100.101.102.103".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(target.scope, crate::bind::Scope::Tailnet);
        assert_eq!(target.label, "tailnet box.tail.ts.net");
    }

    #[test]
    fn falls_back_with_reasons() {
        assert!(parse_status_json("nope").unwrap_err().contains("valid JSON"));
        assert!(parse_status_json(r#"{"BackendState":"Stopped"}"#).unwrap_err().contains("Stopped"));
        assert!(parse_status_json(r#"{"Self":{"Online":false}}"#).unwrap_err().contains("offline"));
        assert!(
            parse_status_json(r#"{"Self":{"TailscaleIPs":["10.0.0.1","100.1.2.3"]}}"#).unwrap_err().contains("100.x")
        );
    }
}

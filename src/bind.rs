//! What the embedded server listens on: the mode the user asked for, and the address it
//! resolves to. [`crate::tailscale`] is one of the detectors this drives, not the owner.

use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;

use crate::tailscale;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindMode {
    /// Tailscale 100.x address when Tailscale is running, otherwise loopback.
    #[default]
    Auto,
    /// 127.0.0.1 only.
    Local,
    /// The Tailscale address, or fail: never serve on loopback (for a headless box where a
    /// loopback link would be useless).
    Tailscale,
    /// An explicitly selected literal IP address, or fail: never fall back.
    Ip(IpAddr),
}

/// The values every bind surface accepts, so `--help` and a parse error cannot drift apart.
pub const ACCEPTED: &str = "auto, local, tailscale, or a literal IPv4/IPv6 address (no port)";

/// One parser behind every surface: the `--bind` flag, `BRIEFING_BIND`, and `config.toml`.
impl FromStr for BindMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "local" => Ok(Self::Local),
            "tailscale" => Ok(Self::Tailscale),
            _ => value.parse().map(Self::Ip).map_err(|_| format!("invalid bind value {value:?}: expected {ACCEPTED}")),
        }
    }
}

impl<'de> serde::Deserialize<'de> for BindMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl BindMode {
    pub async fn target(self) -> anyhow::Result<BindTarget> {
        match self {
            BindMode::Local => Ok(BindTarget::local(None)),
            // The only mode that asked for the best address available rather than a specific one.
            BindMode::Auto => Ok(tailscale::detect().await.unwrap_or_else(|reason| BindTarget::local(Some(reason)))),
            BindMode::Ip(ip) => Ok(BindTarget::explicit(ip)),
            BindMode::Tailscale => {
                tailscale::detect().await.map_err(|reason| anyhow::anyhow!("bind tailscale: {reason}"))
            }
        }
    }

    /// Where to serve when [`Self::target`]'s address turns out to be unbindable. Only `auto`
    /// falls back, and only from a detected address: every other mode named the address itself.
    pub fn fallback(self, preferred: &BindTarget) -> Option<BindTarget> {
        (self == BindMode::Auto && preferred.scope == Scope::Tailnet).then(|| {
            BindTarget::local(Some(format!("Fell back to local loopback after {} bind failed", preferred.label)))
        })
    }
}

/// How far a briefing link reaches, as far as we can honestly claim. Every value the CLI, the
/// agent API, and the MCP schema can report, defined once so the schema is generated from the
/// type rather than described in prose beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(rename_all = "lowercase")]
pub enum Scope {
    /// Loopback: reachable only from this machine.
    Local,
    /// A Tailscale address: reachable from this tailnet.
    Tailnet,
    /// User-selected address; makes no claim about network reachability or trust.
    Explicit,
    // The one variant no `BindTarget` carries: nothing in this process binds a hub link.
    /// Served by a remote hub.
    Hub,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Scope::Local => "local",
            Scope::Tailnet => "tailnet",
            Scope::Explicit => "explicit",
            Scope::Hub => "hub",
        })
    }
}

/// A resolved address to serve on, with what we can tell the user about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindTarget {
    /// Address the listener binds, and the one briefing URLs advertise unless `--public-origin`
    /// overrides it.
    pub host: IpAddr,
    pub scope: Scope,
    pub label: String,
    pub diagnostics: Option<String>,
}

impl BindTarget {
    fn new(ip: IpAddr, scope: Scope, label: String, diagnostics: Option<String>) -> Self {
        Self { host: ip, scope, label, diagnostics }
    }

    pub fn explicit(ip: IpAddr) -> Self {
        Self::new(ip, Scope::Explicit, format!("explicit IP {ip}"), None)
    }

    pub fn local(diagnostics: Option<String>) -> Self {
        Self::new(Ipv4Addr::LOCALHOST.into(), Scope::Local, "local loopback".into(), diagnostics)
    }

    pub fn tailnet(ip: IpAddr, node_name: Option<String>) -> Self {
        Self::new(
            ip,
            Scope::Tailnet,
            node_name.as_ref().map_or("tailnet".to_string(), |name| format!("tailnet {name}")),
            node_name.map(|name| format!("Tailscale node: {name}")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every surface reaches [`BindMode::from_str`], so the value tables live here rather than
    /// in a subprocess matrix; `tests/bind_config.rs` checks the three layers reach this parser.
    fn from_toml(value: &str) -> Result<BindMode, toml::de::Error> {
        #[derive(serde::Deserialize)]
        struct Settings {
            bind: BindMode,
        }
        toml::from_str::<Settings>(&format!("bind = {value:?}")).map(|settings| settings.bind)
    }

    #[test]
    fn named_modes_and_literal_addresses_parse() {
        assert_eq!("auto".parse(), Ok(BindMode::Auto));
        assert_eq!("local".parse(), Ok(BindMode::Local));
        assert_eq!("tailscale".parse(), Ok(BindMode::Tailscale));
        for literal in [
            "192.0.2.10",
            "127.0.0.1",
            "0.0.0.0",
            "::",
            "::1",
            "2001:db8::1",
            "2001:0db8:0000:0000:0000:0000:0000:0001",
            "::ffff:192.0.2.1",
        ] {
            let expected = BindMode::Ip(literal.parse().unwrap());
            assert_eq!(literal.parse(), Ok(expected), "{literal}");
            assert_eq!(from_toml(literal).unwrap(), expected, "{literal}");
        }
    }

    #[test]
    fn anything_else_is_rejected_with_the_accepted_values() {
        for value in [
            "",
            "AUTO",
            "localhost",
            "briefings.example",
            "127.0.0.1:7789",
            "[::1]",
            "[::1]:7789",
            "fe80::1%eth0",
            "999.1.1.1",
            "127.1",
            "http://192.0.2.10",
            " 192.0.2.10",
        ] {
            let error = value.parse::<BindMode>().unwrap_err();
            assert!(error.contains("bind") && error.contains("literal IP"), "{value}: {error}");
            assert!(from_toml(value).is_err(), "{value}");
        }
        // A non-string TOML value cannot even reach the parser.
        for value in ["123", "false", "[]", "{}"] {
            assert!(toml::from_str::<BindMode>(value).is_err(), "{value}");
        }
    }

    #[test]
    fn targets_carry_the_scope_they_can_honestly_claim() {
        let explicit = BindTarget::explicit("192.0.2.10".parse().unwrap());
        assert_eq!(explicit.host, "192.0.2.10".parse::<IpAddr>().unwrap());
        assert_eq!(explicit.scope, Scope::Explicit);
        assert_eq!(explicit.label, "explicit IP 192.0.2.10");
        assert_eq!(explicit.diagnostics, None);

        assert_eq!(BindTarget::local(None).host, IpAddr::from(Ipv4Addr::LOCALHOST));
        assert_eq!(BindTarget::local(None).scope, Scope::Local);
        assert_eq!(BindTarget::tailnet("100.64.0.1".parse().unwrap(), None).label, "tailnet");
        let named = BindTarget::tailnet("100.64.0.1".parse().unwrap(), Some("box".into()));
        assert_eq!(named.label, "tailnet box");
        assert_eq!(named.diagnostics.as_deref(), Some("Tailscale node: box"));
    }

    /// One spelling per scope: what a CLI line prints is what the JSON and the MCP schema say.
    #[test]
    fn every_scope_prints_what_it_serializes() {
        for scope in [Scope::Local, Scope::Tailnet, Scope::Explicit, Scope::Hub] {
            assert_eq!(serde_json::to_value(scope).unwrap(), serde_json::Value::from(scope.to_string()));
        }
    }
}

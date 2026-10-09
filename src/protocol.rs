//! The version of the wire protocol between briefing clients and a hub, and compatibility
//! with the previous version.
//!
//! Every agent-API and page-API request carries a `Briefing-Protocol` header, and every
//! response names the protocol it is written in. A request without one comes from a client
//! released before versioning, which speaks protocol 1. A hub serves its own protocol and the
//! one before it, translating requests and responses for the older one and answering in it;
//! anything else gets a 426 naming both versions, labelled with the hub's protocol. A client
//! refuses a response in any protocol but its own.
//!
//! Only the API routes are versioned. Pages, the dashboard, `/mcp`, `/healthz`, and
//! `/control/shutdown` are fetched without a header (by browsers, MCP clients, and clients
//! finding or replacing a hub of any version), so they stay outside the negotiation.
//!
//! Protocol 2 replaced checkpoints and decisions with questions and reduced feedback to
//! questions, comments, and notes (see [`crate::migrate`] for the same change to stored files).
//! Protocol 3 gave each briefing one id, used by agents and in its link, and named it
//! `briefingId` in every response; a create answers with the link's reach as well, and
//! briefings carry the agent harness and session that created them.

use serde_json::{Value, json};

pub const PROTOCOL: u32 = 3;

/// The oldest protocol a hub still translates for.
pub const OLDEST_SUPPORTED: u32 = PROTOCOL - 1;

/// Version assumed when a request has no protocol header.
pub const UNVERSIONED: u32 = 1;

pub const HEADER: &str = "briefing-protocol";

/// The protocol a request asked for: its header, or protocol 1 without one.
pub fn requested(header: Option<&str>) -> Result<u32, String> {
    let version = match header {
        None => UNVERSIONED,
        Some(value) => value.trim().parse().map_err(|_| format!("invalid {HEADER} header: {value:?}"))?,
    };
    if version > PROTOCOL {
        return Err(format!(
            "this client speaks briefing protocol {version}, but this hub only speaks {OLDEST_SUPPORTED}-{PROTOCOL}; upgrade the hub"
        ));
    }
    if version < OLDEST_SUPPORTED {
        return Err(format!(
            "this client speaks briefing protocol {version}, but this hub needs {OLDEST_SUPPORTED}-{PROTOCOL}; upgrade briefing and restart the agent session"
        ));
    }
    Ok(version)
}

/// What a client does with a response's protocol header: anything but its own is an error.
pub fn check_hub(header: Option<&str>, hub: &str) -> Result<(), String> {
    match header.map(str::trim) {
        Some(value) if value == PROTOCOL.to_string() => Ok(()),
        Some(value) => Err(format!(
            "the hub at {hub} speaks briefing protocol {value}, this client speaks {PROTOCOL}; upgrade whichever is older (restart agent sessions after upgrading the client)"
        )),
        None => Err(format!(
            "the hub at {hub} predates briefing protocol {PROTOCOL} (it sent no {HEADER} header); upgrade the hub"
        )),
    }
}

// ---- Protocol 2 compatibility ----

/// A create response for a protocol 2 client, which reads exactly `{id, url}`.
pub fn created_to_v2(created: &mut Value) {
    *created = json!({ "id": created["briefingId"], "url": created["url"] });
}

/// A briefing summary for a protocol 2 client, which calls the id `id` and knows nothing of
/// the session that created it.
pub fn info_to_v2(info: &mut Value) {
    let Some(object) = info.as_object_mut() else { return };
    if let Some(id) = object.remove("briefingId") {
        object.insert("id".into(), id);
    }
    object.remove("harness");
    object.remove("session");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        assert!(requested(None).unwrap_err().contains("restart the agent session"));
        assert_eq!(requested(Some("2")), Ok(2));
        assert_eq!(requested(Some("3")), Ok(3));
        assert!(requested(Some("4")).unwrap_err().contains("upgrade the hub"));
        assert!(requested(Some("1")).unwrap_err().contains("restart the agent session"));
        assert!(requested(Some("three")).is_err());
        assert!(check_hub(Some("3"), "h").is_ok());
        assert!(check_hub(Some("4"), "h").unwrap_err().contains("speaks briefing protocol 4"));
        assert!(check_hub(None, "h").unwrap_err().contains("predates"));
    }

    #[test]
    fn v2_translations() {
        let mut created =
            json!({ "briefingId": "x", "url": "http://h/briefing/x", "scope": "local", "openedBrowser": false });
        created_to_v2(&mut created);
        assert_eq!(created, json!({ "id": "x", "url": "http://h/briefing/x" }));

        let mut info = json!({ "briefingId": "x", "title": "T", "status": "active", "createdAt": 1, "harness": "codex", "session": "s" });
        info_to_v2(&mut info);
        assert_eq!(info, json!({ "id": "x", "title": "T", "status": "active", "createdAt": 1 }));
    }
}

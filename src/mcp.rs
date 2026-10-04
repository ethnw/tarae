//! The MCP revisions tarae's two servers speak — the chat's tools (`tools.rs`, over stream-json) and Claude
//! Code's IDE side (`agent.rs`, over WebSocket) — so a CLI that moves to a newer one still finds them.
//!
//! - Up to 2025-11-25 a connection starts with `initialize` (the version negotiated there) and
//!   `notifications/initialized`; results are plain. Measured 2026-10-03 with claude 2.1.283: it sends
//!   `initialize` 2025-11-25 → `notifications/initialized` → `tools/list` (`tools::tests::installed_claude_speaks_a_revision_we_speak`,
//!   ignored — it notices an upgrade).
//! - 2026-07-28 is stateless: no handshake (a request needs nothing before it), `server/discover` says what
//!   the server speaks, each request names its version in `params._meta`, an unsupported one is answered
//!   `UnsupportedProtocolVersion` (-32022), and every result carries `resultType: "complete"` and the
//!   server's identity in its `_meta` — lists also how long they hold (`ttlMs`, `cacheScope`). `ping` is gone.
//!
//! Never an echo: `initialize` answers the client's version only when it's one of ours.

use serde_json::{Value, json};

/// The revisions spoken, oldest first.
pub const SUPPORTED: [&str; 5] = [HANDSHAKE[0], HANDSHAKE[1], HANDSHAKE[2], HANDSHAKE[3], STATELESS];
/// The ones started with `initialize` — the older two kept for IDE clients that still ask for them (one
/// that got its version back before may hang up on another); the last is the answer to a version we
/// don't speak.
const HANDSHAKE: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// The first stateless revision.
const STATELESS: &str = "2026-07-28";

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";
/// UnsupportedProtocolVersionError (2026-07-28; -32020..-32099 are the spec's).
pub const UNSUPPORTED_VERSION: i64 = -32022;
/// How long a list holds (tarae's lists don't change while it runs; an hour keeps a stale one short).
const LIST_TTL_MS: u64 = 3_600_000;

/// What a request's results look like.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rev {
    /// The handshake's (or no version said): results as they were.
    #[default]
    Handshake,
    /// 2026-07-28's: `resultType`, `_meta` serverInfo, lists' caching.
    Stateless,
}

fn server_info() -> Value {
    json!({ "name": "tarae", "version": env!("CARGO_PKG_VERSION") })
}

/// The revision a request is under, from its `_meta` — Err = the error to answer with (a version we don't
/// speak).
pub fn rev(msg: &Value) -> Result<Rev, Value> {
    let Some(v) = msg["params"]["_meta"][VERSION_KEY].as_str() else { return Ok(Rev::Handshake) };
    if !SUPPORTED.contains(&v) {
        return Err(json!({ "code": UNSUPPORTED_VERSION,
                           "message": format!("unsupported protocol version {v}"),
                           "data": { "supported": SUPPORTED, "requested": v } }));
    }
    Ok(if HANDSHAKE.contains(&v) { Rev::Handshake } else { Rev::Stateless })
}

/// The methods both servers answer alike (`capabilities` = the server's): `initialize`, `server/discover`,
/// and `ping` before 2026-07-28. None = not one of them.
pub fn common(method: &str, msg: &Value, rev: Rev, capabilities: Value) -> Option<Value> {
    Some(match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"].as_str().unwrap_or_default();
            let version = HANDSHAKE.iter().find(|v| **v == asked).unwrap_or(&HANDSHAKE[HANDSHAKE.len() - 1]);
            json!({ "protocolVersion": version, "capabilities": capabilities, "serverInfo": server_info() })
        }
        // (its identity goes in `_meta`, by `finish`)
        "server/discover" => json!({ "supportedVersions": SUPPORTED, "capabilities": capabilities }),
        "ping" if rev == Rev::Handshake => json!({}),
        _ => return None,
    })
}

/// The error for a method neither the server nor `common` knows.
pub fn not_found(method: &str) -> Value {
    json!({ "code": -32601, "message": format!("method not found: {method}") })
}

/// A result as `rev` wants it: under 2026-07-28 (and for `server/discover`, its own method) `resultType`
/// and the server's identity, and the caching of a list or of what `server/discover` says.
pub fn finish(rev: Rev, method: &str, mut result: Value) -> Value {
    if rev == Rev::Handshake && method != "server/discover" {
        return result;
    }
    if let Some(o) = result.as_object_mut() {
        o.insert("resultType".into(), json!("complete"));
        o.entry("_meta").or_insert_with(|| json!({}))[SERVER_INFO_KEY] = server_info();
        if method.ends_with("/list") || method == "server/discover" {
            o.insert("ttlMs".into(), json!(LIST_TTL_MS));
            o.insert("cacheScope".into(), json!("private"));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_never_echoes_a_version_it_does_not_speak() {
        let init = |v: &str| {
            let msg = json!({ "method": "initialize", "params": { "protocolVersion": v } });
            common("initialize", &msg, Rev::Handshake, json!({}))
                .map(|r| r["protocolVersion"].as_str().unwrap().to_string())
        };
        for v in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            assert_eq!(init(v).as_deref(), Some(v), "a handshake revision we speak");
        }
        assert_eq!(init("2024-10-07").as_deref(), Some("2025-11-25"), "older: our newest");
        assert_eq!(init("").as_deref(), Some("2025-11-25"), "none said: our newest");
        assert_eq!(init("2099-01-01").as_deref(), Some("2025-11-25"), "unknown: our newest");
        assert_eq!(init(STATELESS).as_deref(), Some("2025-11-25"), "no handshake in the stateless one");
    }

    #[test]
    fn a_request_names_its_revision_in_meta() {
        let with = |v: &str| json!({ "params": { "_meta": { VERSION_KEY: v } } });
        assert_eq!(rev(&json!({ "params": {} })), Ok(Rev::Handshake));
        assert_eq!(rev(&with("2025-11-25")), Ok(Rev::Handshake));
        assert_eq!(rev(&with("2025-03-26")), Ok(Rev::Handshake));
        assert_eq!(rev(&with("2026-07-28")), Ok(Rev::Stateless));
        let e = rev(&with("2030-01-01")).unwrap_err();
        assert_eq!(e["code"], UNSUPPORTED_VERSION);
        assert_eq!(e["data"]["supported"], json!(SUPPORTED));
        // ping is gone in 2026-07-28
        assert!(common("ping", &json!({}), Rev::Handshake, json!({})).is_some());
        assert!(common("ping", &json!({}), Rev::Stateless, json!({})).is_none());
    }
}

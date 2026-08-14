//! Delivery of Cursor Agent lifecycle hooks.
//!
//! cursor-agent runs plugin hooks with a constructed environment that does not
//! inherit `QMUX_*`. The Claude-style "no-op unless the qmux env is set" shim
//! therefore never notifies, the session is never bound, and a restored pane
//! cannot `--resume`. Instead the app writes one binding file per live Cursor
//! pane and this module matches an arriving hook using `conversation_id` /
//! `session_id` and, before that is known, `workspace_roots` / `cwd`.
//!
//! A hook that matches nothing exits quietly so a standalone `cursor-agent`
//! that happens to load the qmux plugin is unaffected.

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct PaneBinding {
    pane_id: String,
    agent_id: String,
    session_id: Option<String>,
    cwd: String,
    canonical_cwd: String,
    sock: String,
    token: String,
    updated_at: u64,
}

pub fn notify(event: String, bindings_dir: Option<String>) -> Result<(), String> {
    let mut stdin = String::new();
    std::io::stdin()
        .read_to_string(&mut stdin)
        .map_err(|err| format!("failed to read stdin: {err}"))?;
    let payload = parse_payload(&stdin);

    let Some(dir) = bindings_dir
        .map(PathBuf::from)
        .or_else(default_bindings_dir)
    else {
        return Ok(());
    };
    let Some(binding) = resolve_binding(&dir, &payload) else {
        return Ok(());
    };

    crate::request_silent_with(
        &binding.sock,
        &binding.token,
        "hook.notify",
        json!({
            "event": event,
            "paneId": binding.pane_id,
            "agentId": binding.agent_id,
            "adapterId": "cursor",
            "payload": payload,
        }),
    )
}

fn parse_payload(input: &str) -> Value {
    if input.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_string()))
    }
}

/// Session id first (exact, distinguishes two panes in the same workspace),
/// then directory — but only for a binding that has not been claimed yet, so a
/// `cursor-agent` started outside qmux in the same repo cannot post to a live
/// pane. Among unclaimed bindings the most recently launched wins.
fn resolve_binding(dir: &Path, payload: &Value) -> Option<PaneBinding> {
    let bindings = read_bindings(dir);
    if let Some(session_id) = payload_session_id(payload)
        && let Some(binding) = bindings
            .iter()
            .find(|binding| binding.session_id.as_deref() == Some(session_id.as_str()))
    {
        return Some(clone_binding(binding));
    }

    let cwd = payload_cwd(payload)?;
    let canonical = fs::canonicalize(&cwd)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| cwd.clone());
    bindings
        .iter()
        .filter(|binding| binding.session_id.is_none())
        .filter(|binding| {
            binding.cwd == cwd
                || binding.canonical_cwd == canonical
                || binding.cwd == canonical
                || binding.canonical_cwd == cwd
        })
        .max_by_key(|binding| binding.updated_at)
        .map(clone_binding)
}

fn payload_session_id(payload: &Value) -> Option<String> {
    string_field(payload, "conversation_id")
        .or_else(|| string_field(payload, "session_id"))
        .or_else(|| string_field(payload, "sessionId"))
}

fn payload_cwd(payload: &Value) -> Option<String> {
    string_field(payload, "cwd").or_else(|| {
        payload
            .get("workspace_roots")
            .and_then(Value::as_array)
            .and_then(|roots| roots.first())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map(ToString::to_string)
    })
}

fn clone_binding(binding: &PaneBinding) -> PaneBinding {
    PaneBinding {
        pane_id: binding.pane_id.clone(),
        agent_id: binding.agent_id.clone(),
        session_id: binding.session_id.clone(),
        cwd: binding.cwd.clone(),
        canonical_cwd: binding.canonical_cwd.clone(),
        sock: binding.sock.clone(),
        token: binding.token.clone(),
        updated_at: binding.updated_at,
    }
}

fn default_bindings_dir() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("QMUX_CURSOR_HOME") {
        return Some(PathBuf::from(explicit).join("bindings"));
    }
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data_home.join("qmux").join("cursor").join("bindings"))
}

fn read_bindings(dir: &Path) -> Vec<PaneBinding> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        })
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter_map(|value| parse_binding(&value))
        .collect()
}

fn parse_binding(value: &Value) -> Option<PaneBinding> {
    let cwd = string_field(value, "cwd")?;
    Some(PaneBinding {
        pane_id: string_field(value, "paneId")?,
        agent_id: string_field(value, "agentId")?,
        session_id: string_field(value, "sessionId"),
        canonical_cwd: string_field(value, "canonicalCwd").unwrap_or_else(|| cwd.clone()),
        cwd,
        sock: string_field(value, "sock")?,
        token: string_field(value, "token")?,
        updated_at: value
            .get("updatedAt")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    })
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(pane: &str, cwd: &str, session: Option<&str>, updated_at: u64) -> Value {
        json!({
            "paneId": pane,
            "agentId": format!("agent-for-{pane}"),
            "cwd": cwd,
            "canonicalCwd": cwd,
            "sessionId": session,
            "sock": "/tmp/qmux.sock",
            "token": format!("token-{pane}"),
            "updatedAt": updated_at,
        })
    }

    fn write_bindings(name: &str, documents: &[Value]) -> PathBuf {
        let home = env::temp_dir().join(format!("qmux-cursor-cli-{name}-{}", std::process::id()));
        let dir = home.join("bindings");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&dir).unwrap();
        for document in documents {
            let pane = document["paneId"].as_str().unwrap();
            fs::write(dir.join(format!("{pane}.json")), document.to_string()).unwrap();
        }
        dir
    }

    fn resolve(dir: &Path, payload: Value) -> Option<PaneBinding> {
        resolve_binding(dir, &payload)
    }

    #[test]
    fn conversation_id_wins_over_a_shared_directory() {
        let dir = write_bindings(
            "session-wins",
            &[
                binding("pane-1", "/work", Some("chat-a"), 10),
                binding("pane-2", "/work", Some("chat-b"), 20),
            ],
        );

        let found = resolve(
            &dir,
            json!({ "conversation_id": "chat-a", "workspace_roots": ["/work"] }),
        )
        .expect("session match");
        assert_eq!(found.pane_id, "pane-1");
        assert_eq!(found.token, "token-pane-1");
    }

    #[test]
    fn workspace_roots_match_the_most_recent_unclaimed_launch() {
        let dir = write_bindings(
            "cwd-recency",
            &[
                binding("pane-1", "/work", None, 10),
                binding("pane-2", "/work", None, 20),
            ],
        );

        let found =
            resolve(&dir, json!({ "workspace_roots": ["/work"] })).expect("directory match");
        assert_eq!(found.pane_id, "pane-2");
    }

    #[test]
    fn a_claimed_binding_is_no_longer_reachable_by_directory() {
        let dir = write_bindings("claimed", &[binding("pane-1", "/work", Some("chat-a"), 10)]);

        assert!(
            resolve(
                &dir,
                json!({ "conversation_id": "outsider", "workspace_roots": ["/work"] })
            )
            .is_none()
        );
        assert!(resolve(&dir, json!({ "workspace_roots": ["/work"] })).is_none());
        assert_eq!(
            resolve(&dir, json!({ "conversation_id": "chat-a" }))
                .expect("own session")
                .pane_id,
            "pane-1"
        );
    }

    #[test]
    fn an_unmatched_payload_resolves_to_nothing() {
        let dir = write_bindings("unmatched", &[binding("pane-1", "/work", None, 10)]);
        assert!(resolve(&dir, json!({ "workspace_roots": ["/elsewhere"] })).is_none());
        assert!(resolve(&dir, json!({})).is_none());
    }
}

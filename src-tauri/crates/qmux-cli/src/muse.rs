//! Delivery of Muse Code lifecycle hooks.
//!
//! Every other adapter's hook shim identifies its pane from the environment
//! (`QMUX_PANE_ID` / `QMUX_SOCK` / `QMUX_TOKEN`). Muse runs hooks with a
//! sanitized environment that strips every `QMUX_*` variable, so that route does
//! not exist. Instead the app writes one *binding file* per live Muse pane
//! before launching the process, and this module matches an arriving hook to a
//! binding using the two identifiers Muse does put in every payload: the
//! `session_id` and the `cwd`.
//!
//! A hook that matches nothing exits quietly. That is the normal outcome for a
//! `muse` the user started outside qmux, which still inherits the globally
//! installed plugin.

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// One live Muse pane, as recorded by the app before launch.
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
        // Not a qmux-launched Muse session (or its pane is gone). Say nothing:
        // Muse surfaces hook stderr in the session log, and a standalone run
        // must not be littered with qmux diagnostics.
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
            "adapterId": "muse",
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

/// Picks the pane this hook belongs to.
///
/// Session id first: it is exact, and it is the only thing that tells two panes
/// working in the same directory apart. The app records it at launch for a
/// resume (`muse resume <id>` never fires SessionStart, so there is no later
/// chance) and refreshes it from SessionStart for a fresh session.
///
/// Directory second, and then only for a binding that has not been claimed by a
/// session yet. That restriction is what keeps a `muse` the user started outside
/// qmux, in a directory a qmux pane is already working in, from posting its
/// hooks to that pane: once the pane's own SessionStart has stamped its binding,
/// the directory is no longer a way in. Among unclaimed bindings the most
/// recently launched wins.
fn resolve_binding(dir: &Path, payload: &Value) -> Option<PaneBinding> {
    let bindings = read_bindings(dir);
    let session_id = string_field(payload, "session_id");
    if let Some(session_id) = session_id.as_deref()
        && let Some(binding) = bindings
            .iter()
            .find(|binding| binding.session_id.as_deref() == Some(session_id))
    {
        return Some(clone_binding(binding));
    }

    let cwd = string_field(payload, "cwd")?;
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

/// Fallback for a `muse-notify` invoked by hand, mirroring
/// `muse_integration_home()` in the app's adapter.
///
/// The hook path never reaches this: Muse's env whitelist strips both
/// `QMUX_MUSE_HOME` and `XDG_DATA_HOME`, so a shim that derived the directory
/// here would silently find nothing. That is why the generated shim passes the
/// directory as an argument instead.
fn default_bindings_dir() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("QMUX_MUSE_HOME") {
        return Some(PathBuf::from(explicit).join("bindings"));
    }
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data_home.join("qmux").join("muse").join("bindings"))
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
        let home = env::temp_dir().join(format!("qmux-muse-cli-{name}-{}", std::process::id()));
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
    fn session_id_wins_over_a_shared_directory() {
        let dir = write_bindings(
            "session-wins",
            &[
                binding("pane-1", "/work", Some("session-a"), 10),
                binding("pane-2", "/work", Some("session-b"), 20),
            ],
        );

        let found = resolve(&dir, json!({ "session_id": "session-a", "cwd": "/work" }))
            .expect("session match");
        assert_eq!(found.pane_id, "pane-1");
        assert_eq!(found.token, "token-pane-1");
    }

    #[test]
    fn directory_match_prefers_the_most_recent_launch() {
        let dir = write_bindings(
            "cwd-recency",
            &[
                binding("pane-1", "/work", None, 10),
                binding("pane-2", "/work", None, 20),
            ],
        );

        // Before SessionStart is seen there is nothing but the directory to go
        // on, so the newest pane in it wins.
        let found = resolve(&dir, json!({ "session_id": "unseen", "cwd": "/work" }))
            .expect("directory match");
        assert_eq!(found.pane_id, "pane-2");
    }

    #[test]
    fn a_claimed_binding_is_no_longer_reachable_by_directory() {
        let dir = write_bindings(
            "claimed",
            &[binding("pane-1", "/work", Some("session-a"), 10)],
        );

        // A `muse` started outside qmux in the same directory reports a session
        // this binding does not know. It must not fall through to the pane.
        assert!(resolve(&dir, json!({ "session_id": "outsider", "cwd": "/work" })).is_none());
        // Nor may a payload without a session id at all.
        assert!(resolve(&dir, json!({ "cwd": "/work" })).is_none());
        // The pane's own session still resolves.
        assert_eq!(
            resolve(&dir, json!({ "session_id": "session-a", "cwd": "/work" }))
                .expect("own session")
                .pane_id,
            "pane-1"
        );
    }

    #[test]
    fn an_unmatched_payload_resolves_to_nothing() {
        let dir = write_bindings("unmatched", &[binding("pane-1", "/work", None, 10)]);
        assert!(resolve(&dir, json!({ "cwd": "/elsewhere" })).is_none());
        // A standalone `muse` run reaches the shim with no qmux pane at all.
        assert!(resolve(&dir, json!({})).is_none());
    }

    #[test]
    fn incomplete_bindings_are_ignored_rather_than_panicking() {
        let dir = write_bindings("incomplete", &[binding("pane-1", "/work", None, 10)]);
        // A half-written file (no token) must not be treated as usable.
        fs::write(
            dir.join("pane-2.json"),
            json!({ "paneId": "pane-2", "agentId": "a", "cwd": "/work" }).to_string(),
        )
        .unwrap();
        fs::write(dir.join("pane-3.json"), "{ not json").unwrap();

        let found = resolve(&dir, json!({ "cwd": "/work" })).expect("usable binding");
        assert_eq!(found.pane_id, "pane-1");
    }

    #[test]
    fn payload_parsing_tolerates_non_json_stdin() {
        assert_eq!(parse_payload(""), Value::Null);
        assert_eq!(parse_payload("   "), Value::Null);
        assert_eq!(parse_payload("oops"), Value::String("oops".to_string()));
        assert_eq!(parse_payload(r#"{"a":1}"#), json!({ "a": 1 }));
    }
}

//! Global, resumable conversation history.
//!
//! Claude and Codex keep durable JSONL transcripts outside qmux. This scanner
//! treats those stores as the source of truth, merges live qmux ownership onto
//! each entry, and launches only entries rediscovered during the command. The
//! latter is an authorization boundary: the webview chooses an opaque history
//! id, never an arbitrary transcript path or session id.

use crate::adapters::{SpawnAgentRequest, adapter_registry};
use crate::state::{AppState, PaneInfo};
use crate::workspace::{AgentStatus, WorkspaceScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HISTORY_ENTRIES: usize = 500;
const MAX_SCAN_FILES: usize = 4_000;
const MAX_METADATA_LINE_BYTES: u64 = 1024 * 1024;
const MAX_HEAD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TAIL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub adapter: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub preview: Option<String>,
    pub transcript_path: String,
    pub last_active_at: u128,
    pub created_at: Option<u128>,
    pub cwd_exists: bool,
    pub active: bool,
    pub pane_id: Option<String>,
    pub agent_id: Option<String>,
    pub status: Option<AgentStatus>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HistoryLaunchMode {
    Resume,
    Fork,
    ForkWorktree,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryLaunchRequest {
    pub history_id: String,
    pub mode: HistoryLaunchMode,
    #[serde(default)]
    pub prompt: Option<String>,
}

pub fn list(state: &AppState) -> Result<Vec<HistoryEntry>, String> {
    let mut entries = scan_stores()?;
    merge_qmux_state(state, &mut entries)?;
    entries.sort_by(|left, right| {
        right
            .last_active_at
            .cmp(&left.last_active_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    entries.truncate(MAX_HISTORY_ENTRIES);
    Ok(entries)
}

pub fn launch(state: &AppState, request: HistoryLaunchRequest) -> Result<PaneInfo, String> {
    let requested_id = request.history_id.trim();
    if requested_id.is_empty() {
        return Err("history entry is required".to_string());
    }

    // Rescan at the mutation boundary. A stale UI entry may refer to a transcript
    // that was moved/deleted after the dialog opened; more importantly, callers
    // never get to provide a filesystem path that was not independently discovered.
    let entry = list(state)?
        .into_iter()
        .find(|entry| entry.id == requested_id)
        .ok_or_else(|| "that conversation is no longer available".to_string())?;
    if !entry.cwd_exists {
        return Err(format!(
            "the original working directory no longer exists: {}",
            entry.cwd
        ));
    }
    if entry.active && request.mode == HistoryLaunchMode::Resume {
        return Err("that conversation is already open; focus it or fork it instead".to_string());
    }

    let group_id = matching_local_group(state, &entry.cwd)?;
    let fork = request.mode != HistoryLaunchMode::Resume;
    let use_worktree = request.mode == HistoryLaunchMode::ForkWorktree;
    adapter_registry(state.config())
        .get(&entry.adapter)?
        .launch(
            state,
            SpawnAgentRequest {
                adapter_id: entry.adapter,
                prompt: request.prompt.unwrap_or_default(),
                group_id,
                base_repo: Some(entry.cwd.clone()),
                base_ref: Some("HEAD".to_string()),
                cwd: (!use_worktree).then_some(entry.cwd),
                model: entry.model,
                initial_size: None,
                use_worktree: Some(use_worktree),
                options: Value::Null,
                parent_id: None,
                resume_session_id: Some(entry.session_id),
                fork_session: fork,
            },
        )
}

fn matching_local_group(state: &AppState, cwd: &str) -> Result<Option<String>, String> {
    let target = canonical_or_original(Path::new(cwd));
    Ok(state.list_groups()?.into_iter().find_map(|group| {
        if group.scope != WorkspaceScope::Terminal || group.remote.is_some() {
            return None;
        }
        let matches = group
            .base_repo
            .as_deref()
            .into_iter()
            .chain(std::iter::once(group.dir.as_str()))
            .any(|candidate| canonical_or_original(Path::new(candidate)) == target);
        matches.then_some(group.id)
    }))
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn merge_qmux_state(state: &AppState, entries: &mut [HistoryEntry]) -> Result<(), String> {
    let agents = state.list_agents()?;
    let recent = state.list_recent_sessions(MAX_HISTORY_ENTRIES)?;
    for entry in entries {
        let live = agents.iter().find(|agent| {
            agent.adapter == entry.adapter
                && (agent.session_id.as_deref() == Some(entry.session_id.as_str())
                    || agent.transcript_path.as_deref() == Some(entry.transcript_path.as_str()))
        });
        let cached = recent.iter().find(|session| {
            session.adapter == entry.adapter
                && (session.session_id.as_deref() == Some(entry.session_id.as_str())
                    || session.transcript_path.as_deref() == Some(entry.transcript_path.as_str()))
        });
        if let Some(agent) = live {
            entry.active = agent.pane_id.is_some();
            entry.pane_id = agent.pane_id.clone();
            entry.agent_id = Some(agent.id.clone());
            entry.status = Some(agent.status);
            entry.model = agent.model.clone();
            entry.effort = agent.effort.clone();
        } else if let Some(session) = cached {
            entry.pane_id = session.pane_id.clone();
            entry.agent_id = session.agent_id.clone();
            entry.status = session.status;
            entry.model = session.model.clone();
            entry.effort = session.effort.clone();
            entry.active = session.pane_id.is_some();
            if entry.preview.is_none() {
                entry.preview = session.preview.clone();
            }
        }
    }
    Ok(())
}

fn scan_stores() -> Result<Vec<HistoryEntry>, String> {
    let home = dirs::home_dir().ok_or_else(|| "HOME is unavailable".to_string())?;
    let mut by_id = HashMap::<String, HistoryEntry>::new();
    scan_claude(&home.join(".claude/projects"), &mut by_id);
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    scan_codex(&codex_home.join("sessions"), &mut by_id);
    scan_codex(&codex_home.join("archived_sessions"), &mut by_id);
    Ok(by_id.into_values().collect())
}

fn scan_claude(root: &Path, entries: &mut HashMap<String, HistoryEntry>) {
    let Ok(projects) = fs::read_dir(root) else {
        return;
    };
    let mut seen = 0_usize;
    for project in projects.flatten() {
        let Ok(kind) = project.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            if seen >= MAX_SCAN_FILES {
                return;
            }
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            seen += 1;
            if let Some(entry) = claude_entry(&path) {
                insert_newest(entries, entry);
            }
        }
    }
}

fn claude_entry(path: &Path) -> Option<HistoryEntry> {
    let session_id = path.file_stem()?.to_str()?.trim().to_string();
    if session_id.len() < 16 {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    let (cwd, preview) = read_claude_head(path)?;
    let title = latest_claude_title(path)
        .or_else(|| preview.clone())
        .unwrap_or_else(|| "Claude conversation".to_string());
    Some(HistoryEntry {
        id: format!("claude:{session_id}"),
        adapter: "claude".to_string(),
        session_id,
        cwd_exists: Path::new(&cwd).is_dir(),
        cwd,
        title: compact_preview(&title, 120),
        preview,
        transcript_path: path.display().to_string(),
        last_active_at: millis(metadata.modified().ok()),
        created_at: Some(millis(metadata.created().ok())),
        active: false,
        pane_id: None,
        agent_id: None,
        status: None,
        model: None,
        effort: None,
    })
}

fn read_claude_head(path: &Path) -> Option<(String, Option<String>)> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut consumed = 0_u64;
    let mut cwd = None;
    let mut preview = None;
    while consumed < MAX_HEAD_BYTES {
        let mut line = String::new();
        let read = reader
            .by_ref()
            .take(MAX_METADATA_LINE_BYTES + 1)
            .read_line(&mut line)
            .ok()?;
        if read == 0 {
            break;
        }
        consumed = consumed.saturating_add(read as u64);
        if read as u64 > MAX_METADATA_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            continue;
        };
        cwd = cwd.or_else(|| string_at(&value, &["cwd"]));
        if preview.is_none() && value.get("type").and_then(Value::as_str) == Some("user") {
            preview = message_text(&value).map(|text| compact_preview(&text, 180));
        }
        if cwd.is_some() && preview.is_some() {
            break;
        }
    }
    cwd.map(|cwd| (cwd, preview))
}

fn latest_claude_title(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .filter_map(|line| {
            let value = serde_json::from_str::<Value>(line).ok()?;
            (value.get("type").and_then(Value::as_str) == Some("ai-title"))
                .then(|| string_at(&value, &["aiTitle"]))
                .flatten()
        })
        .last()
}

fn scan_codex(root: &Path, entries: &mut HashMap<String, HistoryEntry>) {
    let mut stack = vec![(root.to_path_buf(), 0_u8)];
    let mut seen = 0_usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(children) = fs::read_dir(dir) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(kind) = child.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 3 {
                stack.push((child.path(), depth + 1));
                continue;
            }
            if !kind.is_file() || seen >= MAX_SCAN_FILES {
                continue;
            }
            let path = child.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
            {
                continue;
            }
            seen += 1;
            if let Some(entry) = codex_entry(&path) {
                insert_newest(entries, entry);
            }
        }
        if seen >= MAX_SCAN_FILES {
            break;
        }
    }
}

fn codex_entry(path: &Path) -> Option<HistoryEntry> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    reader
        .by_ref()
        .take(MAX_METADATA_LINE_BYTES + 1)
        .read_line(&mut first)
        .ok()?;
    if first.len() as u64 > MAX_METADATA_LINE_BYTES {
        return None;
    }
    let meta = serde_json::from_str::<Value>(first.trim_end()).ok()?;
    if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = meta.get("payload")?;
    let session_id = string_at(payload, &["id"])?;
    let cwd = string_at(payload, &["cwd"])?;
    let preview = first_codex_prompt(&mut reader);
    let metadata = fs::metadata(path).ok()?;
    Some(HistoryEntry {
        id: format!("codex:{session_id}"),
        adapter: "codex".to_string(),
        session_id,
        cwd_exists: Path::new(&cwd).is_dir(),
        cwd,
        title: preview
            .clone()
            .unwrap_or_else(|| "Codex conversation".to_string()),
        preview,
        transcript_path: path.display().to_string(),
        last_active_at: millis(metadata.modified().ok()),
        created_at: Some(millis(metadata.created().ok())),
        active: false,
        pane_id: None,
        agent_id: None,
        status: None,
        model: None,
        effort: None,
    })
}

fn first_codex_prompt(reader: &mut BufReader<File>) -> Option<String> {
    let mut consumed = 0_u64;
    while consumed < MAX_HEAD_BYTES {
        let mut line = String::new();
        let read = reader
            .by_ref()
            .take(MAX_METADATA_LINE_BYTES + 1)
            .read_line(&mut line)
            .ok()?;
        if read == 0 {
            return None;
        }
        consumed = consumed.saturating_add(read as u64);
        if read as u64 > MAX_METADATA_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("event_msg")
            && value
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                == Some("user_message")
            && let Some(text) = value
                .get("payload")
                .and_then(|payload| string_at(payload, &["message"]))
        {
            return Some(compact_preview(&text, 180));
        }
        if value.get("type").and_then(Value::as_str) == Some("response_item")
            && let Some(payload) = value.get("payload")
            && payload.get("role").and_then(Value::as_str) == Some("user")
            && let Some(text) = message_text(payload)
        {
            return Some(compact_preview(&text, 180));
        }
    }
    None
}

fn message_text(value: &Value) -> Option<String> {
    let message = value.get("message").unwrap_or(value);
    match message.get("content")? {
        Value::String(text) => non_empty(text),
        Value::Array(items) => items.iter().find_map(|item| {
            let kind = item.get("type").and_then(Value::as_str);
            if kind.is_some_and(|kind| kind != "text" && kind != "input_text") {
                return None;
            }
            item.get("text").and_then(Value::as_str).and_then(non_empty)
        }),
        _ => None,
    }
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().and_then(non_empty)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn compact_preview(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let head = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn millis(value: Option<SystemTime>) -> u128 {
    value
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn insert_newest(entries: &mut HashMap<String, HistoryEntry>, entry: HistoryEntry) {
    match entries.get(&entry.id) {
        Some(existing) if existing.last_active_at >= entry.last_active_at => {}
        _ => {
            entries.insert(entry.id.clone(), entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qmux-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut file = File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn reads_claude_cwd_prompt_and_latest_title() {
        let path = temp_file(
            "12345678-1234-1234-1234-123456789abc.jsonl",
            concat!(
                "{\"type\":\"user\",\"cwd\":\"/tmp/project\",\"message\":{\"content\":\"first prompt\"}}\n",
                "{\"type\":\"ai-title\",\"aiTitle\":\"Useful title\"}\n"
            ),
        );
        let entry = claude_entry(&path).unwrap();
        assert_eq!(entry.cwd, "/tmp/project");
        assert_eq!(entry.preview.as_deref(), Some("first prompt"));
        assert_eq!(entry.title, "Useful title");
    }

    #[test]
    fn reads_codex_session_metadata_and_prompt() {
        let path = temp_file(
            "rollout-example.jsonl",
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sess-1\",\"cwd\":\"/tmp/repo\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"fix the bug\"}}\n"
            ),
        );
        let entry = codex_entry(&path).unwrap();
        assert_eq!(entry.id, "codex:sess-1");
        assert_eq!(entry.title, "fix the bug");
    }

    #[test]
    fn scans_codex_sessions_at_the_archive_root() {
        let root = temp_file("placeholder", "").parent().unwrap().to_path_buf();
        let path = root.join("rollout-archived.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"archived-1\",\"cwd\":\"/tmp/repo\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"old task\"}}\n"
            ),
        )
        .unwrap();
        let mut entries = HashMap::new();
        scan_codex(&root, &mut entries);
        assert_eq!(
            entries
                .get("codex:archived-1")
                .map(|entry| entry.title.as_str()),
            Some("old task")
        );
    }

    #[test]
    fn preview_normalization_is_bounded() {
        assert_eq!(compact_preview("  a\n b\t c ", 20), "a b c");
        assert_eq!(compact_preview("abcdef", 3), "abc…");
    }
}

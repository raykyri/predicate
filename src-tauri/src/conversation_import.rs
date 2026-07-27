//! Importing external conversations into the research area.
//!
//! Sources land here as claude.ai/ChatGPT data-export archives (zips
//! carrying a `conversations.json`) or native harness transcripts: Claude
//! Code sessions (`~/.claude/projects/<slug>/<id>.jsonl`), Codex rollouts
//! (`$CODEX_HOME/sessions`, date-sharded), and hand-picked `.jsonl` files
//! from other harnesses.
//! The backend's role is confined byte access and, later, staging/commit;
//! parsing the source formats into records happens in the webview.

use crate::research::{
    ConfinedImportSpec, read_confined_import_file, read_confined_import_file_within,
};
use serde::Serialize;
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// Native agent transcripts are JSONL with one record per line; real Claude
/// Code sessions run to tens of MB, so the cap sits well above the largest
/// observed sessions while still bounding a mistaken pick.
pub const MAX_IMPORT_TRANSCRIPT_BYTES: usize = 64 * 1024 * 1024;
/// Cap on an export archive — both the zip on disk and the decompressed
/// `conversations.json` inside it (the latter guards against a zip bomb:
/// the entry is streamed against this cap, never trusted from its header).
/// Multi-year ChatGPT histories reach tens of MB; 256 MB leaves headroom
/// while bounding what one staging slot can pin in memory.
pub const MAX_IMPORT_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

const TRANSCRIPT_IMPORT: ConfinedImportSpec = ConfinedImportSpec {
    label: "transcript files",
    extensions: &["jsonl", "json"],
    max_bytes: MAX_IMPORT_TRANSCRIPT_BYTES,
};

const ARCHIVE_IMPORT: ConfinedImportSpec = ConfinedImportSpec {
    label: "conversation exports",
    extensions: &["zip", "json"],
    max_bytes: MAX_IMPORT_ARCHIVE_BYTES,
};

/// Reads one transcript file for import (a Claude Code `.jsonl` session, or a
/// bare `conversations.json` already extracted from an export archive), with
/// the same home confinement and symlink discipline as Markdown drops.
pub fn read_conversation_import_file(path: &Path) -> Result<String, String> {
    transcript_text(read_confined_import_file(path, &TRANSCRIPT_IMPORT)?)
}

fn transcript_text(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| "transcript files must be valid UTF-8".to_string())
}

/// Which product exported the staged archive, detected from the shape of its
/// `conversations.json` elements.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ImportArchiveFormat {
    ClaudeAi,
    Chatgpt,
}

/// One conversation's listing row for the import picker: everything the
/// dialog needs to render and select, none of the content.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedConversationMeta {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    /// Countable user/assistant messages — 0 disables the row in the picker.
    pub message_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationArchiveSummary {
    pub token: String,
    pub format: ImportArchiveFormat,
    pub conversations: Vec<StagedConversationMeta>,
}

/// The staged archive's per-conversation JSON slices, held in memory between
/// listing and the user's selection. A single slot: staging a new archive
/// replaces the old one, and `discard_conversation_archive` clears it when
/// the dialog closes. Bounded by MAX_IMPORT_ARCHIVE_BYTES via the read caps.
struct StagedArchive {
    token: String,
    slices: Vec<String>,
}

static STAGED_ARCHIVE: LazyLock<Mutex<Option<StagedArchive>>> = LazyLock::new(|| Mutex::new(None));
static STAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Reads an export archive (or bare conversations.json), detects its format,
/// stages the per-conversation slices in memory, and returns the picker
/// listing. The heavy JSON stays backend-side; the summary is a few KB.
pub fn stage_conversation_archive(path: &Path) -> Result<ConversationArchiveSummary, String> {
    stage_archive_bytes(read_confined_import_file(path, &ARCHIVE_IMPORT)?)
}

/// Confinement-free core of [`stage_conversation_archive`], split out so
/// tests can stage in-memory archives directly.
fn stage_archive_bytes(bytes: Vec<u8>) -> Result<ConversationArchiveSummary, String> {
    let json = if bytes.starts_with(b"PK") {
        extract_conversations_json(&bytes)?
    } else {
        bytes
    };
    let conversations: Vec<Value> = serde_json::from_slice(&json)
        .map_err(|err| format!("conversations.json is not a conversation array: {err}"))?;
    if conversations.is_empty() {
        return Err("this export contains no conversations".to_string());
    }
    let format = detect_archive_format(&conversations)?;
    let meta = conversations
        .iter()
        .enumerate()
        .map(|(index, conversation)| conversation_meta(format, index as u32, conversation))
        .collect::<Vec<_>>();
    let slices = conversations
        .iter()
        .map(|conversation| conversation.to_string())
        .collect::<Vec<_>>();

    let token = format!(
        "import-stage-{}",
        STAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let mut slot = STAGED_ARCHIVE
        .lock()
        .map_err(|_| "import staging lock poisoned".to_string())?;
    *slot = Some(StagedArchive {
        token: token.clone(),
        slices,
    });
    Ok(ConversationArchiveSummary {
        token,
        format,
        conversations: meta,
    })
}

/// Returns the staged JSON slices for the selected rows. Errors on a stale
/// token (a newer archive replaced this one, or the slot was discarded) so
/// the dialog can tell the user to reopen the file.
pub fn read_staged_conversations(token: &str, indices: &[u32]) -> Result<Vec<String>, String> {
    let slot = STAGED_ARCHIVE
        .lock()
        .map_err(|_| "import staging lock poisoned".to_string())?;
    let staged = slot
        .as_ref()
        .filter(|staged| staged.token == token)
        .ok_or_else(|| "this archive listing has expired — reopen the export file".to_string())?;
    indices
        .iter()
        .map(|&index| {
            staged
                .slices
                .get(index as usize)
                .cloned()
                .ok_or_else(|| format!("conversation index {index} is out of range"))
        })
        .collect()
}

/// Clears the staging slot if it still holds `token`'s archive. Best-effort
/// cleanup on dialog close; a mismatched token means a newer stage already
/// replaced it and must not be dropped.
pub fn discard_conversation_archive(token: &str) {
    if let Ok(mut slot) = STAGED_ARCHIVE.lock()
        && slot.as_ref().is_some_and(|staged| staged.token == token)
    {
        *slot = None;
    }
}

/// Pulls `conversations.json` out of an export zip, streamed against the
/// archive byte cap so a zip bomb's declared-vs-actual size mismatch cannot
/// balloon memory. Both claude.ai and ChatGPT exports keep the file at the
/// archive root; a single leading directory (a re-zipped export) is accepted
/// by matching on the file name with the shallowest path winning.
fn extract_conversations_json(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|err| format!("failed to open the export zip: {err}"))?;
    let mut best: Option<(usize, usize)> = None; // (depth, entry index)
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|err| format!("failed to list the export zip: {err}"))?;
        let name = entry.name();
        if name.ends_with("conversations.json") {
            let depth = name.matches('/').count();
            let is_exact = name == "conversations.json" || name.ends_with("/conversations.json");
            if is_exact && best.is_none_or(|(best_depth, _)| depth < best_depth) {
                best = Some((depth, index));
            }
        }
    }
    let (_, index) =
        best.ok_or_else(|| "the zip does not contain a conversations.json".to_string())?;
    let entry = archive
        .by_index(index)
        .map_err(|err| format!("failed to read conversations.json from the zip: {err}"))?;
    let mut json = Vec::new();
    entry
        .take(MAX_IMPORT_ARCHIVE_BYTES as u64 + 1)
        .read_to_end(&mut json)
        .map_err(|err| format!("failed to decompress conversations.json: {err}"))?;
    if json.len() > MAX_IMPORT_ARCHIVE_BYTES {
        return Err(format!(
            "conversations.json is limited to {} MB",
            MAX_IMPORT_ARCHIVE_BYTES / (1024 * 1024)
        ));
    }
    Ok(json)
}

/// Sessions listed per project in the Claude Code browser, newest first. A
/// long-lived project accumulates hundreds of JSONL files; the browser only
/// ever needs the recent tail, and each listed session costs a bounded
/// preview read.
const MAX_SESSIONS_PER_PROJECT: usize = 100;
/// Overall cap on the Codex browser's listing. Codex keeps every project's
/// sessions in one global date-sharded tree, so there is no per-project
/// directory to cap; the newest 300 across all projects bound the preview
/// reads instead.
const MAX_CODEX_SESSIONS: usize = 300;
/// Lines scanned from the head of a session for its `cwd` field. Every real
/// Claude Code row carries one; the margin covers leading summary rows.
const CWD_SCAN_LINE_LIMIT: usize = 10;

/// One selectable native-harness session (Claude Code or Codex) in the
/// import browser.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSessionSummary {
    /// A grouping fallback when no `cwd` can be read: the directory name
    /// under `~/.claude/projects` (a lossy path slug) for Claude Code, the
    /// transcript's parent directory name for Codex.
    pub project_slug: String,
    /// The project's real working directory, from the session's records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub path: String,
    pub modified_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Builds one listing row from a gathered candidate: bounded preview read
/// plus mtime conversion, shared by the Claude Code and Codex listers.
fn harness_session_summary(
    project_slug: String,
    project_dir: Option<String>,
    session_id: Option<String>,
    candidate: &crate::transcript::TranscriptCandidate,
) -> HarnessSessionSummary {
    let (preview, _line_count) = crate::transcript::read_transcript_meta(&candidate.path);
    HarnessSessionSummary {
        project_slug,
        project_dir,
        session_id,
        path: candidate.path.display().to_string(),
        modified_ms: candidate
            .modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
        preview,
    }
}

/// Newest first, path as the stable tiebreaker — the order both browsers
/// present.
fn sort_sessions_newest_first(sessions: &mut [HarnessSessionSummary]) {
    sessions.sort_by(|left, right| {
        right
            .modified_ms
            .cmp(&left.modified_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
}

/// Scans `~/.claude/projects` for importable Claude Code sessions: each
/// project directory's top-level `*.jsonl` files (subagent transcripts live
/// in subdirectories and are not primary conversations), newest first with a
/// per-project cap, annotated with the session's cwd and first-prompt
/// preview. Symlinked entries are ignored, matching the transcript picker's
/// discipline.
pub fn list_claude_code_sessions() -> Result<Vec<HarnessSessionSummary>, String> {
    list_claude_code_sessions_in(&home_dir()?.join(".claude").join("projects"))
}

fn home_dir() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or_else(|| "cannot determine your home directory".to_string())
}

fn list_claude_code_sessions_in(projects: &Path) -> Result<Vec<HarnessSessionSummary>, String> {
    let entries = match std::fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!("failed to inspect {}: {err}", projects.display()));
        }
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let project_slug = entry.file_name().to_string_lossy().into_owned();
        let mut candidates =
            crate::transcript::gather_transcript_candidates(&entry.path()).unwrap_or_default();
        candidates.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        candidates.truncate(MAX_SESSIONS_PER_PROJECT);
        for candidate in candidates {
            sessions.push(harness_session_summary(
                project_slug.clone(),
                transcript_cwd(&candidate.path),
                candidate.session_id.clone(),
                &candidate,
            ));
        }
    }
    sort_sessions_newest_first(&mut sessions);
    Ok(sessions)
}

/// Scans the Codex sessions tree (`$CODEX_HOME`, else `~/.codex`, then
/// `sessions/`) for importable rollouts. Unlike Claude Code's per-project
/// directories, Codex date-shards every project's sessions into one nested
/// tree, so the scan is recursive, the project comes from each rollout's
/// `session_meta` cwd, and the cap is a single overall newest-first budget.
/// Symlinked entries are ignored by the shared candidate gatherer.
pub fn list_codex_sessions() -> Result<Vec<HarnessSessionSummary>, String> {
    let root = match std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
    {
        Some(home) => home,
        None => home_dir()?.join(".codex"),
    };
    list_codex_sessions_in(&root.join("sessions"))
}

fn list_codex_sessions_in(sessions_dir: &Path) -> Result<Vec<HarnessSessionSummary>, String> {
    // A missing sessions root is an empty listing (the gatherer maps NotFound
    // to no candidates), matching the Claude Code lister.
    let mut candidates = crate::transcript::gather_transcript_candidates_recursive(sessions_dir)?;
    candidates.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates.truncate(MAX_CODEX_SESSIONS);
    let sessions = candidates
        .iter()
        .map(|candidate| {
            // Rollout filenames encode a timestamp prefix, not a bare session
            // id, so the session_meta payload id is authoritative — the same
            // preference the transcript picker applies to Codex sessions.
            let session_id = crate::transcript::codex_transcript_session_id(&candidate.path)
                .or_else(|| candidate.session_id.clone());
            let project_slug = candidate
                .path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            harness_session_summary(
                project_slug,
                crate::transcript::codex_transcript_cwd(&candidate.path),
                session_id,
                candidate,
            )
        })
        .collect();
    Ok(sessions)
}

/// Overall cap on the OpenCode browser's listing. OpenCode groups sessions
/// under per-project hash directories with no meaningful per-project budget,
/// so like Codex the newest 300 across all projects bound the listing.
const MAX_OPENCODE_SESSIONS: usize = 300;
/// A session metadata file is a few hundred bytes of ids, title, and
/// timestamps; 1 MB bounds a corrupt or hostile one during a listing scan.
const MAX_OPENCODE_METADATA_BYTES: u64 = 1024 * 1024;

/// Byte access into an OpenCode store for session assembly. The per-file cap
/// is set per read from the remaining assembly budget, so the whole combined
/// session — metadata, messages, and parts — stays under the transcript cap.
fn opencode_component_spec(max_bytes: usize) -> ConfinedImportSpec {
    ConfinedImportSpec {
        label: "OpenCode sessions",
        extensions: &["json"],
        max_bytes,
    }
}

/// Resolves OpenCode's local store root: `$XDG_DATA_HOME` (else
/// `~/.local/share`) joined with `opencode/storage`.
fn opencode_storage_root() -> Result<std::path::PathBuf, String> {
    let data_home = match std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
    {
        Some(dir) => dir,
        None => home_dir()?.join(".local").join("share"),
    };
    Ok(data_home.join("opencode").join("storage"))
}

/// Scans the OpenCode store (`$XDG_DATA_HOME`/`~/.local/share`, then
/// `opencode/storage`) for importable sessions. Unlike the JSONL harnesses,
/// OpenCode keeps per-session metadata files under `session/<project>/`, so a
/// row costs one small JSON read and no transcript preview scan: the stored
/// title is the preview, the metadata `directory` is the project dir, and the
/// metadata timestamps beat file mtimes. Symlinked entries are ignored.
pub fn list_opencode_sessions() -> Result<Vec<HarnessSessionSummary>, String> {
    list_opencode_sessions_in(&opencode_storage_root()?)
}

fn list_opencode_sessions_in(storage: &Path) -> Result<Vec<HarnessSessionSummary>, String> {
    let session_root = storage.join("session");
    let projects = match std::fs::read_dir(&session_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "failed to inspect {}: {err}",
                session_root.display()
            ));
        }
    };

    let mut sessions = Vec::new();
    for project in projects {
        let Ok(project) = project else {
            continue;
        };
        let Ok(project_type) = project.file_type() else {
            continue;
        };
        if project_type.is_symlink() || !project_type.is_dir() {
            continue;
        }
        let project_slug = project.file_name().to_string_lossy().into_owned();
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            if let Some(row) = opencode_session_row(&entry.path(), project_slug.clone()) {
                sessions.push(row);
            }
        }
    }
    sort_sessions_newest_first(&mut sessions);
    sessions.truncate(MAX_OPENCODE_SESSIONS);
    Ok(sessions)
}

/// One listing row from an OpenCode session metadata file, or None when the
/// file is not a parseable `.json` metadata object.
fn opencode_session_row(path: &Path, project_slug: String) -> Option<HarnessSessionSummary> {
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_OPENCODE_METADATA_BYTES {
        return None;
    }
    let meta: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;

    // The stored `updated` (else `created`) epoch-ms beats the file mtime,
    // which archival tools can rewrite.
    let stored_ms = meta
        .get("time")
        .and_then(|time| time.get("updated").or_else(|| time.get("created")))
        .and_then(Value::as_u64)
        .map(u128::from);
    let modified_ms = stored_ms
        .or_else(|| {
            metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis())
        })
        .unwrap_or_default();

    let preview = string_field(&meta, "title")
        .filter(|title| !title.trim().is_empty())
        // `summary` is an object in current stores, so this only fires on a
        // hypothetical string-valued variant — a graceful degrade, not a path
        // the schema exercises today.
        .or_else(|| string_field(&meta, "summary").filter(|summary| !summary.trim().is_empty()));

    Some(HarnessSessionSummary {
        project_slug,
        project_dir: string_field(&meta, "directory").filter(|dir| !dir.is_empty()),
        session_id: string_field(&meta, "id"),
        path: path.display().to_string(),
        modified_ms,
        preview,
    })
}

/// True when an id read from store JSON is safe to use as a single path
/// component — OpenCode ids are `ses_`/`msg_` tokens, so anything resembling
/// path syntax is treated as corruption rather than resolved.
fn safe_opencode_component(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && !id.contains("..")
}

/// Reads one store file for session assembly under the shared confinement
/// discipline (canonicalize, containment in the storage root, `.json` only),
/// charging the shared assembly budget so the combined session stays under
/// the transcript cap.
fn read_opencode_component(
    path: &Path,
    storage: &Path,
    budget: &mut usize,
) -> Result<Vec<u8>, String> {
    let spec = opencode_component_spec(*budget);
    let bytes = read_confined_import_file_within(path, storage, &spec).map_err(|err| {
        // The per-read cap is the remaining budget, so a size failure means
        // the assembled session (not this one file) blew the overall cap.
        if err.contains("limited to") {
            format!(
                "OpenCode sessions are limited to {} MB",
                MAX_IMPORT_TRANSCRIPT_BYTES / (1024 * 1024)
            )
        } else {
            err
        }
    })?;
    *budget -= bytes.len();
    Ok(bytes)
}

/// Assembles one OpenCode session for import from its metadata file path:
/// the metadata itself, every message in `message/<sessionId>/`, and each
/// message's parts from `part/<messageId>/`, combined as
/// `{"session": …, "messages": [{…, "parts": […]}, …]}` with messages in
/// conversation order (stored creation time, then id) and parts in id order
/// (OpenCode part ids sort in creation order). Every file read is confined to
/// the resolved storage root and charged against the transcript byte cap.
pub fn read_opencode_session(session_path: &Path) -> Result<String, String> {
    read_opencode_session_in(session_path, &opencode_storage_root()?)
}

fn read_opencode_session_in(session_path: &Path, storage: &Path) -> Result<String, String> {
    let mut budget = MAX_IMPORT_TRANSCRIPT_BYTES;
    let session: Value = serde_json::from_slice(&read_opencode_component(
        session_path,
        storage,
        &mut budget,
    )?)
    .map_err(|err| format!("the session metadata is not valid JSON: {err}"))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| safe_opencode_component(id))
        .ok_or_else(|| "the session metadata has no usable id".to_string())?;

    let message_dir = storage.join("message").join(session_id);
    let entries = match std::fs::read_dir(&message_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err("session has no messages".to_string());
        }
        Err(err) => {
            return Err(format!(
                "failed to inspect {}: {err}",
                message_dir.display()
            ));
        }
    };

    let mut messages = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let bytes = read_opencode_component(&path, storage, &mut budget)?;
        // A corrupt message file drops that message rather than the session.
        let Ok(mut message) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let parts = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| safe_opencode_component(id))
            .map(|message_id| {
                opencode_message_parts(&storage.join("part").join(message_id), storage, &mut budget)
            })
            .transpose()?
            .unwrap_or_default();
        if let Some(object) = message.as_object_mut() {
            object.insert("parts".to_string(), Value::Array(parts));
            messages.push(message);
        }
    }
    if messages.is_empty() {
        return Err("session has no messages".to_string());
    }
    // Conversation order: stored creation time, id as the stable tiebreaker
    // (OpenCode ids sort in creation order, covering equal timestamps).
    messages.sort_by_cached_key(|message| {
        (
            message
                .get("time")
                .and_then(|time| time.get("created"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    });

    Ok(serde_json::json!({ "session": session, "messages": messages }).to_string())
}

/// One message's parts in id (= creation) order. A missing part directory is
/// an empty part list — metadata-only messages exist in real stores.
fn opencode_message_parts(
    part_dir: &Path,
    storage: &Path,
    budget: &mut usize,
) -> Result<Vec<Value>, String> {
    let entries = match std::fs::read_dir(part_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    let mut parts = Vec::new();
    for path in paths {
        let bytes = read_opencode_component(&path, storage, budget)?;
        if let Ok(part) = serde_json::from_slice::<Value>(&bytes) {
            parts.push(part);
        }
    }
    Ok(parts)
}

/// The session's working directory from the head of its transcript: the first
/// `cwd` string within the scan window. The whole head read is byte-bounded
/// so one pathological record cannot balloon a listing scan.
fn transcript_cwd(path: &Path) -> Option<String> {
    use std::io::BufRead;
    const CWD_SCAN_BYTE_LIMIT: u64 = 4 * 1024 * 1024;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file.take(CWD_SCAN_BYTE_LIMIT));
    for line in reader.lines().take(CWD_SCAN_LINE_LIMIT) {
        let line = line.ok()?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str)
            && !cwd.is_empty()
        {
            return Some(cwd.to_string());
        }
    }
    None
}

/// claude.ai conversations carry `chat_messages`; ChatGPT conversations carry
/// a `mapping` node tree. The first element decides and every element must
/// agree — a mixed file is corrupt, not a third format.
fn detect_archive_format(conversations: &[Value]) -> Result<ImportArchiveFormat, String> {
    let format_of = |conversation: &Value| {
        if conversation.get("chat_messages").is_some() {
            Some(ImportArchiveFormat::ClaudeAi)
        } else if conversation.get("mapping").is_some() {
            Some(ImportArchiveFormat::Chatgpt)
        } else {
            None
        }
    };
    let format = format_of(&conversations[0]).ok_or_else(|| {
        "unrecognized export format: expected a claude.ai or ChatGPT conversations.json".to_string()
    })?;
    if conversations
        .iter()
        .any(|conversation| format_of(conversation) != Some(format))
    {
        return Err("the export mixes conversation formats and cannot be imported".to_string());
    }
    Ok(format)
}

fn conversation_meta(
    format: ImportArchiveFormat,
    index: u32,
    conversation: &Value,
) -> StagedConversationMeta {
    match format {
        ImportArchiveFormat::ClaudeAi => StagedConversationMeta {
            index,
            id: string_field(conversation, "uuid"),
            title: string_field(conversation, "name")
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "Untitled conversation".to_string()),
            created_at: timestamp_field(conversation, "created_at"),
            updated_at: timestamp_field(conversation, "updated_at"),
            message_count: conversation
                .get("chat_messages")
                .and_then(Value::as_array)
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| {
                            matches!(
                                message.get("sender").and_then(Value::as_str),
                                Some("human") | Some("assistant")
                            )
                        })
                        .count()
                })
                .unwrap_or(0),
        },
        ImportArchiveFormat::Chatgpt => StagedConversationMeta {
            index,
            id: string_field(conversation, "id")
                .or_else(|| string_field(conversation, "conversation_id")),
            title: string_field(conversation, "title")
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "Untitled conversation".to_string()),
            created_at: timestamp_field(conversation, "create_time"),
            updated_at: timestamp_field(conversation, "update_time"),
            message_count: conversation
                .get("mapping")
                .and_then(Value::as_object)
                .map(|mapping| {
                    mapping
                        .values()
                        .filter(|node| {
                            matches!(
                                node.get("message")
                                    .and_then(|message| message.get("author"))
                                    .and_then(|author| author.get("role"))
                                    .and_then(Value::as_str),
                                Some("user") | Some("assistant")
                            )
                        })
                        .count()
                })
                .unwrap_or(0),
        },
    }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_string)
}

/// Epoch-ms from either export's timestamp spelling: RFC3339 strings
/// (claude.ai) or float epoch seconds (ChatGPT).
fn timestamp_field(value: &Value, field: &str) -> Option<i64> {
    match value.get(field)? {
        Value::String(text) => crate::transcript::rfc3339_to_epoch_ms(text),
        Value::Number(number) => {
            let raw = number.as_f64()?;
            if !raw.is_finite() || raw <= 0.0 {
                return None;
            }
            // Second-resolution epochs sit far below any millisecond epoch of
            // the same era; scale them up rather than misreading them as 1970.
            if raw >= 1_000_000_000_000.0 {
                Some(raw as i64)
            } else {
                Some((raw * 1000.0) as i64)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::read_confined_import_file_within;
    use std::fs;

    fn read_transcript_within(path: &Path, root: &Path) -> Result<String, String> {
        transcript_text(read_confined_import_file_within(
            path,
            root,
            &TRANSCRIPT_IMPORT,
        )?)
    }

    fn claude_ai_conversation(uuid: &str, name: &str, messages: usize) -> Value {
        serde_json::json!({
            "uuid": uuid,
            "name": name,
            "created_at": "2026-07-01T10:00:00.000Z",
            "updated_at": "2026-07-02T10:00:00.000Z",
            "chat_messages": (0..messages).map(|i| serde_json::json!({
                "sender": if i % 2 == 0 { "human" } else { "assistant" },
                "text": format!("message {i}"),
                "created_at": "2026-07-01T10:00:00.000Z",
            })).collect::<Vec<_>>(),
        })
    }

    fn chatgpt_conversation(id: &str, title: &str) -> Value {
        serde_json::json!({
            "id": id,
            "title": title,
            "create_time": 1_782_900_000.5f64,
            "update_time": 1_782_986_400.0f64,
            "current_node": "n2",
            "mapping": {
                "n0": { "id": "n0", "parent": null, "children": ["n1"] },
                "n1": {
                    "id": "n1", "parent": "n0", "children": ["n2"],
                    "message": { "author": { "role": "user" },
                                 "content": { "content_type": "text", "parts": ["hi"] } }
                },
                "n2": {
                    "id": "n2", "parent": "n1", "children": [],
                    "message": { "author": { "role": "assistant" },
                                 "content": { "content_type": "text", "parts": ["hello"] } }
                }
            }
        })
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut cursor);
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        cursor.into_inner()
    }

    // One test drives the whole staging lifecycle: STAGED_ARCHIVE is a
    // process-global single slot, so splitting these into parallel tests
    // would race on it.
    #[test]
    fn archive_staging_lists_reads_and_discards_conversations() {
        let conversations = serde_json::to_vec(&vec![
            claude_ai_conversation("conv-1", "Trip planning", 4),
            claude_ai_conversation("conv-2", "  ", 0),
        ])
        .unwrap();
        let archive = zip_with(&[
            ("users.json", b"[]".as_slice()),
            ("conversations.json", conversations.as_slice()),
        ]);

        let summary = stage_archive_bytes(archive).unwrap();
        assert_eq!(summary.format, ImportArchiveFormat::ClaudeAi);
        assert_eq!(summary.conversations.len(), 2);
        let first = &summary.conversations[0];
        assert_eq!(first.id.as_deref(), Some("conv-1"));
        assert_eq!(first.title, "Trip planning");
        assert_eq!(first.message_count, 4);
        assert!(first.created_at.is_some_and(|ms| ms > 1_700_000_000_000));
        // A blank name falls back rather than rendering an empty row.
        assert_eq!(summary.conversations[1].title, "Untitled conversation");
        assert_eq!(summary.conversations[1].message_count, 0);

        // Slices come back for selected indices only, and parse as the
        // original conversation objects.
        let slices = read_staged_conversations(&summary.token, &[1]).unwrap();
        assert_eq!(slices.len(), 1);
        let parsed: Value = serde_json::from_str(&slices[0]).unwrap();
        assert_eq!(parsed["uuid"], "conv-2");
        let error = read_staged_conversations(&summary.token, &[5]).unwrap_err();
        assert!(error.contains("out of range"), "{error}");

        // Staging a ChatGPT export replaces the slot; the old token expires.
        let chatgpt =
            serde_json::to_vec(&vec![chatgpt_conversation("g-1", "Rust question")]).unwrap();
        let replacement = stage_archive_bytes(chatgpt).unwrap();
        assert_eq!(replacement.format, ImportArchiveFormat::Chatgpt);
        assert_eq!(replacement.conversations[0].message_count, 2);
        // ChatGPT float-second timestamps scale to ms.
        assert_eq!(
            replacement.conversations[0].created_at,
            Some(1_782_900_000_500)
        );
        let error = read_staged_conversations(&summary.token, &[0]).unwrap_err();
        assert!(error.contains("expired"), "{error}");

        // A stale token does not discard the newer stage; the live one does.
        discard_conversation_archive(&summary.token);
        assert!(read_staged_conversations(&replacement.token, &[0]).is_ok());
        discard_conversation_archive(&replacement.token);
        let error = read_staged_conversations(&replacement.token, &[0]).unwrap_err();
        assert!(error.contains("expired"), "{error}");
    }

    #[test]
    fn archive_staging_rejects_bad_shapes() {
        // No conversations.json in the zip.
        let error = stage_archive_bytes(zip_with(&[("users.json", b"[]".as_slice())])).unwrap_err();
        assert!(error.contains("conversations.json"), "{error}");

        // Bare JSON that is not an array.
        let error = stage_archive_bytes(b"{\"chat_messages\":[]}".to_vec()).unwrap_err();
        assert!(error.contains("conversation array"), "{error}");

        // An empty export has nothing to pick.
        let error = stage_archive_bytes(b"[]".to_vec()).unwrap_err();
        assert!(error.contains("no conversations"), "{error}");

        // Neither claude.ai nor ChatGPT shape.
        let error = stage_archive_bytes(b"[{\"messages\":[]}]".to_vec()).unwrap_err();
        assert!(error.contains("unrecognized export format"), "{error}");

        // Mixed shapes are corruption, not a third format.
        let mixed = serde_json::to_vec(&vec![
            claude_ai_conversation("conv-1", "A", 2),
            chatgpt_conversation("g-1", "B"),
        ])
        .unwrap();
        let error = stage_archive_bytes(mixed).unwrap_err();
        assert!(error.contains("mixes conversation formats"), "{error}");
    }

    #[test]
    fn conversations_json_is_found_at_root_or_one_level_deep() {
        let conversations =
            serde_json::to_vec(&vec![claude_ai_conversation("conv-1", "Nested", 2)]).unwrap();
        // A re-zipped export with a leading directory still resolves, and the
        // shallowest match wins over a deeper decoy.
        let archive = zip_with(&[
            (
                "export/backup/conversations.json",
                b"not the real one".as_slice(),
            ),
            ("export/conversations.json", conversations.as_slice()),
        ]);
        // Goes through the pure extractor: stage_archive_bytes would write
        // the process-global staging slot and race the lifecycle test.
        let json = extract_conversations_json(&archive).unwrap();
        let parsed: Vec<Value> = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed[0]["name"], "Nested");
    }

    #[test]
    fn claude_code_session_listing_scans_projects_with_previews_and_cwd() {
        let projects =
            std::env::temp_dir().join(format!("qmux-import-projects-{}", std::process::id()));
        let _ = fs::remove_dir_all(&projects);
        let project = projects.join("-Users-bob-code-demo");
        fs::create_dir_all(&project).unwrap();

        let session = project.join("11111111-aaaa-bbbb-cccc-000000000001.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"summary\",\"summary\":\"prior context\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/Users/bob/code/demo\",\"message\":{\"role\":\"user\",\"content\":\"fix the flaky test\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"on it\"}}\n",
            ),
        )
        .unwrap();
        // Subagent transcripts live in subdirectories and stay out of the
        // primary listing; symlinked files are ignored outright.
        let subagents = project
            .join("11111111-aaaa-bbbb-cccc-000000000001")
            .join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-x.jsonl"), "{}\n").unwrap();
        std::os::unix::fs::symlink(&session, project.join("link.jsonl")).unwrap();

        let missing = list_claude_code_sessions_in(&projects.join("does-not-exist")).unwrap();
        assert!(missing.is_empty());

        let sessions = list_claude_code_sessions_in(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        let listed = &sessions[0];
        assert_eq!(listed.project_slug, "-Users-bob-code-demo");
        assert_eq!(listed.project_dir.as_deref(), Some("/Users/bob/code/demo"));
        assert_eq!(
            listed.session_id.as_deref(),
            Some("11111111-aaaa-bbbb-cccc-000000000001")
        );
        assert_eq!(listed.preview.as_deref(), Some("fix the flaky test"));
        assert!(listed.modified_ms > 0);

        let _ = fs::remove_dir_all(projects);
    }

    #[test]
    fn codex_session_listing_scans_the_sharded_tree_with_meta_and_previews() {
        let root = std::env::temp_dir().join(format!("qmux-import-codex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let day = root.join("sessions").join("2026").join("07").join("27");
        fs::create_dir_all(&day).unwrap();

        let rollout = day.join("rollout-x.jsonl");
        fs::write(
            &rollout,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"22222222-bbbb-cccc-dddd-000000000002\",\"cwd\":\"/Users/bob/code/demo\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"tighten the retry loop\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"on it\"}]}}\n",
            ),
        )
        .unwrap();
        // Symlinked rollouts are skipped by the shared candidate gatherer.
        std::os::unix::fs::symlink(&rollout, day.join("link.jsonl")).unwrap();

        let missing = list_codex_sessions_in(&root.join("no-such-sessions")).unwrap();
        assert!(missing.is_empty());

        let sessions = list_codex_sessions_in(&root.join("sessions")).unwrap();
        assert_eq!(sessions.len(), 1);
        let listed = &sessions[0];
        // The filename stem ("rollout-x") is a timestamp shard, not a session
        // id; the session_meta payload id wins.
        assert_eq!(
            listed.session_id.as_deref(),
            Some("22222222-bbbb-cccc-dddd-000000000002")
        );
        assert_eq!(listed.project_dir.as_deref(), Some("/Users/bob/code/demo"));
        assert_eq!(listed.project_slug, "27");
        assert_eq!(listed.preview.as_deref(), Some("tighten the retry loop"));
        assert!(listed.modified_ms > 0);

        let _ = fs::remove_dir_all(root);
    }

    /// One session's worth of invented OpenCode store files, mirroring the
    /// observed layout: `session/<project>/<ses>.json` metadata,
    /// `message/<ses>/msg_*.json` messages, `part/<msg>/prt_*.json` parts.
    fn write_opencode_store(storage: &Path) {
        let session_dir = storage.join("session").join("global");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("ses_alpha.json"),
            serde_json::json!({
                "id": "ses_alpha",
                "version": "1.0.0",
                "projectID": "global",
                "directory": "/Users/pat/code/demo",
                "title": "Tidy the changelog",
                "time": { "created": 1_770_000_000_000u64, "updated": 1_770_000_009_000u64 },
                "summary": { "additions": 1, "removals": 2 },
            })
            .to_string(),
        )
        .unwrap();

        let message_dir = storage.join("message").join("ses_alpha");
        fs::create_dir_all(&message_dir).unwrap();
        // Written with ids out of creation order to prove the sort; msg_b is
        // the earlier (user) message.
        fs::write(
            message_dir.join("msg_late.json"),
            serde_json::json!({
                "id": "msg_late",
                "sessionID": "ses_alpha",
                "role": "assistant",
                "time": { "created": 1_770_000_002_000u64, "completed": 1_770_000_003_000u64 },
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            message_dir.join("msg_early.json"),
            serde_json::json!({
                "id": "msg_early",
                "sessionID": "ses_alpha",
                "role": "user",
                "time": { "created": 1_770_000_001_000u64 },
            })
            .to_string(),
        )
        .unwrap();

        let user_parts = storage.join("part").join("msg_early");
        fs::create_dir_all(&user_parts).unwrap();
        fs::write(
            user_parts.join("prt_a1.json"),
            serde_json::json!({
                "id": "prt_a1", "messageID": "msg_early", "sessionID": "ses_alpha",
                "type": "text", "text": "trim the changelog to the last release",
            })
            .to_string(),
        )
        .unwrap();

        let assistant_parts = storage.join("part").join("msg_late");
        fs::create_dir_all(&assistant_parts).unwrap();
        fs::write(
            assistant_parts.join("prt_b1.json"),
            serde_json::json!({
                "id": "prt_b1", "messageID": "msg_late", "sessionID": "ses_alpha",
                "type": "step-start",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            assistant_parts.join("prt_b2.json"),
            serde_json::json!({
                "id": "prt_b2", "messageID": "msg_late", "sessionID": "ses_alpha",
                "type": "text", "text": "trimmed it down to two entries",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            assistant_parts.join("prt_b3.json"),
            serde_json::json!({
                "id": "prt_b3", "messageID": "msg_late", "sessionID": "ses_alpha",
                "type": "tool", "tool": "bash", "callID": "call_1",
                "state": { "status": "completed", "input": {}, "output": "ok" },
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn opencode_session_listing_reads_metadata_rows() {
        let storage =
            std::env::temp_dir().join(format!("qmux-import-opencode-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&storage);
        write_opencode_store(&storage);
        let session_dir = storage.join("session").join("global");
        // An untitled session previews via a string summary; the current
        // object-valued summary yields no preview.
        fs::write(
            session_dir.join("ses_beta.json"),
            serde_json::json!({
                "id": "ses_beta",
                "projectID": "global",
                "directory": "/Users/pat/code/demo",
                "title": "  ",
                "summary": "planning pass over the importer",
                "time": { "created": 1_770_000_100_000u64, "updated": 1_770_000_100_000u64 },
            })
            .to_string(),
        )
        .unwrap();
        // Symlinked metadata files are skipped outright.
        std::os::unix::fs::symlink(
            session_dir.join("ses_alpha.json"),
            session_dir.join("ses_link.json"),
        )
        .unwrap();

        let missing = list_opencode_sessions_in(&storage.join("no-such-storage")).unwrap();
        assert!(missing.is_empty());

        let sessions = list_opencode_sessions_in(&storage).unwrap();
        assert_eq!(sessions.len(), 2);
        // Newest first: ses_beta's stored `updated` beats ses_alpha's.
        assert_eq!(
            sessions[0].preview.as_deref(),
            Some("planning pass over the importer")
        );
        let listed = &sessions[1];
        assert_eq!(listed.project_slug, "global");
        assert_eq!(listed.project_dir.as_deref(), Some("/Users/pat/code/demo"));
        assert_eq!(listed.session_id.as_deref(), Some("ses_alpha"));
        assert_eq!(listed.preview.as_deref(), Some("Tidy the changelog"));
        // The stored `updated` epoch-ms, not the file mtime.
        assert_eq!(listed.modified_ms, 1_770_000_009_000);
        assert!(listed.path.ends_with("ses_alpha.json"));

        let _ = fs::remove_dir_all(storage);
    }

    #[test]
    fn opencode_session_listing_caps_the_overall_row_count() {
        let storage =
            std::env::temp_dir().join(format!("qmux-import-opencode-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&storage);
        let session_dir = storage.join("session").join("global");
        fs::create_dir_all(&session_dir).unwrap();
        for index in 0..(MAX_OPENCODE_SESSIONS + 1) {
            fs::write(
                session_dir.join(format!("ses_{index:04}.json")),
                serde_json::json!({
                    "id": format!("ses_{index:04}"),
                    "title": format!("session {index}"),
                    "time": { "created": 1_770_000_000_000u64 + index as u64 },
                })
                .to_string(),
            )
            .unwrap();
        }
        let sessions = list_opencode_sessions_in(&storage).unwrap();
        assert_eq!(sessions.len(), MAX_OPENCODE_SESSIONS);
        // The cap keeps the newest rows: the oldest (index 0) is the one cut.
        assert!(
            sessions
                .iter()
                .all(|session| session.session_id.as_deref() != Some("ses_0000"))
        );
        let _ = fs::remove_dir_all(storage);
    }

    #[test]
    fn opencode_session_assembly_orders_messages_and_attaches_parts() {
        let storage = std::env::temp_dir().join(format!(
            "qmux-import-opencode-assemble-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&storage);
        write_opencode_store(&storage);
        let metadata_path = storage
            .join("session")
            .join("global")
            .join("ses_alpha.json");

        let combined = read_opencode_session_in(&metadata_path, &storage).unwrap();
        let payload: Value = serde_json::from_str(&combined).unwrap();
        assert_eq!(payload["session"]["title"], "Tidy the changelog");
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        // Conversation order by stored creation time, not directory order.
        assert_eq!(messages[0]["id"], "msg_early");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["parts"][0]["text"],
            "trim the changelog to the last release"
        );
        let assistant_parts = messages[1]["parts"].as_array().unwrap();
        assert_eq!(
            assistant_parts
                .iter()
                .map(|part| part["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["step-start", "text", "tool"]
        );
        assert_eq!(assistant_parts[2]["tool"], "bash");

        // A session with no message directory refuses assembly.
        let empty = storage
            .join("session")
            .join("global")
            .join("ses_empty.json");
        fs::write(&empty, serde_json::json!({ "id": "ses_empty" }).to_string()).unwrap();
        let error = read_opencode_session_in(&empty, &storage).unwrap_err();
        assert!(error.contains("no messages"), "{error}");

        // A metadata path outside the storage root is refused by confinement.
        let outside = std::env::temp_dir().join(format!(
            "qmux-import-opencode-outside-{}.json",
            std::process::id()
        ));
        fs::write(
            &outside,
            serde_json::json!({ "id": "ses_alpha" }).to_string(),
        )
        .unwrap();
        let error = read_opencode_session_in(&outside, &storage).unwrap_err();
        assert!(error.contains("OpenCode sessions"), "{error}");

        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(storage);
    }

    #[test]
    fn transcript_import_reads_jsonl_and_refuses_other_extensions() {
        let dir = std::env::temp_dir().join(format!("qmux-import-read-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let transcript = dir.join("session.jsonl");
        fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(
            read_transcript_within(&transcript, &dir).unwrap(),
            "{\"type\":\"user\"}\n"
        );

        let zip = dir.join("export.zip");
        fs::write(&zip, "PK").unwrap();
        let error = read_transcript_within(&zip, &dir).unwrap_err();
        assert!(error.contains(".jsonl"), "{error}");

        let invalid = dir.join("invalid.jsonl");
        fs::write(&invalid, [0xff]).unwrap();
        let error = read_transcript_within(&invalid, &dir).unwrap_err();
        assert!(error.contains("UTF-8"), "{error}");

        let _ = fs::remove_dir_all(dir);
    }
}

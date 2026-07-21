use crate::adapters::{TranscriptLifecycleEvent, adapter_registry};
use crate::events::QmuxEvent;
use crate::state::{AgentSendSource, AppState};
use crate::turn_queue::{IdleResolution, advance_after_interruption};
use crate::workspace::{AgentInfo, AgentStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub role: String,
    pub blocks: Vec<TurnBlock>,
    pub source_index: usize,
    /// Milliseconds since the Unix epoch when the native transcript recorded
    /// this turn; None for adapters or records without time data (and for
    /// transcripts persisted before the field existed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TurnStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<TurnStatusReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_message_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Superseded,
    Interrupted,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatusReason {
    CodexRollback,
    Interrupted,
    ClaudePromptBranch,
    UnknownBranch,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum TurnBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: Option<String>,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: Option<String>,
        content: Value,
        is_error: bool,
    },
    Raw {
        value: Value,
    },
}

pub fn start_transcript_tail(
    state: AppState,
    agent_id: String,
    transcript_path: String,
    adapter_id: String,
) {
    if let Err(err) = adapter_registry(state.config()).get(&adapter_id) {
        state.emit(QmuxEvent::new(
            "transcript.error",
            None,
            Some(agent_id),
            json!({ "error": err, "path": transcript_path, "adapterId": adapter_id }),
        ));
        return;
    }

    let should_start = state
        .mark_transcript_tail(&agent_id, &transcript_path)
        .unwrap_or(false);
    if !should_start {
        return;
    }

    thread::spawn(move || {
        let path = PathBuf::from(&transcript_path);
        // Incremental tail state: bytes of complete lines already consumed, and the
        // running absolute line index so parsed turns keep stable source indices as
        // the file grows. Reading only the appended tail each tick keeps steady
        // state O(new bytes) instead of re-reading and re-diffing the whole file.
        let mut consumed: u64 = 0;
        let mut line_index: usize = 0;
        let mut read_failures: u32 = 0;
        let mut notice_active = false;
        // The first successful read rebuilds the timeline from the whole file rather
        // than appending. Turn ids are `agent-<line index>`, so they collide across a
        // pane's transcripts; binding a new file (e.g. picking a past session) must
        // replace the agent's turns wholesale, or the dedup-by-id on the frontend
        // would keep the previously loaded transcript's turns.
        let mut first_read = true;
        // Whether this tail has ever read its bound file. Recovery only makes sense
        // for a file we were actually following that then vanished (a rotation); a
        // file that has never appeared is a freshly launched session still warming
        // up, and jumping to the newest existing JSONL would bind us to an unrelated
        // old session instead of the new one whose SessionStart just set this path.
        let mut have_read_bound_file = false;
        let mut raw_lines: Vec<String> = Vec::new();
        let mut raw_line_offset: usize = 0;
        let registry = adapter_registry(state.config());
        let adapter = match registry.get(&adapter_id) {
            Ok(adapter) => adapter,
            Err(err) => {
                state.emit(QmuxEvent::new(
                    "transcript.error",
                    None,
                    Some(agent_id),
                    json!({ "error": err, "path": transcript_path, "adapterId": adapter_id }),
                ));
                return;
            }
        };

        loop {
            // Stop once the agent has rotated to a different transcript file (resume,
            // compact, a fresh session) or has gone away entirely. Claude only ever
            // changes the path alongside a freshly started tail for the new file, so
            // this tail exiting leaves exactly one live tail rather than two racing on
            // the same agent. Without this the tail stays pinned to a now-dead file
            // and the timeline silently stops advancing while the agent runs on.
            // A poisoned model lock (the implicit Err case) is transient from this
            // thread's view, so it falls through and we keep polling rather than
            // tearing the tail down on a momentary failure.
            if let Ok(found) = state.agent(&agent_id) {
                let current = found.as_ref().map(|agent| agent.transcript_path.as_deref());
                // Also stop once the agent has been parked off its pane — its owning
                // pane was closed (or it was detached) but a queued turn keeps it around
                // for restart recovery. The session process is dead, so this file will
                // never grow again; a resume respawns a fresh tail. Without this the
                // tail polls the now-static/deleted file for the rest of the process.
                let parked = found
                    .as_ref()
                    .is_some_and(|agent| agent.orphaned_queue_pane_id.is_some());
                if parked || !tail_should_continue(current, &transcript_path) {
                    if notice_active {
                        state.emit(transcript_notice(&agent_id, &transcript_path, None));
                    }
                    state.clear_transcript_tail(&agent_id, &transcript_path);
                    return;
                }
            }

            let snapshot = match read_transcript_from(&path, consumed) {
                Ok(snapshot) => {
                    read_failures = 0;
                    have_read_bound_file = true;
                    if notice_active {
                        notice_active = false;
                        state.emit(transcript_notice(&agent_id, &transcript_path, None));
                    }
                    snapshot
                }
                Err(err) => {
                    if should_recover_missing(err.kind(), have_read_bound_file)
                        && let Ok(Some(recovered_path)) = recover_missing_transcript(
                            &state,
                            &agent_id,
                            &transcript_path,
                            &path,
                            &adapter_id,
                        )
                    {
                        if notice_active {
                            state.emit(transcript_notice(&agent_id, &transcript_path, None));
                        }
                        state.clear_transcript_tail(&agent_id, &transcript_path);
                        start_transcript_tail(
                            state.clone(),
                            agent_id.clone(),
                            recovered_path,
                            adapter_id.clone(),
                        );
                        return;
                    }
                    // A single miss is normal while Claude is mid-write; a file that
                    // stays unreadable means the timeline has quietly stalled, so
                    // surface that once (cleared above when reads recover).
                    read_failures = read_failures.saturating_add(1);
                    if read_failures == READ_FAILURE_NOTICE_THRESHOLD && !notice_active {
                        notice_active = true;
                        state.emit(transcript_notice(
                            &agent_id,
                            &transcript_path,
                            Some("Transcript unavailable"),
                        ));
                    }
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            };
            if snapshot.reset || first_read {
                // Rebuild from the whole file: either this is the tail's first read of
                // a freshly bound transcript (which must replace any prior timeline), or
                // the file is now shorter than what we'd already consumed (a truncation
                // or in-place rewrite) so our timeline no longer prefixes it.
                raw_lines = complete_lines(&snapshot.data);
                raw_line_offset = snapshot.start_line_index;
                let turns =
                    adapter.resolve_transcript_turns(&agent_id, raw_line_offset, &raw_lines);
                // Bound-checked write: a rebind can land between this tail's
                // loop-top binding check and here, and an unconditional replace
                // would swap the new transcript's timeline for this dead file's
                // parse. A skipped write emits nothing; the next loop-top check
                // retires this tail.
                match state.replace_turns_for_transcript(&agent_id, &transcript_path, turns.clone())
                {
                    Err(err) => {
                        state.emit(transcript_persist_error(&agent_id, &transcript_path, &err));
                    }
                    Ok(true) => {
                        state.emit(QmuxEvent::new(
                            "turn.updated",
                            None,
                            Some(agent_id.clone()),
                            json!({ "reset": true, "turns": turns }),
                        ));
                    }
                    Ok(false) => {}
                }
                line_index = raw_line_offset + raw_lines.len();
                consumed = snapshot.consumed_bytes;
            } else {
                // Steady state: parse only the complete lines that arrived since the
                // last tick. line_index advances for every complete line (parsed or
                // not) so source indices stay aligned with the file's line numbers.
                let lines = complete_lines(&snapshot.data);
                let should_refresh_turns = lines
                    .iter()
                    .any(|line| adapter.transcript_line_can_update_turn_status(line));
                if should_refresh_turns {
                    raw_lines.extend(lines.iter().cloned());
                    trim_transcript_window(&mut raw_lines, &mut raw_line_offset);
                    let turns =
                        adapter.resolve_transcript_turns(&agent_id, raw_line_offset, &raw_lines);
                    match state.replace_turns_for_transcript(
                        &agent_id,
                        &transcript_path,
                        turns.clone(),
                    ) {
                        Err(err) => {
                            state.emit(transcript_persist_error(&agent_id, &transcript_path, &err));
                        }
                        Ok(true) => {
                            state.emit(QmuxEvent::new(
                                "turn.updated",
                                None,
                                Some(agent_id.clone()),
                                json!({ "reset": true, "turns": turns }),
                            ));
                        }
                        Ok(false) => {}
                    }
                }
                for line in lines {
                    let lifecycle_event = adapter.parse_transcript_lifecycle_event(&line);
                    if !should_refresh_turns
                        && let Some(turn) =
                            adapter.parse_transcript_line(&agent_id, line_index, &line)
                    {
                        // Surface a persistence failure rather than silently emitting a
                        // turn the store never recorded, which would drift the UI
                        // timeline from recovered state. A write skipped because the
                        // agent rebound mid-poll emits nothing.
                        match state.append_turn_for_transcript(turn.clone(), &transcript_path) {
                            Err(err) => {
                                state.emit(transcript_persist_error(
                                    &agent_id,
                                    &transcript_path,
                                    &err,
                                ));
                            }
                            Ok(true) => {
                                state.emit(QmuxEvent::new(
                                    "turn.appended",
                                    None,
                                    Some(agent_id.clone()),
                                    json!({ "turn": turn }),
                                ));
                            }
                            Ok(false) => {}
                        }
                    }
                    if !should_refresh_turns {
                        raw_lines.push(line.to_string());
                    }
                    if let Some(lifecycle_event) = lifecycle_event {
                        match transcript_lifecycle_agent_event(
                            &state,
                            &agent_id,
                            &transcript_path,
                            lifecycle_event,
                        ) {
                            Ok(Some(event)) => state.emit(event),
                            Ok(None) => {}
                            Err(err) => {
                                state.emit(transcript_persist_error(
                                    &agent_id,
                                    &transcript_path,
                                    &err,
                                ));
                            }
                        }
                    }
                    line_index += 1;
                }
                if !should_refresh_turns {
                    trim_transcript_window(&mut raw_lines, &mut raw_line_offset);
                }
                consumed += snapshot.consumed_bytes;
            }
            first_read = false;

            thread::sleep(Duration::from_millis(350));
        }
    });
}

/// Consecutive failed reads (at 500ms each, ~3s) before the bound transcript file
/// being unreadable is surfaced as an unexpected state rather than a write race.
const READ_FAILURE_NOTICE_THRESHOLD: u32 = 6;
// The UI/state retain at most 200 parsed turns. Keep substantially more raw records
// for branch/rollback resolution, but never retain or read an unbounded transcript.
const TRANSCRIPT_TAIL_LINE_LIMIT: usize = 20_000;
const TRANSCRIPT_TAIL_BYTE_LIMIT: u64 = 16 * 1024 * 1024;

fn trim_transcript_window(lines: &mut Vec<String>, source_index_offset: &mut usize) {
    let overflow = lines.len().saturating_sub(TRANSCRIPT_TAIL_LINE_LIMIT);
    if overflow > 0 {
        lines.drain(..overflow);
        *source_index_offset += overflow;
    }
}

fn transcript_lifecycle_agent_event(
    state: &AppState,
    agent_id: &str,
    transcript_path: &str,
    lifecycle_event: TranscriptLifecycleEvent,
) -> Result<Option<QmuxEvent>, String> {
    let Some(agent) = state.agent(agent_id)? else {
        return Ok(None);
    };
    if lifecycle_event == TranscriptLifecycleEvent::TurnStarted {
        // Codex can append the replacement turn immediately after an abort while its
        // UserPromptSubmit hook races the 350ms transcript poll. Only revive the
        // interruption state we set ourselves: a delayed task_started from a turn whose
        // Stop hook already reached Done must not resurrect completed work.
        if !matches!(agent.status, AgentStatus::AwaitingInput) {
            return Ok(None);
        }
        state.set_agent_status(agent_id, AgentStatus::Running)?;
        return transcript_lifecycle_updated_agent_event(
            state,
            agent_id,
            transcript_path,
            lifecycle_event,
            "agent.running",
        );
    }
    if !matches!(agent.status, AgentStatus::Starting | AgentStatus::Running) {
        return Ok(None);
    }
    // If a normal Stop/idle hook already drained a queued turn, a late transcript
    // abort marker belongs to the previous turn. Do not drain again while that
    // queued send is still waiting for its prompt-submit echo.
    if state.agent_has_outstanding_send_source(agent_id, AgentSendSource::QueuedTurn)? {
        return Ok(None);
    }

    match advance_after_interruption(state, agent_id) {
        Ok(IdleResolution::Drained) => transcript_lifecycle_updated_agent_event(
            state,
            agent_id,
            transcript_path,
            lifecycle_event,
            "agent.running",
        ),
        Ok(IdleResolution::Paused | IdleResolution::Idle) => {
            transcript_lifecycle_updated_agent_event(
                state,
                agent_id,
                transcript_path,
                lifecycle_event,
                "agent.interrupted",
            )
        }
        Err(err) => Ok(Some(QmuxEvent::new(
            "agent.queue_error",
            agent.pane_id,
            Some(agent_id.to_string()),
            json!({
                "error": err,
                "transcriptLifecycleEvent": lifecycle_event.as_str(),
                "transcriptPath": transcript_path,
            }),
        ))),
    }
}

fn transcript_lifecycle_updated_agent_event(
    state: &AppState,
    agent_id: &str,
    transcript_path: &str,
    lifecycle_event: TranscriptLifecycleEvent,
    event_type: &str,
) -> Result<Option<QmuxEvent>, String> {
    let Some(agent) = state.agent(agent_id)? else {
        return Ok(None);
    };
    Ok(Some(QmuxEvent::new(
        event_type,
        agent.pane_id.clone(),
        Some(agent.id.clone()),
        json!({
            "agent": agent,
            "transcriptLifecycleEvent": lifecycle_event.as_str(),
            "transcriptPath": transcript_path,
        }),
    )))
}

fn recover_missing_transcript(
    state: &AppState,
    agent_id: &str,
    bound_path: &str,
    missing_path: &Path,
    adapter_id: &str,
) -> Result<Option<String>, String> {
    if adapter_id != "claude" {
        return Ok(None);
    }

    let Some(dir) = missing_path.parent() else {
        return Ok(None);
    };
    let candidates = gather_transcript_candidates(dir)?;
    // Never recover onto a file another agent is already tailing — in a shared
    // project directory the newest JSONL by mtime is frequently a sibling agent's
    // live session, which would silently bind this agent to the wrong transcript.
    let excluded = other_agent_transcript_paths(state, agent_id);
    let Some(candidate) = select_newest_transcript_candidate(&candidates, &excluded, bound_path)
    else {
        return Ok(None);
    };
    let recovered_path = candidate.path.display().to_string();
    if recovered_path == bound_path {
        return Ok(None);
    }

    // Apply the rebind as a field-scoped mutation under the model lock rather than
    // a whole-struct `update_agent` from a snapshot read outside it. Between such a
    // read and the write, another thread can set this agent's status or session_id
    // (an idle Stop hook, or a SessionStart on the control-socket thread) — and
    // writing the stale snapshot back would revert those fields, losing the real
    // session id (breaking fork/resume) or leaving the agent stuck Running with its
    // queue undrained. Re-check the binding inside the closure so recovery only
    // fires while the agent is still pointing at the now-missing path.
    let applied = std::cell::Cell::new(false);
    let Some(agent) = state.mutate_agent(agent_id, |agent| {
        if agent.transcript_path.as_deref() == Some(bound_path) {
            agent.session_id = candidate.session_id.clone();
            agent.transcript_path = Some(recovered_path.clone());
            applied.set(true);
        }
    })?
    else {
        return Ok(None);
    };
    if !applied.get() {
        // The binding changed under us between deciding to recover and taking the
        // lock; leave whoever rebound it in charge.
        return Ok(None);
    }
    state.emit(QmuxEvent::new(
        "agent.transcript_recovered",
        agent.pane_id.clone(),
        Some(agent.id.clone()),
        json!({
            "agent": agent,
            "missingPath": bound_path,
            "transcriptPath": recovered_path,
        }),
    ));

    Ok(Some(recovered_path))
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptCandidate {
    pub(crate) path: PathBuf,
    pub(crate) modified: SystemTime,
    pub(crate) session_id: Option<String>,
}

/// All `*.jsonl` transcript files in `dir`, each paired with its mtime and the
/// session id read from its filename. Shared by auto-recovery and the manual
/// session picker so both reason over the same candidate set.
fn gather_transcript_candidates(dir: &Path) -> Result<Vec<TranscriptCandidate>, String> {
    gather_transcript_candidates_in(dir, false)
}

pub(crate) fn gather_transcript_candidates_recursive(
    dir: &Path,
) -> Result<Vec<TranscriptCandidate>, String> {
    gather_transcript_candidates_in(dir, true)
}

fn gather_transcript_candidates_in(
    dir: &Path,
    recursive: bool,
) -> Result<Vec<TranscriptCandidate>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "failed to inspect transcript directory {}: {err}",
                dir.display()
            ));
        }
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        // DirectoryEntry::metadata follows symlinks. Recursing on that result lets a
        // link cycle overflow the stack and lets a link under the global Codex session
        // root pull unrelated filesystem trees into the picker scan. Classify with
        // lstat-style file_type instead and ignore links entirely.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if recursive {
                candidates.extend(gather_transcript_candidates_in(&path, true)?);
            }
            continue;
        }
        if !file_type.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        candidates.push(TranscriptCandidate {
            session_id: session_id_from_transcript_path(&path),
            path,
            modified,
        });
    }

    Ok(candidates)
}

/// Transcript paths bound to agents other than `agent_id`, so recovery can avoid
/// stealing a sibling agent's live session.
fn other_agent_transcript_paths(state: &AppState, agent_id: &str) -> HashSet<String> {
    state
        .list_agents()
        .unwrap_or_default()
        .into_iter()
        .filter(|agent| agent.id != agent_id)
        .filter_map(|agent| agent.transcript_path)
        .collect()
}

/// Newest candidate by mtime, ignoring the now-missing bound path and any file
/// another agent is tailing. Path is a stable tiebreaker for equal mtimes.
fn select_newest_transcript_candidate(
    candidates: &[TranscriptCandidate],
    excluded: &HashSet<String>,
    bound_path: &str,
) -> Option<TranscriptCandidate> {
    candidates
        .iter()
        .filter(|candidate| {
            let path = candidate.path.display().to_string();
            path != bound_path && !excluded.contains(&path)
        })
        .max_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then(left.path.cmp(&right.path))
        })
        .cloned()
}

/// Cap on how many sessions the picker offers, newest first — old projects can
/// accumulate hundreds of JSONL files and the user only ever wants a recent one.
const MAX_TRANSCRIPT_OPTIONS: usize = 30;

/// Characters of the first usable user message shown as a session preview.
const PREVIEW_MAX_CHARS: usize = 90;
const PREVIEW_USER_MESSAGE_LOOKAHEAD_LIMIT: usize = 5;
// Session previews never need to parse an enormous tool-result record. Keep the
// picker memory bounded even when a transcript contains a single pathological line.
const TRANSCRIPT_META_LINE_LIMIT: u64 = 1024 * 1024;

/// One selectable session for the right pane's transcript picker.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptOption {
    pub path: String,
    pub session_id: Option<String>,
    pub modified_ms: u128,
    pub preview: Option<String>,
    pub line_count: usize,
    /// This is the transcript the agent is currently bound to.
    pub is_active: bool,
    /// Another agent is tailing this file; selecting it would collide.
    pub bound_to_other_agent: bool,
}

fn transcript_listing_root(agent: &AgentInfo, current_path: &Path) -> Option<PathBuf> {
    if agent.adapter == "codex" {
        codex_sessions_root(current_path)
    } else if agent.adapter == "grok" && is_grok_native_transcript(current_path) {
        grok_session_group_root(current_path)
    } else {
        current_path.parent().map(Path::to_path_buf)
    }
}

fn is_grok_native_transcript(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("chat_history.jsonl")
}

fn grok_session_group_root(path: &Path) -> Option<PathBuf> {
    path.parent()?.parent().map(Path::to_path_buf)
}

fn codex_sessions_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some("sessions"))
        .map(Path::to_path_buf)
}

fn transcript_session_id(
    agent: &AgentInfo,
    path: &Path,
    fallback: Option<String>,
) -> Option<String> {
    if agent.adapter == "codex" {
        return codex_transcript_session_id(path).or(fallback);
    }
    if agent.adapter == "grok" && is_grok_native_transcript(path) {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(ToString::to_string)
            .or(fallback);
    }
    fallback
}

pub(crate) fn codex_transcript_session_id(path: &Path) -> Option<String> {
    read_codex_transcript_session_id(path).ok().flatten()
}

pub(crate) fn read_codex_transcript_session_id(path: &Path) -> Result<Option<String>, String> {
    let file = fs::File::open(path)
        .map_err(|err| format!("failed to open Codex transcript {}: {err}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    let bytes = reader
        .read_line(&mut first)
        .map_err(|err| format!("failed to read Codex transcript {}: {err}", path.display()))?;
    if bytes == 0 {
        return Ok(None);
    }

    let terminated = first.ends_with('\n');
    let first = first.trim_end_matches(['\n', '\r']);
    let value = match serde_json::from_str::<Value>(first) {
        Ok(value) => value,
        Err(_) if !terminated => return Ok(None),
        Err(err) => {
            return Err(format!(
                "Codex transcript {} does not start with valid JSON: {err}",
                path.display()
            ));
        }
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Err(format!(
            "Codex transcript {} does not start with session_meta",
            path.display()
        ));
    }
    Ok(value
        .get("payload")
        .and_then(|payload| string_field(payload, "id")))
}

/// The working directory recorded in a Codex rollout's leading `session_meta`
/// line. Codex stores every project's sessions in one global tree (unlike Claude's
/// per-project session directories), so this is how the picker scopes its listing
/// to the current session's project. Best-effort: an unreadable file, an empty
/// file, or a first line that isn't a `session_meta` with a `cwd` yields `None`.
pub(crate) fn codex_transcript_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    if reader.read_line(&mut first).ok()? == 0 {
        return None;
    }
    let first = first.trim_end_matches(['\n', '\r']);
    let value = serde_json::from_str::<Value>(first).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    value
        .get("payload")
        .and_then(|payload| string_field(payload, "cwd"))
}

pub(crate) fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Sessions in the agent's transcript directory, newest first, for the manual
/// picker. Empty when the agent has no transcript path yet (nothing to scan).
pub fn list_agent_transcripts(
    state: &AppState,
    agent_id: &str,
) -> Result<Vec<TranscriptOption>, String> {
    let Some(agent) = state.agent(agent_id)? else {
        return Ok(Vec::new());
    };
    // Legacy OpenCode integration used one combined `<agent>.jsonl` file. It has
    // no safe session boundaries, so keep the picker hidden until SessionStart
    // rotates the binding into `<agent>/<session>.jsonl`.
    if agent.adapter == "opencode"
        && agent
            .transcript_path
            .as_deref()
            .and_then(|path| Path::new(path).parent())
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some(agent.id.as_str())
    {
        return Ok(Vec::new());
    }
    let Some(current_path) = agent.transcript_path.clone() else {
        return Ok(Vec::new());
    };
    let current = Path::new(&current_path);
    let Some(dir) = transcript_listing_root(&agent, current) else {
        return Ok(Vec::new());
    };

    let mut candidates = if agent.adapter == "codex" {
        let mut candidates = gather_transcript_candidates_recursive(&dir)?;
        // Codex keeps every project's rollouts in one global `sessions` tree, so the
        // recursive scan above sees sessions from unrelated directories. Scope the
        // picker to the project the current session ran in — its `session_meta` cwd —
        // so it lists only same-project sessions, matching Claude's naturally
        // per-project listing. If the active rollout's cwd can't be read, fall back to
        // the unfiltered list rather than hiding everything.
        if let Some(project_cwd) = codex_transcript_cwd(current) {
            candidates.retain(|candidate| {
                candidate.path.as_path() == current
                    || codex_transcript_cwd(&candidate.path).as_deref()
                        == Some(project_cwd.as_str())
            });
        }
        candidates
    } else if agent.adapter == "grok" && is_grok_native_transcript(current) {
        let mut candidates = gather_transcript_candidates_recursive(&dir)?;
        candidates.retain(|candidate| is_grok_native_transcript(&candidate.path));
        candidates
    } else {
        gather_transcript_candidates(&dir)?
    };
    candidates.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then(left.path.cmp(&right.path))
    });

    let other = other_agent_transcript_paths(state, agent_id);
    let options = candidates
        .into_iter()
        .take(MAX_TRANSCRIPT_OPTIONS)
        .map(|candidate| {
            let path = candidate.path.display().to_string();
            let (preview, line_count) = read_transcript_meta(&candidate.path);
            TranscriptOption {
                is_active: path == current_path,
                bound_to_other_agent: other.contains(&path),
                modified_ms: candidate
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .map(|since| since.as_millis())
                    .unwrap_or(0),
                session_id: transcript_session_id(&agent, &candidate.path, candidate.session_id),
                preview,
                line_count,
                path,
            }
        })
        .collect();

    Ok(options)
}

/// Repoints an agent at `path` and restarts its tail there, or clears the current
/// binding when `path` is `None`. The old tail stops itself once it sees the agent
/// no longer pointing at its file.
pub fn set_agent_transcript(
    state: &AppState,
    agent_id: &str,
    path: Option<&str>,
) -> Result<AgentInfo, String> {
    let Some(mut agent) = state.agent(agent_id)? else {
        return Err(format!("agent {agent_id} not found"));
    };
    let Some(path) = path else {
        agent.session_id = None;
        agent.transcript_path = None;
        state.update_agent(agent.clone())?;
        return Ok(agent);
    };

    let candidate = Path::new(path);
    if candidate
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("jsonl")
    {
        return Err("transcript must be a .jsonl file".to_string());
    }
    if !candidate.is_file() {
        return Err(format!("transcript {path} does not exist"));
    }
    // Confinement needs a reference directory. A repoint is only ever offered by
    // the session picker, which scans the directory of the agent's *current*
    // transcript (`transcript_listing_root`) — so with no current transcript
    // there is no legitimate source directory. Refuse instead of binding an
    // arbitrary `.jsonl`: otherwise a caller (e.g. a compromised webview) could
    // `set_agent_transcript(id, null)` to clear the binding and then bind any
    // `.jsonl` on disk, turning this into an unconfined transcript-read
    // primitive over sessions from unrelated projects. qmux discovers the
    // initial transcript itself via the adapter's SessionStart hook.
    let Some(current) = agent.transcript_path.as_deref() else {
        return Err("cannot repoint a transcript before this agent has an active one".to_string());
    };
    let current = Path::new(current);
    if agent.adapter == "codex" {
        let Some(root) = codex_sessions_root(current) else {
            return Err("transcript is outside the agent's session directory".to_string());
        };
        let root = root.canonicalize().map_err(|err| {
            format!(
                "failed to resolve transcript session directory {}: {err}",
                root.display()
            )
        })?;
        let candidate_root = candidate
            .canonicalize()
            .map_err(|err| format!("failed to resolve transcript {path}: {err}"))?;
        if !candidate_root.starts_with(root) {
            return Err("transcript is outside the agent's session directory".to_string());
        }
        // Mirror the picker's project scoping: a Codex rollout from a different
        // project (a different `session_meta` cwd) must not be bound here, even
        // though it shares the global sessions root. Lenient when either cwd can't
        // be read, so an unparseable rollout still binds rather than hard-failing.
        if let Some(project_cwd) = codex_transcript_cwd(current)
            && let Some(candidate_cwd) = codex_transcript_cwd(candidate)
            && project_cwd != candidate_cwd
        {
            return Err("transcript belongs to a different project".to_string());
        }
    } else if agent.adapter == "grok" && is_grok_native_transcript(current) {
        validate_grok_transcript_candidate(current, candidate)?;
    } else if current.parent() != candidate.parent() {
        return Err("transcript is outside the agent's session directory".to_string());
    }

    let already_bound = agent.transcript_path.as_deref() == Some(path);
    agent.session_id = transcript_session_id(
        &agent,
        candidate,
        session_id_from_transcript_path(candidate),
    );
    agent.transcript_path = Some(path.to_string());
    state.update_agent(agent.clone())?;
    // Clear any recovery/ambiguity notice tied to the previous binding.
    state.emit(transcript_notice(agent_id, path, None));
    if !already_bound {
        start_transcript_tail(
            state.clone(),
            agent_id.to_string(),
            path.to_string(),
            agent.adapter.clone(),
        );
    }

    Ok(agent)
}

fn validate_grok_transcript_candidate(current: &Path, candidate: &Path) -> Result<(), String> {
    if !is_grok_native_transcript(candidate) {
        return Err("Grok transcript must be a chat_history.jsonl file".to_string());
    }
    let root = grok_session_group_root(current)
        .ok_or_else(|| "transcript is outside the agent's session directory".to_string())?
        .canonicalize()
        .map_err(|err| format!("failed to resolve Grok session directory: {err}"))?;
    let candidate = candidate
        .canonicalize()
        .map_err(|err| format!("failed to resolve Grok transcript: {err}"))?;
    let candidate_group = candidate
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "transcript is outside the agent's session directory".to_string())?;
    if candidate_group != root {
        return Err("transcript is outside the agent's session directory".to_string());
    }
    Ok(())
}

/// Reads a transcript's first usable user-message preview and total line count with
/// bounded memory — best-effort, so an unreadable file yields `(None, 0)`. Oversized
/// records still count as lines but are not parsed for previews.
pub(crate) fn read_transcript_meta(path: &Path) -> (Option<String>, usize) {
    let Ok(file) = fs::File::open(path) else {
        return (None, 0);
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut line_count = 0;
    let mut preview = None;
    let mut user_messages_seen = 0;
    loop {
        line.clear();
        let read = match reader
            .by_ref()
            .take(TRANSCRIPT_META_LINE_LIMIT + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(read) => read,
            Err(_) => break,
        };
        if read == 0 {
            break;
        }
        let complete = line.last() == Some(&b'\n');
        let oversized = !complete && line.len() as u64 > TRANSCRIPT_META_LINE_LIMIT;
        if oversized && discard_through_newline(&mut reader).is_err() {
            break;
        }
        line_count += 1;
        if oversized
            || preview.is_some()
            || user_messages_seen >= PREVIEW_USER_MESSAGE_LOOKAHEAD_LIMIT
        {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("synthetic_reason").is_some() {
            continue;
        }
        let message = transcript_message_value(&value);
        let is_user = value.get("type").and_then(Value::as_str) == Some("user")
            || message
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("user");
        if !is_user {
            continue;
        }
        if let Some(text) = first_text_block(message.and_then(|message| message.get("content"))) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                user_messages_seen += 1;
                if !is_tagged_user_instruction(&text) {
                    preview = Some(truncate_preview(trimmed));
                }
            }
        }
    }
    (preview, line_count)
}

fn discard_through_newline(reader: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        if let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            return Ok(());
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

fn transcript_message_value(value: &Value) -> Option<&Value> {
    if value.get("type").and_then(Value::as_str) == Some("response_item") {
        return value
            .get("payload")
            .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("message"));
    }
    Some(value.get("message").unwrap_or(value))
}

/// First textual content of a message: the string itself, or the first text block
/// of a content array. Ignores tool results and other non-text blocks.
fn first_text_block(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            blocks
                .iter()
                .find_map(|block| match block.get("type").and_then(Value::as_str) {
                    Some("text" | "input_text" | "output_text") => block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                })
        }
        _ => None,
    }
}

fn truncate_preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_MAX_CHARS {
        return collapsed;
    }
    let head: String = collapsed.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{head}…")
}

/// Strips qmux-injected tagged instruction blocks from the front of a user
/// message, mirroring `stripLeadingTaggedInstructionBlocks` in
/// src/lib/taggedInstructions.ts: repeated `# `-labelled prefix lines and
/// line-structured `<tag>…</tag>` blocks (depth-matched) are removed until
/// real content is reached. Returns `None` when nothing but instruction
/// blocks remain. Narrower than the frontend on purpose — only whole-line
/// tags are recognized, so a message merely *containing* markup is kept
/// rather than over-stripped.
pub(crate) fn strip_leading_tagged_instruction_blocks(text: &str) -> Option<&str> {
    let mut current = text;
    let mut removed = false;
    loop {
        let Some(content_start) = tagged_instruction_content_start(current) else {
            // Only `# ` label lines or blank lines remain. If blocks were
            // stripped, those labels belonged to them; otherwise the message
            // was headings all along and stays as it is.
            return if removed { None } else { Some(current) };
        };
        let content = &current[content_start..];
        let first_line_end = content.find('\n').unwrap_or(content.len());
        let first_line =
            trim_horizontal_whitespace(strip_trailing_carriage_return(&content[..first_line_end]));
        if parse_inline_tag(first_line).is_some() {
            let cut = content_start + first_line_end;
            current = current.get(cut + 1..).unwrap_or("");
            removed = true;
            continue;
        }
        let Some(opening_tag) = parse_opening_tag(first_line) else {
            // Real content: keep the remainder as-is, including any `# ` lines
            // before it — headings are content when no block follows them.
            return Some(current);
        };
        let mut depth = 1usize;
        let mut cursor = content_start + first_line_end;
        let mut block_end = None;
        while cursor < current.len() {
            let line_start = cursor + 1;
            let line_end = current[line_start..]
                .find('\n')
                .map(|index| line_start + index)
                .unwrap_or(current.len());
            let line = trim_horizontal_whitespace(strip_trailing_carriage_return(
                &current[line_start..line_end],
            ));
            if parse_opening_tag(line) == Some(opening_tag) {
                depth += 1;
            } else if parse_closing_tag(line) == Some(opening_tag) {
                depth -= 1;
                if depth == 0 {
                    block_end = Some(line_end);
                    break;
                }
            }
            cursor = line_end;
        }
        // An unterminated block is content, not an instruction wrapper.
        let Some(block_end) = block_end else {
            return Some(current);
        };
        current = current.get(block_end + 1..).unwrap_or("");
        removed = true;
    }
}

pub(crate) fn is_tagged_user_instruction(text: &str) -> bool {
    let Some(content_start) = tagged_instruction_content_start(text) else {
        return false;
    };

    if is_inline_tagged_instruction_sequence(&text[content_start..]) {
        return true;
    }

    let content = &text[content_start..];
    let Some(first_line_end) = content.find('\n') else {
        return false;
    };
    let first_line =
        trim_horizontal_whitespace(strip_trailing_carriage_return(&content[..first_line_end]));
    let last_line_start = text.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let last_line =
        trim_horizontal_whitespace(strip_trailing_carriage_return(&text[last_line_start..]));
    let Some(opening_tag) = parse_opening_tag(first_line) else {
        return false;
    };
    parse_closing_tag(last_line) == Some(opening_tag)
}

fn is_inline_tagged_instruction_sequence(text: &str) -> bool {
    let mut saw_tag = false;
    for raw_line in text.split('\n') {
        let line = trim_horizontal_whitespace(strip_trailing_carriage_return(raw_line));
        if line.is_empty() {
            continue;
        }
        if parse_inline_tag(line).is_none() {
            return false;
        }
        saw_tag = true;
    }
    saw_tag
}

fn tagged_instruction_content_start(text: &str) -> Option<usize> {
    let mut start = 0;
    while start < text.len() {
        let line_end = text[start..]
            .find('\n')
            .map(|index| start + index)
            .unwrap_or(text.len());
        let line = strip_trailing_carriage_return(&text[start..line_end]);
        if !is_tagged_instruction_prefix_line(line) {
            return Some(start);
        }
        if line_end == text.len() {
            return None;
        }
        start = line_end + 1;
    }
    None
}

fn is_tagged_instruction_prefix_line(line: &str) -> bool {
    line.starts_with("# ") || trim_horizontal_whitespace(line).is_empty()
}

fn strip_trailing_carriage_return(value: &str) -> &str {
    value.strip_suffix('\r').unwrap_or(value)
}

fn trim_horizontal_whitespace(value: &str) -> &str {
    value.trim_matches(|char: char| char != '\n' && char != '\r' && char.is_whitespace())
}

fn parse_inline_tag(line: &str) -> Option<&str> {
    if line.len() < 7 || !line.starts_with('<') {
        return None;
    }
    let opening_end = line.find('>')?;
    if opening_end < 2 {
        return None;
    }
    let tag = &line[1..opening_end];
    if !is_instruction_tag_name(tag) {
        return None;
    }
    let closing = format!("</{tag}>");
    line.ends_with(&closing).then_some(tag)
}

fn parse_opening_tag(line: &str) -> Option<&str> {
    if line.len() < 3 || !line.starts_with('<') || !line.ends_with('>') {
        return None;
    }
    let tag = &line[1..line.len() - 1];
    is_instruction_tag_name(tag).then_some(tag)
}

fn parse_closing_tag(line: &str) -> Option<&str> {
    if line.len() < 4 || !line.starts_with("</") || !line.ends_with('>') {
        return None;
    }
    let tag = &line[2..line.len() - 1];
    is_instruction_tag_name(tag).then_some(tag)
}

fn is_instruction_tag_name(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || char == '_' || char == '-')
}

/// The session id a Claude/Grok transcript path encodes: transcripts are stored as
/// `<project dir>/<session-id>.jsonl`. Shared by the session picker and the Claude
/// adapter's fork stale-payload guard so the naming convention has one owner.
pub(crate) fn session_id_from_transcript_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty() && !stem.starts_with('.'))
        .map(ToString::to_string)
}

/// Builds a `transcript.notice` event carrying a short, user-facing message about
/// the tail's health. A `None` message clears any notice the UI is showing.
fn transcript_notice(agent_id: &str, path: &str, message: Option<&str>) -> QmuxEvent {
    QmuxEvent::new(
        "transcript.notice",
        None,
        Some(agent_id.to_string()),
        json!({ "message": message, "path": path }),
    )
}

/// Reports a failure to persist parsed turns (a poisoned state lock or full
/// disk) so the UI can show the timeline is no longer authoritative instead of
/// silently diverging from recovered state.
fn transcript_persist_error(agent_id: &str, path: &str, error: &str) -> QmuxEvent {
    QmuxEvent::new(
        "transcript.error",
        None,
        Some(agent_id.to_string()),
        json!({ "error": error, "path": path }),
    )
}

/// Returns the newline-terminated lines of a transcript snapshot, holding back
/// any trailing bytes after the final '\n'. A transcript record is one JSON
/// object per line ending in '\n', so content past the last newline is a record
/// still being written: parsing it would either be dropped as invalid JSON or,
/// once it completes, differ from the stored partial line and churn a full
/// timeline reset. Deferring it until its newline lands keeps the tail purely
/// append-driven.
fn complete_lines(raw: &str) -> Vec<String> {
    let complete = raw.rfind('\n').map_or("", |idx| &raw[..=idx]);
    complete.lines().map(ToString::to_string).collect()
}

/// Result of an incremental transcript read.
struct TranscriptRead {
    /// File content from the read offset, or the whole file when `reset` is set.
    data: String,
    /// Raw file bytes covered by `data`: the byte length of the newline-terminated
    /// prefix that was read. The tail offset must advance by this, not by
    /// `data.len()` — `from_utf8_lossy` can make `data` *longer* than the bytes read
    /// when a complete line contains an invalid byte (each becomes a 3-byte U+FFFD),
    /// and measuring the decoded string would overshoot the real file position and
    /// wedge the tail into a perpetual reset.
    consumed_bytes: u64,
    /// Absolute line index of the first returned record. Nonzero when an initial
    /// read/reset keeps only the bounded tail of a large file.
    start_line_index: usize,
    /// The file is now shorter than the requested offset (truncated or rewritten),
    /// so `data` holds the whole file and the caller must rebuild rather than append.
    reset: bool,
}

/// Reads a transcript incrementally: only the bytes appended past `offset`. When
/// the file has shrunk below `offset` it reads the whole file and flags a reset so
/// the caller rebuilds the timeline. `offset` is always a newline boundary, so the
/// read starts on a valid-UTF-8 boundary; the *end* may land mid-record, so the read
/// holds back any unterminated trailing bytes (see `read_complete_lines_utf8`).
fn read_transcript_from(path: &Path, offset: u64) -> std::io::Result<TranscriptRead> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    // A shrink below `offset` is an obvious truncation/rewrite. An in-place rewrite to
    // the same-or-greater length is subtler: `offset` is always a newline boundary, so
    // if the byte just before it is no longer '\n', the bytes up to `offset` changed and
    // appending from here would splice new content onto a stale timeline. Rebuild in
    // both cases. (This catches the common rewrite where line boundaries shift; a rewrite
    // that happens to keep a newline at exactly `offset - 1` still reads as an append.)
    let rewritten_in_place =
        offset > 0 && len >= offset && !byte_before_is_newline(&mut file, offset)?;
    if offset == 0
        || len < offset
        || rewritten_in_place
        || len.saturating_sub(offset) > TRANSCRIPT_TAIL_BYTE_LIMIT
    {
        let (data, consumed_bytes, start_line_index) = read_complete_tail_utf8(&mut file)?;
        return Ok(TranscriptRead {
            data,
            consumed_bytes,
            start_line_index,
            reset: offset > 0,
        });
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let (data, consumed_bytes) = read_complete_lines_utf8(&mut file)?;
    if data.bytes().filter(|byte| *byte == b'\n').count() > TRANSCRIPT_TAIL_LINE_LIMIT {
        let (data, consumed_bytes, start_line_index) = read_complete_tail_utf8(&mut file)?;
        return Ok(TranscriptRead {
            data,
            consumed_bytes,
            start_line_index,
            reset: true,
        });
    }
    Ok(TranscriptRead {
        data,
        consumed_bytes,
        start_line_index: 0,
        reset: false,
    })
}

/// Scans with a fixed buffer, then reads only the newest complete records that fit
/// both tail limits. The scan is O(file size) on first bind/reset but its memory is
/// fixed; normal polling remains incremental through `read_complete_lines_utf8`.
fn read_complete_tail_utf8(file: &mut fs::File) -> std::io::Result<(String, u64, usize)> {
    file.seek(SeekFrom::Start(0))?;
    let mut newline_ends = VecDeque::with_capacity(TRANSCRIPT_TAIL_LINE_LIMIT + 1);
    let mut total_lines = 0usize;
    let mut complete_end = 0u64;
    let mut absolute = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for (index, byte) in buffer[..read].iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let end = absolute + index as u64 + 1;
            complete_end = end;
            total_lines += 1;
            newline_ends.push_back(end);
            if newline_ends.len() > TRANSCRIPT_TAIL_LINE_LIMIT + 1 {
                newline_ends.pop_front();
            }
        }
        absolute += read as u64;
    }

    if complete_end == 0 {
        return Ok((String::new(), 0, 0));
    }

    let line_limited_start = if total_lines > TRANSCRIPT_TAIL_LINE_LIMIT {
        newline_ends.front().copied().unwrap_or(complete_end)
    } else {
        0
    };
    let minimum_byte_start = complete_end.saturating_sub(TRANSCRIPT_TAIL_BYTE_LIMIT);
    let byte_limited_start = if minimum_byte_start == 0 {
        0
    } else {
        newline_ends
            .iter()
            .copied()
            .find(|end| *end >= minimum_byte_start)
            .unwrap_or(complete_end)
    };
    let start = line_limited_start.max(byte_limited_start);
    let retained_lines = newline_ends.iter().filter(|end| **end > start).count();
    let start_line_index = total_lines.saturating_sub(retained_lines);

    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (complete_end - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        complete_end,
        start_line_index,
    ))
}

/// Reads the single byte at `offset - 1` to check the tail offset still lands just
/// after a newline. Caller guarantees `0 < offset <= len`, so the byte exists.
fn byte_before_is_newline(file: &mut fs::File, offset: u64) -> std::io::Result<bool> {
    file.seek(SeekFrom::Start(offset - 1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

/// Reads from the file's current position to EOF and returns the longest prefix that
/// ends on a newline, decoded as UTF-8. A transcript is appended one JSON line at a
/// time, so the bytes after the final '\n' are an in-progress record that may end in
/// the middle of a multi-byte UTF-8 character. `read_to_string` would reject the whole
/// read as invalid UTF-8 — surfacing a spurious "Transcript unavailable" until the
/// character completes — so instead we cut at the last newline (the unterminated tail
/// is what `complete_lines` discards anyway). Returns the decoded prefix together with
/// its raw byte length (`cut`): a complete line can still hold an invalid byte mid-way,
/// which `from_utf8_lossy` expands to a 3-byte U+FFFD, so the decoded string's length is
/// not a reliable file offset — the caller must advance by the raw byte count.
fn read_complete_lines_utf8(file: &mut fs::File) -> std::io::Result<(String, u64)> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let cut = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |idx| idx + 1);
    Ok((
        String::from_utf8_lossy(&bytes[..cut]).into_owned(),
        cut as u64,
    ))
}

/// Whether a tail bound to `bound_path` should keep running. `current` is the
/// agent's freshly looked-up transcript path: `Some(Some(path))` when the agent
/// exists with a path set, `Some(None)` when it exists with none, and `None` when
/// the agent is gone. The tail only continues while the agent is still pointing at
/// the exact file this tail was started for; any rotation or removal stops it.
fn tail_should_continue(current: Option<Option<&str>>, bound_path: &str) -> bool {
    matches!(current, Some(Some(path)) if path == bound_path)
}

/// Whether a failed read should recover onto a sibling transcript. Only a file we
/// have already followed and that has now vanished counts as a rotation worth
/// recovering. A never-seen file is a freshly launched session (e.g. typing
/// `claude` in the terminal) whose transcript hasn't hit disk yet — recovering
/// then would bind us to an unrelated existing session, so we keep waiting for the
/// real file the new session's SessionStart pointed us at.
fn should_recover_missing(err_kind: ErrorKind, have_read_bound_file: bool) -> bool {
    err_kind == ErrorKind::NotFound && have_read_bound_file
}

/// Parses an RFC3339 timestamp ("2026-07-17T09:30:12.345Z", offset forms
/// included) to milliseconds since the Unix epoch. Hand-rolled so transcript
/// parsing stays dependency-free; anything unparseable yields None.
pub(crate) fn rfc3339_to_epoch_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        slice.parse::<i64>().ok()
    };
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(&b'T' | &b't' | &b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Leap seconds ("…:60Z") clamp to the following second rather than failing.
    let second = second.min(60);

    let mut index = 19;
    let mut millis = 0i64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start {
            return None;
        }
        // Millisecond precision: pad short fractions, truncate long ones.
        let fraction = &text[start..index.min(start + 3)];
        let mut value = fraction.parse::<i64>().ok()?;
        for _ in fraction.len()..3 {
            value *= 10;
        }
        millis = value;
    }
    let offset_minutes = match bytes.get(index) {
        Some(&b'Z' | &b'z') if index + 1 == bytes.len() => 0,
        Some(sign @ (&b'+' | &b'-')) => {
            if bytes.get(index + 3) != Some(&b':') || index + 6 != bytes.len() {
                return None;
            }
            let hours = digits(index + 1..index + 3)?;
            let minutes = digits(index + 4..index + 6)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            (if *sign == b'+' { 1 } else { -1 }) * (hours * 60 + minutes)
        }
        _ => return None,
    };

    // Days-from-civil (Howard Hinnant): Gregorian date to days since the epoch.
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

#[cfg(test)]
mod timestamp_tests {
    use super::rfc3339_to_epoch_ms;

    #[test]
    fn parses_utc_with_and_without_fraction() {
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00.250Z"), Some(250));
        // 2026-07-17T09:30:12.345Z, cross-checked against `date -d ... +%s`.
        assert_eq!(
            rfc3339_to_epoch_ms("2026-07-17T09:30:12.345Z"),
            Some(1_784_280_612_345)
        );
        // Fractions pad and truncate to millisecond precision.
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(
            rfc3339_to_epoch_ms("1970-01-01T00:00:00.123456Z"),
            Some(123)
        );
    }

    #[test]
    fn honors_numeric_offsets() {
        assert_eq!(
            rfc3339_to_epoch_ms("1970-01-01T02:00:00+02:00"),
            Some(0),
            "positive offsets subtract"
        );
        assert_eq!(
            rfc3339_to_epoch_ms("1969-12-31T19:00:00-05:00"),
            Some(0),
            "negative offsets add"
        );
    }

    #[test]
    fn rejects_malformed_inputs() {
        for input in [
            "",
            "not a date",
            "2026-07-17",
            "2026-07-17T09:30",
            "2026-07-17T09:30:12",
            "2026-13-01T00:00:00Z",
            "2026-07-17T09:30:12.Z",
            "2026-07-17T09:30:12+0200",
            "2026-07-17T09:30:12Zextra",
        ] {
            assert_eq!(rfc3339_to_epoch_ms(input), None, "input: {input:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig, QmuxConfig,
    };
    use std::time::UNIX_EPOCH;

    #[test]
    fn tail_continues_only_while_bound_to_the_same_path() {
        // Agent still pointing at this tail's file: keep tailing.
        assert!(tail_should_continue(Some(Some("/t/a.jsonl")), "/t/a.jsonl"));
        // Rotated to a new transcript (resume/compact/new session): stop.
        assert!(!tail_should_continue(
            Some(Some("/t/b.jsonl")),
            "/t/a.jsonl"
        ));
        // Path cleared while the agent lives: stop.
        assert!(!tail_should_continue(Some(None), "/t/a.jsonl"));
        // Agent gone entirely: stop.
        assert!(!tail_should_continue(None, "/t/a.jsonl"));
    }

    #[test]
    fn complete_lines_holds_back_an_unterminated_trailing_record() {
        // Fully terminated snapshot: every record is stable.
        assert_eq!(
            complete_lines("{\"a\":1}\n{\"b\":2}\n"),
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]
        );
        // A record still being written (no trailing newline) is withheld until
        // its newline lands, so it is never parsed as a partial line.
        assert_eq!(
            complete_lines("{\"a\":1}\n{\"b\":2"),
            vec!["{\"a\":1}".to_string()]
        );
        // Once the newline arrives the previously-partial record becomes stable,
        // appended after the line already seen (no reset churn).
        assert_eq!(
            complete_lines("{\"a\":1}\n{\"b\":2}\n"),
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]
        );
        // A snapshot with no complete line yet yields nothing.
        assert!(complete_lines("{\"partial").is_empty());
        assert!(complete_lines("").is_empty());
    }

    #[test]
    fn recovery_waits_for_a_fresh_session_file_to_appear() {
        // A file we followed that then vanished is a rotation: recover to a sibling.
        assert!(should_recover_missing(ErrorKind::NotFound, true));
        // A never-seen file is a fresh session warming up: keep waiting, don't bind
        // to a pre-existing session in the same folder.
        assert!(!should_recover_missing(ErrorKind::NotFound, false));
        // Other read errors (permissions, mid-write races) never trigger recovery.
        assert!(!should_recover_missing(ErrorKind::PermissionDenied, true));
    }

    #[test]
    fn read_reports_only_the_newline_terminated_prefix_as_consumed() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-consumed-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        // Two complete records plus an unterminated third: only the newline-terminated
        // prefix counts as consumed, so the partial tail is picked up on a later read.
        fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3").unwrap();
        let read = read_transcript_from(&path, 0).unwrap();
        assert_eq!(read.data, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(read.consumed_bytes, 16);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transcript_lifecycle_interruption_marks_running_agent_awaiting_input() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::Running))
            .unwrap();

        let event = transcript_lifecycle_agent_event(
            &state,
            "agent-1",
            "/tmp/session.jsonl",
            TranscriptLifecycleEvent::Interrupted,
        )
        .unwrap()
        .expect("interruption should emit an agent event");

        assert_eq!(event.event_type, "agent.interrupted");
        assert_eq!(event.payload["transcriptLifecycleEvent"], "interrupted");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::AwaitingInput));
    }

    #[test]
    fn transcript_turn_start_restores_running_after_interruption() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::AwaitingInput))
            .unwrap();

        let event = transcript_lifecycle_agent_event(
            &state,
            "agent-1",
            "/tmp/session.jsonl",
            TranscriptLifecycleEvent::TurnStarted,
        )
        .unwrap()
        .expect("turn start should emit an agent event");

        assert_eq!(event.event_type, "agent.running");
        assert_eq!(event.payload["transcriptLifecycleEvent"], "turnStarted");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Running));
    }

    #[test]
    fn delayed_transcript_turn_start_does_not_revive_done_agent() {
        let state = test_state();
        state.insert_agent(sample_agent(AgentStatus::Done)).unwrap();

        let event = transcript_lifecycle_agent_event(
            &state,
            "agent-1",
            "/tmp/session.jsonl",
            TranscriptLifecycleEvent::TurnStarted,
        )
        .unwrap();

        assert!(event.is_none());
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    #[test]
    fn transcript_lifecycle_interruption_ignores_non_working_agent() {
        let state = test_state();
        state.insert_agent(sample_agent(AgentStatus::Done)).unwrap();

        let event = transcript_lifecycle_agent_event(
            &state,
            "agent-1",
            "/tmp/session.jsonl",
            TranscriptLifecycleEvent::Interrupted,
        )
        .unwrap();

        assert!(event.is_none());
    }

    #[test]
    fn transcript_lifecycle_interruption_does_not_double_drain_queued_send() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::Running))
            .unwrap();
        state
            .record_agent_send(
                "agent-1",
                "already drained".to_string(),
                AgentSendSource::QueuedTurn,
            )
            .unwrap();

        let event = transcript_lifecycle_agent_event(
            &state,
            "agent-1",
            "/tmp/session.jsonl",
            TranscriptLifecycleEvent::Interrupted,
        )
        .unwrap();

        assert!(event.is_none());
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Running));
    }

    #[test]
    fn incremental_read_returns_only_appended_bytes_then_resets_on_shrink() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-read-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        fs::write(&path, "a\nb\n").unwrap();
        let first = read_transcript_from(&path, 0).unwrap();
        assert!(!first.reset);
        assert_eq!(first.data, "a\nb\n");
        let consumed = first.consumed_bytes;
        assert_eq!(consumed, 4);

        // An append is read back as just the new bytes, not the whole file.
        fs::write(&path, "a\nb\nc\n").unwrap();
        let second = read_transcript_from(&path, consumed).unwrap();
        assert!(!second.reset);
        assert_eq!(second.data, "c\n");

        // A file shorter than what we've consumed signals a rebuild from scratch.
        fs::write(&path, "x\n").unwrap();
        let third = read_transcript_from(&path, consumed).unwrap();
        assert!(third.reset);
        assert_eq!(third.data, "x\n");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn initial_read_keeps_a_bounded_tail_with_absolute_line_index() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-tail-window-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let total_lines = TRANSCRIPT_TAIL_LINE_LIMIT + 5;
        let contents = (0..total_lines)
            .map(|index| format!("{index}\n"))
            .collect::<String>();
        fs::write(&path, &contents).unwrap();

        let read = read_transcript_from(&path, 0).unwrap();
        assert!(!read.reset);
        assert_eq!(read.start_line_index, 5);
        assert_eq!(read.data.lines().count(), TRANSCRIPT_TAIL_LINE_LIMIT);
        assert!(read.data.starts_with("5\n"));
        assert_eq!(read.consumed_bytes, contents.len() as u64);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_read_holds_back_a_partial_multibyte_tail_without_erroring() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-utf8-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        // A complete record, then the first byte of '€' (E2 82 AC) with no terminating
        // newline: the next record is mid-write and the read ends mid-character.
        let mut bytes = b"{\"a\":1}\n".to_vec();
        bytes.push(0xE2);
        fs::write(&path, &bytes).unwrap();

        // read_to_string would fail with InvalidData here (a spurious read failure);
        // instead the read succeeds and defers the unterminated partial record.
        let read = read_transcript_from(&path, 0).unwrap();
        assert!(!read.reset);
        assert_eq!(read.data, "{\"a\":1}\n");
        assert_eq!(read.consumed_bytes, 8);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_read_advances_by_raw_bytes_over_an_invalid_utf8_line() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-badutf8-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        // A COMPLETE (newline-terminated) line with a lone invalid byte mid-way.
        // from_utf8_lossy expands 0xFF to a 3-byte U+FFFD, so the decoded string is
        // longer than the file; measuring it would overshoot and wedge the tail into
        // a perpetual reset (len < offset every tick). The offset must track raw bytes.
        let mut bytes = b"caf".to_vec();
        bytes.push(0xFF);
        bytes.push(b'\n');
        let raw_len = bytes.len() as u64;
        fs::write(&path, &bytes).unwrap();

        let read = read_transcript_from(&path, 0).unwrap();
        assert!(!read.reset);
        // Decoded string is longer than the bytes on disk...
        assert!(read.data.len() as u64 > raw_len);
        // ...but the consumed offset is the raw byte count, so a follow-up read from it
        // sees EOF (len == offset), not a spurious reset (len < offset).
        assert_eq!(read.consumed_bytes, raw_len);
        let next = read_transcript_from(&path, read.consumed_bytes).unwrap();
        assert!(!next.reset);
        assert_eq!(next.data, "");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_resets_when_an_in_place_rewrite_moves_the_last_newline() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-rewrite-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        fs::write(&path, "aa\nbb\n").unwrap(); // 6 bytes, trailing newline at offset 5
        let first = read_transcript_from(&path, 0).unwrap();
        assert!(!first.reset);
        assert_eq!(first.consumed_bytes, 6);

        // Rewritten in place to the same length, but the byte before offset 6 is no
        // longer '\n' — the content up to the offset changed. Appending would splice new
        // bytes onto a stale timeline, so this must be detected as a reset and rebuilt.
        fs::write(&path, "wxyz\nQ").unwrap(); // 6 bytes, newline now at offset 4
        let second = read_transcript_from(&path, first.consumed_bytes).unwrap();
        assert!(second.reset);
        assert_eq!(second.data, "wxyz\n");

        fs::remove_dir_all(&dir).ok();
    }

    fn candidate(path: &str, secs: u64, session: &str) -> TranscriptCandidate {
        TranscriptCandidate {
            path: PathBuf::from(path),
            modified: UNIX_EPOCH + Duration::from_secs(secs),
            session_id: Some(session.to_string()),
        }
    }

    #[test]
    fn newest_transcript_candidate_prefers_latest_modified_file() {
        let candidates = vec![
            candidate("/tmp/a.jsonl", 10, "a"),
            candidate("/tmp/b.jsonl", 20, "b"),
        ];

        let selected = select_newest_transcript_candidate(&candidates, &HashSet::new(), "")
            .expect("newest candidate is selected");

        assert_eq!(selected.path, PathBuf::from("/tmp/b.jsonl"));
        assert_eq!(selected.session_id.as_deref(), Some("b"));
    }

    #[test]
    fn selection_skips_the_bound_path_and_other_agents_files() {
        let candidates = vec![
            candidate("/tmp/a.jsonl", 10, "a"),
            candidate("/tmp/b.jsonl", 20, "b"),
            candidate("/tmp/c.jsonl", 30, "c"),
        ];
        // c is newest but owned by another agent; b is the bound (missing) file —
        // so recovery must fall back to a, the newest unclaimed candidate.
        let excluded = HashSet::from(["/tmp/c.jsonl".to_string()]);

        let selected = select_newest_transcript_candidate(&candidates, &excluded, "/tmp/b.jsonl")
            .expect("an unclaimed candidate remains");

        assert_eq!(selected.path, PathBuf::from("/tmp/a.jsonl"));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_transcript_scan_ignores_symlink_cycles_and_files() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-symlinks-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let transcript = nested.join("session.jsonl");
        fs::write(&transcript, "{}\n").unwrap();
        symlink(&dir, nested.join("cycle")).unwrap();
        symlink(&transcript, dir.join("linked.jsonl")).unwrap();

        let candidates = gather_transcript_candidates_recursive(&dir).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, transcript);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_preview_collapses_whitespace_and_caps_length() {
        assert_eq!(truncate_preview("  hello   world \n"), "hello world");
        let long = "x ".repeat(120);
        let preview = truncate_preview(&long);
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
    }

    #[test]
    fn read_transcript_meta_extracts_first_user_message_and_line_count() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-meta-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first prompt\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"reply\"}}\n",
            ),
        )
        .unwrap();

        let (preview, line_count) = read_transcript_meta(&path);
        assert_eq!(preview.as_deref(), Some("first prompt"));
        assert_eq!(line_count, 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_transcript_meta_extracts_codex_user_message() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-codex-meta-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-06-21T20-08-03-019eeca7.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019eeca7\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"codex prompt\"}]}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"duplicate\"}}\n",
            ),
        )
        .unwrap();

        let (preview, line_count) = read_transcript_meta(&path);
        assert_eq!(preview.as_deref(), Some("codex prompt"));
        assert_eq!(line_count, 3);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strip_leading_tagged_instruction_blocks_mirrors_the_frontend_semantics() {
        // Whole-message block: nothing left.
        assert_eq!(
            strip_leading_tagged_instruction_blocks(
                "<system-reminder>\ninjected\n</system-reminder>"
            ),
            None
        );
        // Leading block followed by real content: the block goes, the
        // content stays.
        assert_eq!(
            strip_leading_tagged_instruction_blocks(
                "<system-reminder>\ninjected\n</system-reminder>\n\nReal question"
            ),
            Some("\nReal question")
        );
        // Repeated blocks with `# ` labels between them all strip together.
        assert_eq!(
            strip_leading_tagged_instruction_blocks(
                "# label\n<a-tag>\none\n</a-tag>\n# other\n<b-tag>\ntwo\n</b-tag>\nkept"
            ),
            Some("kept")
        );
        // Inline single-line tag blocks strip too.
        assert_eq!(
            strip_leading_tagged_instruction_blocks("<note>inline</note>\nkept"),
            Some("kept")
        );
        // Headings without a following block are content, kept intact.
        assert_eq!(
            strip_leading_tagged_instruction_blocks("# My heading\ncontent"),
            Some("# My heading\ncontent")
        );
        // An unterminated block is content, not an instruction wrapper.
        assert_eq!(
            strip_leading_tagged_instruction_blocks("<config>\nkey = value"),
            Some("<config>\nkey = value")
        );
        // A tag line with attributes is not an instruction tag.
        assert_eq!(
            strip_leading_tagged_instruction_blocks("<ide_context file=\"a.rs\">\nbody"),
            Some("<ide_context file=\"a.rs\">\nbody")
        );
        // Nested same-name blocks are depth-matched like the frontend.
        assert_eq!(
            strip_leading_tagged_instruction_blocks("<t>\n<t>\ninner\n</t>\n</t>\nkept"),
            Some("kept")
        );
    }

    #[test]
    fn read_transcript_meta_skips_tagged_instruction_previews() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-tagged-meta-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<context>ignore</context>\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"# comment\\n<instructions>ignore</instructions>\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"reply\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"real prompt\"}}\n",
            ),
        )
        .unwrap();

        let (preview, line_count) = read_transcript_meta(&path);
        assert_eq!(preview.as_deref(), Some("real prompt"));
        assert_eq!(line_count, 4);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_transcript_meta_stops_preview_scan_after_five_user_messages() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-preview-limit-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<one>ignore</one>\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<two>ignore</two>\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<three>ignore</three>\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<four>ignore</four>\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<five>ignore</five>\"}}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"sixth prompt\"}}\n",
            ),
        )
        .unwrap();

        let (preview, line_count) = read_transcript_meta(&path);
        assert_eq!(preview, None);
        assert_eq!(line_count, 6);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_transcript_meta_skips_oversized_records_with_bounded_buffering() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-bounded-meta-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let mut contents = vec![b'x'; TRANSCRIPT_META_LINE_LIMIT as usize + 1];
        contents.extend_from_slice(
            b"\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"bounded prompt\"}}\n",
        );
        fs::write(&path, contents).unwrap();

        let (preview, line_count) = read_transcript_meta(&path);
        assert_eq!(preview.as_deref(), Some("bounded prompt"));
        assert_eq!(line_count, 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_transcript_cwd_reads_session_meta_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-transcript-codex-cwd-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();

        // A well-formed rollout exposes its project directory from session_meta.
        let with_cwd = dir.join("rollout-with-cwd.jsonl");
        fs::write(
            &with_cwd,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\",\"cwd\":\"/work/project\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}}\n",
            ),
        )
        .unwrap();
        assert_eq!(
            codex_transcript_cwd(&with_cwd).as_deref(),
            Some("/work/project")
        );

        // A first line that isn't a session_meta, or one without a cwd, yields None
        // (so the picker falls back to listing rather than hiding everything).
        let without_meta = dir.join("rollout-no-meta.jsonl");
        fs::write(&without_meta, "{\"type\":\"response_item\"}\n").unwrap();
        assert_eq!(codex_transcript_cwd(&without_meta), None);

        let empty = dir.join("rollout-empty.jsonl");
        fs::write(&empty, "").unwrap();
        assert_eq!(codex_transcript_cwd(&empty), None);

        assert_eq!(codex_transcript_cwd(&dir.join("missing.jsonl")), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_sessions_root_finds_date_sharded_parent() {
        let path = Path::new(
            "/Users/raymond/.codex/sessions/2026/06/21/rollout-2026-06-21T20-08-03-id.jsonl",
        );

        assert_eq!(
            codex_sessions_root(path).as_deref(),
            Some(Path::new("/Users/raymond/.codex/sessions"))
        );
    }

    #[test]
    fn session_id_comes_from_transcript_filename() {
        assert_eq!(
            session_id_from_transcript_path(Path::new(
                "/Users/raymond/.claude/projects/project/5e675dea.jsonl"
            ))
            .as_deref(),
            Some("5e675dea")
        );
        assert_eq!(
            session_id_from_transcript_path(Path::new("/tmp/.jsonl")),
            None
        );
    }

    #[test]
    fn grok_picker_lists_chat_histories_from_sibling_sessions() {
        let dir = temp_dir();
        let group = dir.join("sessions").join("%2Fwork%2Fproject");
        let first = group.join("session-1").join("chat_history.jsonl");
        let second = group.join("session-2").join("chat_history.jsonl");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(
            &first,
            concat!(
                "{\"type\":\"user\",\"synthetic_reason\":\"system_reminder\",\"content\":[{\"type\":\"text\",\"text\":\"private\"}]}\n",
                "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"first prompt\"}]}\n",
            ),
        )
        .unwrap();
        fs::write(
            &second,
            "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"second prompt\"}]}\n",
        )
        .unwrap();
        fs::write(group.join("session-2").join("updates.jsonl"), "{}\n").unwrap();

        let state = test_state();
        let mut agent = sample_agent(AgentStatus::Idle);
        agent.adapter = "grok".to_string();
        agent.session_id = Some("session-1".to_string());
        agent.transcript_path = Some(first.display().to_string());
        state.insert_agent(agent).unwrap();

        let options = list_agent_transcripts(&state, "agent-1").unwrap();
        assert_eq!(options.len(), 2);
        assert!(
            options
                .iter()
                .all(|option| option.path.ends_with("chat_history.jsonl"))
        );
        assert!(options.iter().any(|option| {
            option.session_id.as_deref() == Some("session-1")
                && option.preview.as_deref() == Some("first prompt")
        }));
        assert!(options.iter().any(|option| {
            option.session_id.as_deref() == Some("session-2")
                && option.preview.as_deref() == Some("second prompt")
        }));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grok_picker_repoint_is_confined_to_native_sibling_sessions() {
        let dir = temp_dir();
        let group = dir.join("sessions").join("%2Fwork%2Fproject");
        let current = group.join("session-1").join("chat_history.jsonl");
        let sibling = group.join("session-2").join("chat_history.jsonl");
        let wrong_name = group.join("session-2").join("updates.jsonl");
        let outside = dir
            .join("other")
            .join("session-3")
            .join("chat_history.jsonl");
        for path in [&current, &sibling, &wrong_name, &outside] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "{}\n").unwrap();
        }

        assert!(validate_grok_transcript_candidate(&current, &sibling).is_ok());
        assert!(validate_grok_transcript_candidate(&current, &wrong_name).is_err());
        assert!(validate_grok_transcript_candidate(&current, &outside).is_err());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_picker_lists_rotated_agent_sessions_only() {
        let dir = temp_dir();
        let agent_dir = dir.join(".qmux").join("opencode").join("agent-1");
        fs::create_dir_all(&agent_dir).unwrap();
        let first = agent_dir.join("session-1.jsonl");
        let second = agent_dir.join("session-2.jsonl");
        fs::write(
            &first,
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"first prompt\"}]}}\n",
        )
        .unwrap();
        fs::write(
            &second,
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"second prompt\"}]}}\n",
        )
        .unwrap();

        let state = test_state();
        let mut agent = sample_agent(AgentStatus::Idle);
        agent.adapter = "opencode".to_string();
        agent.session_id = Some("session-1".to_string());
        agent.transcript_path = Some(first.display().to_string());
        state.insert_agent(agent).unwrap();

        let options = list_agent_transcripts(&state, "agent-1").unwrap();
        assert_eq!(options.len(), 2);
        assert!(options.iter().any(|option| {
            option.session_id.as_deref() == Some("session-1")
                && option.preview.as_deref() == Some("first prompt")
        }));
        assert!(options.iter().any(|option| {
            option.session_id.as_deref() == Some("session-2")
                && option.preview.as_deref() == Some("second prompt")
        }));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_picker_stays_hidden_for_legacy_combined_transcript() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("agent-1.jsonl");
        fs::write(&legacy, "{}\n").unwrap();

        let state = test_state();
        let mut agent = sample_agent(AgentStatus::Idle);
        agent.adapter = "opencode".to_string();
        agent.transcript_path = Some(legacy.display().to_string());
        state.insert_agent(agent).unwrap();

        assert!(
            list_agent_transcripts(&state, "agent-1")
                .unwrap()
                .is_empty()
        );

        fs::remove_dir_all(&dir).ok();
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            workspace_root: temp_dir(),
            socket_path: PathBuf::from("/tmp/qmux-transcript-test.sock"),
            adapters: AdapterConfigs {
                claude: ClaudeAdapterConfig {
                    binary: Some("claude".to_string()),
                },
                codex: CodexAdapterConfig {
                    binary: Some("codex".to_string()),
                },
                opencode: OpencodeAdapterConfig {
                    binary: Some("opencode".to_string()),
                },
                grok: GrokAdapterConfig {
                    binary: Some("grok".to_string()),
                },
            },
            legacy_claude_binary: None,
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
        })
    }

    fn sample_agent(status: AgentStatus) -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/qmux-transcript-test".to_string(),
            branch: None,
            pane_id: Some("pane-1".to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: Some("/tmp/session.jsonl".to_string()),
            status,
            model: None,
            effort: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            thread_id: None,
            branch_id: None,
            paused: false,
            created_at: 1,
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "qmux-transcript-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }
}

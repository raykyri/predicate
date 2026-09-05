//! Desktop-owned SSH transcript readers. Remote hooks supply metadata only;
//! all local destinations are derived here, never from a remote pathname.
use crate::events::QmuxEvent;
use crate::host::{self, Host, RemoteCommand};
use crate::state::AppState;
use qmux_cli::transcript_stream::{Cursor, Frame, MAX_CHUNK, MAX_FRAME};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Binding {
    agent: String,
    pane: String,
    adapter: String,
    session: String,
    remote_id: String,
    remote_host: String,
    hint: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Checkpoint {
    cursor: Cursor,
    generation: u64,
    source: String,
}
static READERS: LazyLock<Mutex<HashMap<PathBuf, Binding>>> = LazyLock::new(Default::default);

fn encoded(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn directory(state: &AppState, agent: &str) -> PathBuf {
    state
        .config()
        .workspace_root
        .join("remote-transcripts")
        .join(encoded(agent))
}
fn session_directory(root: &Path, binding: &Binding) -> PathBuf {
    root.join(encoded(&binding.remote_id))
        .join(encoded(&binding.session))
}
fn mirror(dir: &Path, checkpoint: &Checkpoint, session: &str) -> PathBuf {
    dir.join(checkpoint.generation.to_string())
        .join(format!("{session}.jsonl"))
}
fn save_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, value).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(temp, path).map_err(|e| e.to_string())
}

fn live_host(state: &AppState, binding: &Binding) -> Option<Host> {
    if !qmux_cli::transcript_stream::valid_session(&binding.session) {
        return None;
    }
    let agent = state.agent(&binding.agent).ok()??;
    if agent.pane_id.as_deref() != Some(binding.pane.as_str())
        || agent.session_id.as_deref() != Some(binding.session.as_str())
        || agent.adapter != binding.adapter
        || agent.fork_point.as_deref() == Some(binding.session.as_str())
        || agent.orphaned_queue_pane_id.is_some()
    {
        return None;
    }
    let pane = state
        .list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.id == binding.pane)?;
    if pane.remote_session.as_ref()?.remote_id != binding.remote_id {
        return None;
    }
    let group = state.group(&agent.group_id).ok()??;
    let host = host::for_group(group.remote.as_ref());
    let remote = host.remote()?;
    (remote.id == binding.remote_id && remote.ssh == binding.remote_host).then_some(host)
}

/// Called only after adapter ingestion has accepted the session identity. Ignore
/// subagent and stale-session metadata rather than following a different session.
pub fn observe(state: &AppState, pane: &str, payload: &Value) {
    if payload.get("agent_id").is_some() || payload.get("agentId").is_some() {
        return;
    }
    let result = (|| -> Result<(), String> {
        let Some(agent) = state.agent_by_pane(pane)? else {
            return Ok(());
        };
        if !matches!(agent.adapter.as_str(), "claude" | "codex") {
            return Ok(());
        }
        let Some(session) = agent.session_id.clone() else {
            return Ok(());
        };
        if agent.fork_point.as_deref() == Some(session.as_str())
            || !qmux_cli::transcript_stream::valid_session(&session)
        {
            return Ok(());
        }
        let reported = payload
            .get("session_id")
            .or_else(|| payload.get("sessionId"))
            .or_else(|| payload.get("resource_id"))
            .or_else(|| payload.get("resourceId"))
            .and_then(Value::as_str);
        if reported.is_some_and(|id| id != session) {
            return Ok(());
        }
        let Some(group) = state.group(&agent.group_id)? else {
            return Ok(());
        };
        let host = host::for_group(group.remote.as_ref());
        let Some(remote) = host.remote() else {
            return Ok(());
        };
        let root = directory(state, &agent.id);
        let hint = payload
            .get("transcript_path")
            .or_else(|| payload.get("transcriptPath"))
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 8192)
            .unwrap_or("")
            .to_string();
        let mut binding = Binding {
            agent: agent.id,
            pane: pane.into(),
            adapter: agent.adapter,
            session,
            remote_id: remote.id.clone(),
            remote_host: remote.ssh.clone(),
            hint,
        };
        let ready = live_host(state, &binding).is_some();
        // Launch reserves the agent binding before spawning SSH. A fast hook
        // can arrive before insert_pane; keep its path for that handoff.
        if !ready && state.list_panes()?.iter().any(|p| p.id == pane) {
            return Ok(());
        }
        let mut readers = READERS.lock().map_err(|e| e.to_string())?;
        if binding.hint.is_empty() {
            let previous = readers.get(&root).cloned().or_else(|| {
                fs::read(root.join("binding.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Binding>(&bytes).ok())
            });
            if let Some(old) = previous
                && old.session == binding.session
                && old.remote_id == binding.remote_id
                && old.remote_host == binding.remote_host
                && old.adapter == binding.adapter
            {
                binding.hint = old.hint;
            }
        }
        if readers.get(&root) == Some(&binding) {
            return Ok(());
        }
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        save_json(&root.join("binding.json"), &binding)?;
        if !ready {
            return Ok(());
        }
        let start = readers.insert(root.clone(), binding).is_none();
        if start {
            let state = state.clone();
            thread::spawn(move || worker(state, root));
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("qmux: remote transcript setup: {error}");
    }
}

/// Reattach needs no fresh SessionStart: recover metadata persisted by the last
/// hook. Old hosts/sessions are checked against the current pane before use.
pub fn restore(state: &AppState, pane: &str) {
    let Ok(Some(agent)) = state.agent_by_pane(pane) else {
        return;
    };
    let root = directory(state, &agent.id);
    let Ok(bytes) = fs::read(root.join("binding.json")) else {
        // Existing panes from before streaming have a native session id but no
        // mirror metadata yet. Discover that session without waiting for a hook.
        observe(state, pane, &Value::Null);
        return;
    };
    let Ok(binding) = serde_json::from_slice::<Binding>(&bytes) else {
        return;
    };
    if binding.agent != agent.id || live_host(state, &binding).is_none() {
        return;
    }
    let Ok(mut readers) = READERS.lock() else {
        return;
    };
    if live_host(state, &binding).is_none() {
        return;
    }
    let start = readers.insert(root.clone(), binding).is_none();
    if !start {
        return;
    }
    let state = state.clone();
    thread::spawn(move || worker(state, root));
}

fn notice(state: &AppState, binding: &Binding, message: Option<&str>) {
    state.emit(QmuxEvent::new(
        "transcript.notice",
        Some(binding.pane.clone()),
        Some(binding.agent.clone()),
        json!({"message": message}),
    ));
}

fn recover(dir: &Path, session: &str) -> Result<Checkpoint, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join("checkpoint.json");
    let checkpoint: Checkpoint = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Checkpoint::default(),
        Err(e) => return Err(e.to_string()),
    };
    if checkpoint.cursor.offset > 0 {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(mirror(dir, &checkpoint, session))
            .map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() < checkpoint.cursor.offset {
            return Err("local transcript mirror is shorter than its checkpoint".into());
        }
        // Discard an uncommitted suffix left by a crash between data and cursor.
        file.set_len(checkpoint.cursor.offset)
            .map_err(|e| e.to_string())?;
    }
    Ok(checkpoint)
}

fn accept(
    dir: &Path,
    session: &str,
    checkpoint: &mut Checkpoint,
    frame: Frame,
) -> Result<PathBuf, String> {
    if frame.session != session
        || frame.data.len() > MAX_CHUNK
        || frame.start.checked_add(frame.data.len() as u64) != Some(frame.cursor.offset)
        || frame.cursor.anchor.len() != frame.cursor.offset.min(256) as usize
        || (frame.reset && frame.start != 0)
        || (!frame.reset
            && (frame.start != checkpoint.cursor.offset
                || frame.cursor.identity != checkpoint.cursor.identity))
    {
        return Err("invalid remote transcript frame".into());
    }
    // Frames may end inside a JSON record. Persist the exact fragments; the
    // ordinary local tail only parses bytes through the last complete newline.
    let next = Checkpoint {
        cursor: frame.cursor,
        generation: checkpoint.generation + u64::from(frame.reset),
        source: frame.path,
    };
    let path = mirror(dir, &next, session);
    if !frame.data.is_empty() || frame.reset {
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.set_len(frame.start).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(frame.start))
            .map_err(|e| e.to_string())?;
        file.write_all(frame.data.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        save_json(&dir.join("checkpoint.json"), &next)?;
    }
    *checkpoint = next;
    Ok(path)
}

fn worker(state: AppState, root: PathBuf) {
    let mut installed: Option<(Host, String)> = None;
    let historical_end = Arc::new(AtomicU64::new(u64::MAX));
    loop {
        let binding = {
            let Ok(mut readers) = READERS.lock() else {
                return;
            };
            let Some(binding) = readers.get(&root).cloned() else {
                return;
            };
            if live_host(&state, &binding).is_none() {
                readers.remove(&root);
                return;
            }
            binding
        };
        let result = (|| {
            let host = live_host(&state, &binding).ok_or("remote session ended")?;
            if installed
                .as_ref()
                .is_none_or(|(previous, _)| previous != &host)
            {
                let prepared = crate::remote_cli::ensure_cli(&host)?;
                installed = Some((host, prepared.path));
            }
            stream(
                &state,
                &root,
                &binding,
                &installed.as_ref().unwrap().1,
                &historical_end,
            )
        })();
        if let Err(error) = result {
            eprintln!("qmux: remote transcript {}: {error}", binding.agent);
            notice(
                &state,
                &binding,
                Some("Transcript reconnecting; waiting for remote transcript."),
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn stream(
    state: &AppState,
    root: &Path,
    binding: &Binding,
    cli: &str,
    historical_end: &Arc<AtomicU64>,
) -> Result<(), String> {
    historical_end.store(u64::MAX, Ordering::SeqCst);
    let host = live_host(state, binding).ok_or("remote session ended")?;
    let dir = session_directory(root, binding);
    let mut checkpoint = recover(&dir, &binding.session)?;
    let mut child = host
        .command(RemoteCommand {
            program: cli,
            args: vec![
                "transcript-stream".into(),
                binding.adapter.clone(),
                binding.session.clone(),
                if binding.hint.is_empty() {
                    checkpoint.source.clone()
                } else {
                    binding.hint.clone()
                },
                serde_json::to_string(&checkpoint.cursor).map_err(|e| e.to_string())?,
            ],
            ..Default::default()
        })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            let result = reader.by_ref().take(MAX_FRAME + 1).read_line(&mut line);
            match result {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() as u64 > MAX_FRAME || !line.ends_with('\n') => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let result = (|| {
        let mut last_frame = Instant::now();
        let mut announced = false;
        loop {
            // The same lock guards hook rebinding and frame acceptance, so an
            // old worker cannot commit after a new session's hook takes over.
            let readers = READERS.lock().map_err(|e| e.to_string())?;
            if readers.get(root) != Some(binding) || live_host(state, binding).is_none() {
                return Ok(());
            }
            drop(readers);
            let line = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout)
                    if last_frame.elapsed() < Duration::from_secs(60) =>
                {
                    continue;
                }
                Err(_) => return Err("remote transcript stream disconnected or timed out".into()),
            };
            let frame: Frame = serde_json::from_str(&line).map_err(|e| e.to_string())?;
            let readers = READERS.lock().map_err(|e| e.to_string())?;
            if readers.get(root) != Some(binding) || live_host(state, binding).is_none() {
                return Ok(());
            }
            // Publish the boundary before making historical bytes visible to the tail.
            historical_end.store(frame.historical_end, Ordering::SeqCst);
            let path = accept(&dir, &binding.session, &mut checkpoint, frame)?;
            let path = path.to_string_lossy().into_owned();
            let agent = state.agent(&binding.agent)?.ok_or("agent disappeared")?;
            let changed = agent.transcript_path.as_deref() != Some(path.as_str());
            if changed {
                let updated = state.mutate_agent(&binding.agent, |agent| {
                    if agent.session_id.as_deref() == Some(binding.session.as_str())
                        && agent.pane_id.as_deref() == Some(binding.pane.as_str())
                    {
                        agent.transcript_path = Some(path.clone());
                    }
                })?;
                if updated.is_none_or(|agent| {
                    agent.transcript_path.as_deref() != Some(path.as_str())
                        || agent.session_id.as_deref() != Some(binding.session.as_str())
                        || agent.pane_id.as_deref() != Some(binding.pane.as_str())
                }) {
                    return Ok(());
                }
            }
            if changed || !announced {
                crate::transcript::start_remote_transcript_tail(
                    state.clone(),
                    binding.agent.clone(),
                    path,
                    binding.adapter.clone(),
                    historical_end.clone(),
                );
            }
            if !announced {
                notice(state, binding, None);
                announced = true;
            }
            last_frame = Instant::now();
        }
    })();
    // Closing the receiver also releases a reader blocked on the bounded queue.
    drop(rx);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_metadata_is_persisted_and_stale_sessions_cannot_rebind() {
        let root = std::env::temp_dir().join(format!("qmux-mirror-binding-{}", std::process::id()));
        let state = AppState::new(
            serde_json::from_value(json!({
                "workspaceRoot":root, "socketPath":root.join("unused.sock")
            }))
            .unwrap(),
        );
        let group: crate::workspace::GroupInfo = serde_json::from_value(json!({
            "id":"g", "name":"Remote", "dir":"/remote", "managedDir":root.join("g"),
            "createdAt":1, "agents":["a"], "remote":{"id":"r", "label":"Remote", "host":"unreachable.invalid", "multiplexer":"tmux"}
        })).unwrap();
        state.update_group(group.clone()).unwrap();
        state.insert_agent(serde_json::from_value(json!({
            "id":"a", "groupId":"g", "adapter":"claude", "worktreeDir":"/remote", "paneId":"p",
            "sessionId":"session", "status":"running", "createdAt":1
        })).unwrap()).unwrap();
        let identity = crate::state::RemoteSessionIdentity::new("r", "p").unwrap();
        let host = host::for_group(group.remote.as_ref());
        let commands = host
            .existing_tmux_session_commands(&identity, "/unused.sock")
            .unwrap();
        observe(
            &state,
            "p",
            &json!({"session_id":"session", "transcript_path":"/remote/custom/session.jsonl"}),
        );
        let early_key = directory(&state, "a");
        let early: Binding =
            serde_json::from_slice(&fs::read(early_key.join("binding.json")).unwrap()).unwrap();
        assert_eq!(early.hint, "/remote/custom/session.jsonl");
        assert!(!READERS.lock().unwrap().contains_key(&early_key));
        state.insert_pane(crate::state::PaneRuntime {
            info: serde_json::from_value(json!({"id":"p", "title":"Remote", "kind":"agent", "agentId":"a", "groupId":"g", "cwd":"/remote",
                "remoteSession":identity, "cols":80, "rows":24, "status":"running"})).unwrap(),
            backend: crate::state::PaneBackend::RemoteTmux(crate::state::RemoteTmuxBackend::new(
                crate::remote_terminal::RemoteAttachmentController::new(),
                crate::remote_terminal::RemoteHistoryCheckpoint::new(Vec::new()),
                std::sync::Arc::new(Mutex::new(Default::default())), commands, false)),
            cwd_observation_seq:0
        }).unwrap();
        let key = directory(&state, "a");
        // Reserve the worker slot to exercise metadata routing without SSH.
        let reserved = Binding {
            agent: "a".into(),
            pane: "p".into(),
            adapter: "claude".into(),
            session: "old".into(),
            remote_id: "r".into(),
            remote_host: "unreachable.invalid".into(),
            hint: String::new(),
        };
        READERS.lock().unwrap().insert(key.clone(), reserved);
        observe(
            &state,
            "p",
            &json!({"session_id":"session", "transcript_path":"/remote/session.jsonl"}),
        );
        let binding = READERS.lock().unwrap().get(&key).unwrap().clone();
        assert_eq!(binding.session, "session");
        assert_eq!(binding.hint, "/remote/session.jsonl");
        assert!(live_host(&state, &binding).is_some());
        assert!(state.agent("a").unwrap().unwrap().transcript_path.is_none());
        let saved: Binding =
            serde_json::from_slice(&fs::read(key.join("binding.json")).unwrap()).unwrap();
        assert_eq!(saved, binding);
        for payload in [
            json!({"session_id":"stale", "transcript_path":"/bad"}),
            json!({"agent_id":"subagent", "session_id":"session", "transcript_path":"/bad"}),
        ] {
            observe(&state, "p", &payload);
            assert_eq!(READERS.lock().unwrap().get(&key), Some(&binding));
        }
        state
            .mutate_agent("a", |agent| agent.session_id = Some("next".into()))
            .unwrap();
        assert!(live_host(&state, &binding).is_none());
        observe(&state, "p", &json!({"session_id":"next"}));
        let next = READERS.lock().unwrap().get(&key).unwrap().clone();
        assert_eq!(next.session, "next");
        assert!(next.hint.is_empty());
        assert_ne!(
            session_directory(&key, &binding),
            session_directory(&key, &next)
        );
        fs::remove_file(key.join("binding.json")).unwrap();
        READERS.lock().unwrap().insert(key.clone(), binding.clone());
        restore(&state, "p");
        assert_eq!(READERS.lock().unwrap().get(&key).unwrap().session, "next");
        assert!(key.join("binding.json").exists());
        state
            .mutate_agent("a", |agent| agent.fork_point = Some("next".into()))
            .unwrap();
        assert!(live_host(&state, &next).is_none());
        observe(
            &state,
            "p",
            &json!({"session_id":"next", "transcript_path":"/source/fork.jsonl"}),
        );
        assert!(READERS.lock().unwrap().get(&key).unwrap().hint.is_empty());
        READERS.lock().unwrap().remove(&key);
    }

    fn message_line(adapter: &str, message: &str) -> String {
        let value = if adapter == "claude" {
            json!({"type":"user", "uuid":message, "sessionId":"session", "message":{"role":"user", "content":message}})
        } else {
            json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":message}]}})
        };
        value.to_string() + "\n"
    }
    #[test]
    fn claude_mirror_reaches_turn_pipeline_and_resets_cleanly() {
        check_mirrored_pipeline("claude");
    }
    #[test]
    fn codex_mirror_reaches_turn_pipeline_and_resets_cleanly() {
        check_mirrored_pipeline("codex");
    }

    fn check_mirrored_pipeline(adapter: &str) {
        let root = std::env::temp_dir().join(format!(
            "qmux-mirror-pipeline-{adapter}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let config = serde_json::from_value(json!({
            "workspaceRoot": root, "socketPath": root.join("unused.sock")
        }))
        .unwrap();
        let state = AppState::new(config);
        let agent = serde_json::from_value(json!({
            "id": "remote-test-agent", "groupId": "g", "adapter": adapter,
            "worktreeDir": "/remote/project", "paneId": "p", "sessionId": "session",
            "status": "running", "createdAt": 1
        }))
        .unwrap();
        state.insert_agent(agent).unwrap();
        let source = root.join("source.jsonl");
        let dir = root.join("mirror");
        fs::create_dir_all(&dir).unwrap();
        let mut checkpoint = Checkpoint::default();
        for (message, expected_count) in [("hello", 1), ("second", 2)] {
            let line = message_line(adapter, message);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&source)
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
            let frame =
                qmux_cli::transcript_stream::read_frame(&source, "session", &checkpoint.cursor)
                    .unwrap();
            let path = accept(&dir, "session", &mut checkpoint, frame).unwrap();
            state
                .mutate_agent("remote-test-agent", |agent| {
                    agent.transcript_path = Some(path.to_string_lossy().into())
                })
                .unwrap();
            crate::transcript::start_transcript_tail(
                state.clone(),
                "remote-test-agent".into(),
                path.to_string_lossy().into(),
                adapter.into(),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let turns = state.list_turns(Some("remote-test-agent")).unwrap();
                if turns.len() == expected_count {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "transcript did not produce {expected_count} turns: {turns:?}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        // Same session, rewritten file: the new generation must replace turns.
        fs::write(&source, message_line(adapter, "replacement")).unwrap();
        let frame = qmux_cli::transcript_stream::read_frame(&source, "session", &checkpoint.cursor)
            .unwrap();
        let path = accept(&dir, "session", &mut checkpoint, frame).unwrap();
        state
            .mutate_agent("remote-test-agent", |agent| {
                agent.transcript_path = Some(path.to_string_lossy().into())
            })
            .unwrap();
        crate::transcript::start_transcript_tail(
            state.clone(),
            "remote-test-agent".into(),
            path.to_string_lossy().into(),
            adapter.into(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let turns = state.list_turns(Some("remote-test-agent")).unwrap();
            if turns.len() == 1
                && serde_json::to_string(&turns[0].blocks)
                    .unwrap()
                    .contains("replacement")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "rewrite did not replace turns: {turns:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        state
            .mutate_agent("remote-test-agent", |agent| agent.transcript_path = None)
            .unwrap();
        // Leave the temporary workspace to the OS: AppState's debounced writer
        // may still be finishing a snapshot after the tail has stopped.
    }

    #[test]
    fn remote_backfill_completion_does_not_settle_live_agent() {
        let root =
            std::env::temp_dir().join(format!("qmux-backfill-lifecycle-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let state = AppState::new(
            serde_json::from_value(json!({
                "workspaceRoot": root, "socketPath": root.join("unused.sock")
            }))
            .unwrap(),
        );
        let path = root.join("session.jsonl");
        let initial = message_line("codex", "initial");
        fs::write(&path, &initial).unwrap();
        state
            .insert_agent(
                serde_json::from_value(json!({
                    "id":"backfill-agent", "groupId":"g", "adapter":"codex",
                    "worktreeDir":"/remote/project", "paneId":"p", "sessionId":"session",
                    "transcriptPath":path, "status":"running", "createdAt":1
                }))
                .unwrap(),
            )
            .unwrap();
        let completion = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n";
        let history = format!("{completion}{}", message_line("codex", "historical"));
        let boundary = Arc::new(AtomicU64::new((initial.len() + history.len()) as u64));
        crate::transcript::start_remote_transcript_tail(
            state.clone(),
            "backfill-agent".into(),
            path.to_string_lossy().into(),
            "codex".into(),
            boundary.clone(),
        );
        let wait_turns = |count| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while state.list_turns(Some("backfill-agent")).unwrap().len() < count {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(20));
            }
        };
        wait_turns(1); // Ensure the next batch takes the incremental tail path.
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(history.as_bytes())
            .unwrap();
        wait_turns(2);
        assert_eq!(
            state.agent("backfill-agent").unwrap().unwrap().status,
            crate::workspace::AgentStatus::Running
        );
        // A reconnect also treats downloaded completion records as historical.
        boundary.store(u64::MAX, Ordering::SeqCst);
        let reconnect = format!("{completion}{}", message_line("codex", "reconnected"));
        let end = fs::metadata(&path).unwrap().len() + reconnect.len() as u64;
        boundary.store(end, Ordering::SeqCst);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(reconnect.as_bytes())
            .unwrap();
        wait_turns(3);
        assert_eq!(
            state.agent("backfill-agent").unwrap().unwrap().status,
            crate::workspace::AgentStatus::Running
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(completion.as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.agent("backfill-agent").unwrap().unwrap().status
            != crate::workspace::AgentStatus::Done
        {
            assert!(
                Instant::now() < deadline,
                "live completion did not settle agent"
            );
            thread::sleep(Duration::from_millis(20));
        }
        state
            .mutate_agent("backfill-agent", |a| a.transcript_path = None)
            .unwrap();
    }

    #[test]
    fn chunked_large_record_reassembles_after_checkpoint_recovery() {
        let root = std::env::temp_dir().join(format!("qmux-large-mirror-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.jsonl");
        let dir = root.join("mirror");
        let record = json!({"type":"user", "message":{"content":[
            {"type":"image", "source":{"type":"base64", "data":"A".repeat(5 * 1024 * 1024)}},
            {"type":"text", "text":"image prompt"}
        ]}})
        .to_string()
            + "\n";
        let contents = record + &message_line("claude", "following message");
        fs::write(&source, &contents).unwrap();
        let mut cp = Checkpoint::default();
        let first =
            qmux_cli::transcript_stream::read_frame(&source, "session", &cp.cursor).unwrap();
        assert!(!first.data.ends_with('\n'));
        let path = accept(&dir, "session", &mut cp, first).unwrap();
        // Mid-record progress is durable, and an interrupted write is discarded.
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"uncommitted")
            .unwrap();
        cp = recover(&dir, "session").unwrap();
        while cp.cursor.offset < contents.len() as u64 {
            let frame =
                qmux_cli::transcript_stream::read_frame(&source, "session", &cp.cursor).unwrap();
            assert!(!frame.data.is_empty());
            assert_eq!(accept(&dir, "session", &mut cp, frame).unwrap(), path);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        let state = AppState::new(
            serde_json::from_value(json!({
                "workspaceRoot":root, "socketPath":root.join("unused.sock")
            }))
            .unwrap(),
        );
        state
            .insert_agent(
                serde_json::from_value(json!({
                    "id":"large-agent", "groupId":"g", "adapter":"claude",
                    "worktreeDir":"/remote/project", "paneId":"p", "sessionId":"session",
                    "transcriptPath":path, "status":"running", "createdAt":1
                }))
                .unwrap(),
            )
            .unwrap();
        crate::transcript::start_transcript_tail(
            state.clone(),
            "large-agent".into(),
            path.to_string_lossy().into(),
            "claude".into(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let turns = state.list_turns(Some("large-agent")).unwrap();
            if turns.len() == 2 {
                assert!(
                    serde_json::to_string(&turns)
                        .unwrap()
                        .contains("following message")
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "large record did not reach turn pipeline"
            );
            thread::sleep(Duration::from_millis(20));
        }
        state
            .mutate_agent("large-agent", |agent| agent.transcript_path = None)
            .unwrap();
    }

    #[test]
    fn checkpoint_recovers_uncommitted_suffix_and_rejects_duplicates() {
        let dir = std::env::temp_dir().join(format!("qmux-mirror-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut cp = Checkpoint::default();
        let data = "{\"a\":1}\n";
        let make = |reset, start| Frame {
            session: "s".into(),
            path: "/remote/file".into(),
            start,
            reset,
            historical_end: 0,
            cursor: Cursor {
                offset: start + data.len() as u64,
                identity: "inode".into(),
                anchor: data.as_bytes().to_vec(),
            },
            data: data.into(),
        };
        let path = accept(&dir, "s", &mut cp, make(true, 0)).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"uncommitted")
            .unwrap();
        let mut recovered = recover(&dir, "s").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), data);
        assert!(accept(&dir, "s", &mut recovered, make(false, 0)).is_err());
        assert!(accept(&dir, "other", &mut recovered, make(true, 0)).is_err());
        let next = accept(&dir, "s", &mut recovered, make(true, 0)).unwrap();
        assert_ne!(path, next); // A rewrite restarts parsing, even at equal length.
        fs::remove_dir_all(dir).unwrap();
    }
}

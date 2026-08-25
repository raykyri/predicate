//! Headless Claude Agent SDK control protocol over `claude -p` stream-json.
//!
//! Matches the official Python SDK's `SubprocessCLITransport` + `Query`:
//! initialize handshake before any user message, `--permission-prompt-tool stdio`,
//! correlated `control_request` / `control_response`.

use crate::adapters::hook_transcript_path_acceptable;
use crate::headless_process::reconcile_session_id;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

pub const MIN_CLAUDE_VERSION: (u32, u32, u32) = (2, 1, 0);
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(2);
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub const SESSION_INIT_STALL: Duration = Duration::from_secs(30);
pub const RESULT_AFTER_END_TURN_STALL: Duration = Duration::from_secs(30);
pub const LOGIN_ERROR_MESSAGE: &str = "Claude Code is not logged in. Open a terminal tab, run claude, and sign in — then retry this research.";
const MAX_STDOUT_LINE_BYTES: usize = 1024 * 1024;
const MAX_PENDING_STDOUT_MESSAGES: usize = 256;
const MAX_STDERR_LOG_BYTES: usize = 4 * 1024 * 1024;
const READ_ONLY_TOOLS: &str = "Read,Grep,Glob,WebSearch,WebFetch,NotebookRead,TodoWrite";
const ALLOWED_TOOL_NAMES: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "WebSearch",
    "WebFetch",
    "TodoWrite",
    "NotebookRead",
];

#[derive(Debug, Clone)]
pub struct ClaudeVersion {
    pub triple: (u32, u32, u32),
}

impl ClaudeVersion {
    pub fn meets_floor(&self) -> bool {
        self.triple >= MIN_CLAUDE_VERSION
    }

    pub fn display(&self) -> String {
        format!("{}.{}.{}", self.triple.0, self.triple.1, self.triple.2)
    }
}

pub fn parse_claude_version(output: &str) -> Option<ClaudeVersion> {
    let raw = output.lines().next().unwrap_or(output).trim().to_string();
    let mut digits = raw
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let major = digits.next()?.parse().ok()?;
    let minor = digits.next()?.parse().ok()?;
    let patch = digits.next()?.parse().ok()?;
    Some(ClaudeVersion {
        triple: (major, minor, patch),
    })
}

pub fn probe_claude_version(binary: &str) -> Result<ClaudeVersion, String> {
    let mut child = Command::new(binary)
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run `{binary} -v`: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("`{binary} -v` stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("`{binary} -v` stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || {
        let mut stdout = stdout;
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("`{binary} -v` timed out"));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("failed to wait for `{binary} -v`: {err}"));
            }
        }
    }
    let mut combined = stdout_reader.join().unwrap_or_default();
    combined.extend(stderr_reader.join().unwrap_or_default());
    let combined = String::from_utf8_lossy(&combined);
    let version = parse_claude_version(&combined)
        .ok_or_else(|| format!("unable to parse Claude Code version from `{binary} -v`"))?;
    if !version.meets_floor() {
        return Err(format!(
            "Claude Code {} cannot run headless research (need ≥ 2.1.0); update Claude Code or turn off the research SDK setting",
            version.display()
        ));
    }
    Ok(version)
}

pub fn login_error_message(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if lower.contains("not logged in")
        || lower.contains("please run /login")
        || lower.contains("not authenticated")
        || (lower.contains("authentication")
            && (lower.contains("required") || lower.contains("failed") || lower.contains("error")))
    {
        Some(LOGIN_ERROR_MESSAGE.to_string())
    } else {
        None
    }
}

pub fn encoded_claude_project_dir(cwd: &Path) -> String {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let input = canonical.to_string_lossy();
    let mut encoded = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            encoded.push(ch);
            last_dash = false;
        } else if !last_dash {
            encoded.push('-');
            last_dash = true;
        }
    }
    encoded
}

pub fn derived_transcript_path(cwd: &Path, session_id: &str) -> PathBuf {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(".claude")
        });
    config_dir
        .join("projects")
        .join(encoded_claude_project_dir(cwd))
        .join(format!("{session_id}.jsonl"))
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SdkMessage {
    ControlRequest {
        request_id: String,
        subtype: String,
        request: Value,
    },
    ControlResponse {
        request_id: String,
        subtype: String,
        response: Value,
        error: Option<String>,
    },
    ControlCancel {
        request_id: String,
    },
    System {
        subtype: String,
        session_id: Option<String>,
        transcript_path: Option<String>,
        capabilities: Vec<String>,
        raw: Value,
    },
    Assistant {
        session_id: Option<String>,
        transcript_path: Option<String>,
        message_id: Option<String>,
        uuid: Option<String>,
        content: Value,
        raw: Value,
    },
    User {
        session_id: Option<String>,
        content: Value,
        raw: Value,
    },
    StreamEvent {
        session_id: Option<String>,
        event: Value,
        raw: Value,
    },
    Result {
        subtype: String,
        session_id: Option<String>,
        transcript_path: Option<String>,
        result_text: Option<String>,
        is_error: bool,
        errors: Vec<String>,
        raw: Value,
    },
    Other(Value),
}

pub fn parse_sdk_line(line: &str) -> Result<SdkMessage, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|err| format!("invalid SDK JSON: {err}"))?;
    parse_sdk_value(value)
}

pub(crate) fn parse_sdk_value(value: Value) -> Result<SdkMessage, String> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "control_request" => {
            let request_id = string_field(&value, "request_id")
                .ok_or_else(|| "control_request missing request_id".to_string())?;
            let request = value.get("request").cloned().unwrap_or(Value::Null);
            let subtype = request
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(SdkMessage::ControlRequest {
                request_id,
                subtype,
                request,
            })
        }
        "control_response" => {
            let response = value.get("response").cloned().unwrap_or(Value::Null);
            let request_id = string_field(&response, "request_id")
                .or_else(|| string_field(&value, "request_id"))
                .ok_or_else(|| "control_response missing request_id".to_string())?;
            let subtype = string_field(&response, "subtype").unwrap_or_default();
            let error = string_field(&response, "error");
            Ok(SdkMessage::ControlResponse {
                request_id,
                subtype,
                response,
                error,
            })
        }
        "control_cancel_request" => Ok(SdkMessage::ControlCancel {
            request_id: string_field(&value, "request_id").unwrap_or_default(),
        }),
        "system" => Ok(SdkMessage::System {
            subtype: string_field(&value, "subtype").unwrap_or_default(),
            session_id: string_field(&value, "session_id"),
            transcript_path: string_field(&value, "transcript_path"),
            capabilities: value
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            raw: value,
        }),
        "assistant" => {
            let message = value.get("message").cloned().unwrap_or(Value::Null);
            Ok(SdkMessage::Assistant {
                session_id: string_field(&value, "session_id"),
                transcript_path: string_field(&value, "transcript_path"),
                message_id: string_field(&message, "id"),
                uuid: string_field(&value, "uuid"),
                content: message.get("content").cloned().unwrap_or(Value::Null),
                raw: value,
            })
        }
        "user" => {
            let message = value.get("message").cloned().unwrap_or(Value::Null);
            Ok(SdkMessage::User {
                session_id: string_field(&value, "session_id"),
                content: message
                    .get("content")
                    .cloned()
                    .unwrap_or_else(|| value.get("content").cloned().unwrap_or(Value::Null)),
                raw: value,
            })
        }
        "stream_event" => Ok(SdkMessage::StreamEvent {
            session_id: string_field(&value, "session_id"),
            event: value.get("event").cloned().unwrap_or(Value::Null),
            raw: value,
        }),
        "result" => {
            let subtype = string_field(&value, "subtype").unwrap_or_default();
            let errors = value
                .get("errors")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let is_error = subtype.starts_with("error")
                || value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            Ok(SdkMessage::Result {
                subtype,
                session_id: string_field(&value, "session_id"),
                transcript_path: string_field(&value, "transcript_path"),
                result_text: string_field(&value, "result"),
                is_error,
                errors,
                raw: value,
            })
        }
        _ => Ok(SdkMessage::Other(value)),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

pub fn stream_event_text_delta(event: &Value) -> Option<&str> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    match event_type {
        "content_block_delta" => {
            let delta = event.get("delta")?;
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => delta.get("text").and_then(Value::as_str),
                _ => None,
            }
        }
        _ => None,
    }
}

pub fn stream_event_is_end_turn(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("message_delta")
        && event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
            == Some("end_turn")
}

pub fn assistant_message_is_end_turn(raw: &Value) -> bool {
    raw.get("message")
        .and_then(|message| message.get("stop_reason"))
        .and_then(Value::as_str)
        == Some("end_turn")
}

pub fn next_request_id(counter: &mut u64) -> String {
    *counter += 1;
    let mut bytes = [0u8; 4];
    let _ = getrandom::getrandom(&mut bytes);
    format!("req_{}_{}", counter, hex4(bytes))
}

fn hex4(bytes: [u8; 4]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone)]
pub struct ClaudeSdkSpawnSpec {
    pub binary: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub fork: bool,
    pub stderr_log: PathBuf,
}

pub struct ClaudeSdkSession {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Result<SdkMessage, String>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
    request_counter: u64,
    pending: HashMap<String, PendingKind>,
    pub initialized: bool,
    pub interrupt_receipt: bool,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
}

enum PendingKind {
    Initialize,
    Interrupt,
}

impl ClaudeSdkSession {
    pub fn spawn(spec: ClaudeSdkSpawnSpec) -> Result<Self, String> {
        if let Some(parent) = spec.stderr_log.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create research log dir {}: {err}",
                    parent.display()
                )
            })?;
        }
        let mut stderr_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&spec.stderr_log)
            .map_err(|err| {
                format!(
                    "failed to open research log {}: {err}",
                    spec.stderr_log.display()
                )
            })?;
        let _ = writeln!(
            stderr_file,
            "qmux: research SDK spawn binary={} cwd={}",
            spec.binary,
            spec.cwd.display()
        );
        let _ = writeln!(
            stderr_file,
            "qmux: research SDK argv=-p --output-format stream-json --verbose --input-format stream-json --include-partial-messages --permission-prompt-tool stdio --permission-mode dontAsk --safe-mode --setting-sources= --disable-slash-commands --tools {READ_ONLY_TOOLS} --allowedTools {READ_ONLY_TOOLS} --strict-mcp-config --no-chrome"
        );
        let mut command = Command::new(&spec.binary);
        command
            .current_dir(&spec.cwd)
            .arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--permission-prompt-tool")
            .arg("stdio")
            .arg("--permission-mode")
            .arg("dontAsk")
            .arg("--safe-mode")
            .arg("--setting-sources=")
            .arg("--disable-slash-commands")
            .arg("--tools")
            .arg(READ_ONLY_TOOLS)
            .arg("--allowedTools")
            .arg(READ_ONLY_TOOLS)
            .arg("--strict-mcp-config")
            .arg("--no-chrome")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .env("CLAUDE_CODE_ENTRYPOINT", "sdk-qmux")
            .env("CLAUDE_AGENT_SDK_VERSION", "qmux")
            .env("CLAUDE_AGENT_SDK_CLIENT_APP", "qmux")
            .env("PWD", spec.cwd.as_os_str())
            .process_group(0);
        if let Some(model) = &spec.model {
            command.arg("--model").arg(model);
        }
        if let Some(effort) = &spec.effort {
            command.arg("--effort").arg(effort);
        }
        if let Some(resume) = &spec.resume {
            command.arg(format!("--resume={resume}"));
            if spec.fork {
                command.arg("--fork-session");
            }
        }
        if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
            eprintln!("qmux: research auth: ANTHROPIC_API_KEY (subscription Keychain skipped)");
        } else {
            eprintln!("qmux: research auth: Claude Code login");
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("failed to spawn Claude Code: {err}"))?;
        eprintln!("qmux: research SDK pid={}", child.id());
        let Some(stdin) = child.stdin.take() else {
            terminate_child_after_spawn_failure(&mut child);
            return Err("Claude Code stdin was not piped".to_string());
        };
        let Some(stdout) = child.stdout.take() else {
            terminate_child_after_spawn_failure(&mut child);
            return Err("Claude Code stdout was not piped".to_string());
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_child_after_spawn_failure(&mut child);
            return Err("Claude Code stderr was not piped".to_string());
        };
        let stderr_reader = match thread::Builder::new()
            .name("qmux-claude-sdk-stderr".into())
            .spawn(move || copy_bounded_stderr(stderr, stderr_file))
        {
            Ok(reader) => reader,
            Err(err) => {
                terminate_child_after_spawn_failure(&mut child);
                return Err(format!("failed to start Claude stderr reader: {err}"));
            }
        };
        let (tx, rx) = mpsc::sync_channel(MAX_PENDING_STDOUT_MESSAGES);
        if let Err(err) = thread::Builder::new()
            .name("qmux-claude-sdk-stdout".into())
            .spawn(move || read_stdout_lines(stdout, tx))
        {
            terminate_child_after_spawn_failure(&mut child);
            let _ = stderr_reader.join();
            return Err(format!("failed to start Claude stdout reader: {err}"));
        }
        Ok(Self {
            child,
            stdin: Some(stdin),
            events: rx,
            stderr_reader: Some(stderr_reader),
            request_counter: 0,
            pending: HashMap::new(),
            initialized: false,
            interrupt_receipt: false,
            session_id: None,
            transcript_path: None,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    pub fn write_initialize(&mut self) -> Result<String, String> {
        let request_id = next_request_id(&mut self.request_counter);
        let payload = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "initialize", "hooks": null }
        });
        self.write_json(&payload)?;
        self.pending
            .insert(request_id.clone(), PendingKind::Initialize);
        Ok(request_id)
    }

    pub fn send_user_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if !self.initialized {
            return Err("cannot send a user prompt before initialize succeeds".to_string());
        }
        self.write_json(&json!({
            "type": "user",
            "session_id": "",
            "parent_tool_use_id": null,
            "message": { "role": "user", "content": prompt }
        }))
    }

    pub fn write_interrupt(&mut self) -> Result<String, String> {
        let request_id = next_request_id(&mut self.request_counter);
        self.write_json(&json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        }))?;
        self.pending
            .insert(request_id.clone(), PendingKind::Interrupt);
        Ok(request_id)
    }

    pub fn reply_can_use_tool(
        &mut self,
        request_id: &str,
        allow: bool,
        input: &Value,
        message: &str,
    ) -> Result<(), String> {
        let response = if allow {
            json!({
                "behavior": "allow",
                "updatedInput": input
            })
        } else {
            json!({
                "behavior": "deny",
                "message": message
            })
        };
        self.write_json(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response
            }
        }))
    }

    pub fn reply_control_error(&mut self, request_id: &str, error: &str) -> Result<(), String> {
        self.write_json(&json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": request_id,
                "error": error
            }
        }))
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<SdkMessage>, String> {
        match self.events.recv_timeout(timeout) {
            Ok(Ok(message)) => self.observe(message).map(Some),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err("Claude Code stdout closed".to_string()),
        }
    }

    pub fn end_input(&mut self) {
        self.stdin.take();
    }

    pub fn finish_output(&mut self) {
        self.join_stderr_reader();
    }

    pub fn kill(&mut self) {
        let pid = self.child.id();
        terminate_process_tree(pid);
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_stderr_reader();
    }

    pub fn note_transcript_candidate(&mut self, cwd: &Path) {
        let Some(session_id) = self.session_id.as_deref() else {
            return;
        };
        if self.transcript_path.is_some() {
            return;
        }
        let candidate = derived_transcript_path(cwd, session_id);
        if candidate.is_file()
            && hook_transcript_path_acceptable(None, &candidate.display().to_string())
        {
            self.transcript_path = Some(candidate.display().to_string());
        }
    }

    fn observe(&mut self, message: SdkMessage) -> Result<SdkMessage, String> {
        let observed_session_id = match &message {
            SdkMessage::System { session_id, .. }
            | SdkMessage::Assistant { session_id, .. }
            | SdkMessage::User { session_id, .. }
            | SdkMessage::StreamEvent { session_id, .. }
            | SdkMessage::Result { session_id, .. } => session_id.as_deref(),
            _ => None,
        };
        // Stream envelopes are produced by the CLI rather than the model, but
        // still treat their durable identity as untrusted process output. A
        // mismatched or path-shaped id must never redirect transcript lookup.
        reconcile_session_id(&mut self.session_id, observed_session_id, "Claude")?;
        match &message {
            SdkMessage::ControlResponse {
                request_id,
                subtype,
                ..
            } => {
                if let Some(kind) = self.pending.remove(request_id)
                    && matches!(kind, PendingKind::Initialize)
                    && subtype == "success"
                {
                    self.initialized = true;
                }
            }
            SdkMessage::System {
                subtype,
                transcript_path,
                capabilities,
                ..
            } if subtype == "init" => {
                if let Some(path) = transcript_path
                    && hook_transcript_path_acceptable(self.transcript_path.as_deref(), path)
                {
                    self.transcript_path = Some(path.clone());
                }
                self.interrupt_receipt =
                    capabilities.iter().any(|cap| cap == "interrupt_receipt_v1");
            }
            SdkMessage::Result {
                transcript_path, ..
            }
            | SdkMessage::Assistant {
                transcript_path, ..
            } => {
                if let Some(path) = transcript_path
                    && hook_transcript_path_acceptable(self.transcript_path.as_deref(), path)
                {
                    self.transcript_path = Some(path.clone());
                }
            }
            _ => {}
        }
        Ok(message)
    }

    fn write_json(&mut self, value: &Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "Claude Code stdin is closed".to_string())?;
        writeln!(stdin, "{value}")
            .map_err(|err| format!("failed to write to Claude Code: {err}"))?;
        stdin
            .flush()
            .map_err(|err| format!("failed to flush Claude Code stdin: {err}"))
    }

    fn join_stderr_reader(&mut self) {
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for ClaudeSdkSession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill();
        } else {
            self.join_stderr_reader();
        }
    }
}

fn read_stdout_lines(stdout: impl std::io::Read, tx: SyncSender<Result<SdkMessage, String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut buf = Vec::new();
        match reader
            .by_ref()
            .take((MAX_STDOUT_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => break,
            Ok(_) => {
                if buf.len() > MAX_STDOUT_LINE_BYTES {
                    let _ = tx.send(Err("Claude Code stdout line exceeded 1 MB".to_string()));
                    break;
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = tx.send(parse_sdk_line(line));
            }
            Err(err) => {
                let _ = tx.send(Err(format!("failed to read Claude Code stdout: {err}")));
                break;
            }
        }
    }
}

fn copy_bounded_stderr(mut stderr: impl Read, mut log: impl Write) {
    let mut written = 0usize;
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        let read = match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        if written < MAX_STDERR_LOG_BYTES {
            let keep = read.min(MAX_STDERR_LOG_BYTES - written);
            let _ = log.write_all(&buf[..keep]);
            written += keep;
            if keep < read && !truncated {
                let _ = log.write_all(b"\nqmux: stderr log truncated at 4 MB\n");
                truncated = true;
            }
        } else if !truncated {
            let _ = log.write_all(b"\nqmux: stderr log truncated at 4 MB\n");
            truncated = true;
        }
    }
    let _ = log.flush();
}

fn signal_process_tree(pid: u32, descendants: &[u32], signal: libc::c_int) {
    let _ = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    for child_pid in descendants.iter().rev() {
        let _ = unsafe { libc::kill(*child_pid as libc::pid_t, signal) };
    }
}

pub fn terminate_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    // Keep the first descendant snapshot through SIGKILL. Once the group
    // leader exits, descendants that created their own process group may be
    // reparented and disappear from a fresh parent walk even though they are
    // still alive.
    let mut descendants = crate::pty::descendant_process_ids(pid);
    signal_process_tree(pid, &descendants, libc::SIGTERM);
    thread::sleep(Duration::from_millis(100));
    for child_pid in crate::pty::descendant_process_ids(pid) {
        if !descendants.contains(&child_pid) {
            descendants.push(child_pid);
        }
    }
    signal_process_tree(pid, &descendants, libc::SIGKILL);
}

fn terminate_child_after_spawn_failure(child: &mut Child) {
    terminate_process_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

pub fn research_can_use_tool(tool_name: &str, _input: &Value) -> Result<(), String> {
    if tool_name.starts_with("mcp__") {
        return Err("MCP tools are not enabled for research runs".to_string());
    }
    if ALLOWED_TOOL_NAMES.iter().any(|name| *name == tool_name) {
        return Ok(());
    }
    Err(format!(
        "tool {tool_name} is disabled for read-only research"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parse_claude_version_reads_semver_prefix() {
        let version = parse_claude_version("2.1.239 (Claude Code)\n").unwrap();
        assert_eq!(version.triple, (2, 1, 239));
        assert!(version.meets_floor());
        assert!(!parse_claude_version("2.0.9").unwrap().meets_floor());
        assert_eq!(parse_claude_version("v2.1.0").unwrap().triple, (2, 1, 0));
    }

    #[test]
    fn login_error_message_maps_cli_copy() {
        assert!(login_error_message("not logged in").is_some());
        assert!(login_error_message("Please run /login").is_some());
        assert!(login_error_message("authentication failed").is_some());
        assert!(login_error_message("git log output").is_none());
    }

    #[test]
    fn parse_initialize_success_does_not_require_interrupt_command() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1_abcd","response":{"commands":[{"name":"commit","description":"Create a git commit"}],"agents":[],"output_style":"default"}}}"#;
        match parse_sdk_line(line).unwrap() {
            SdkMessage::ControlResponse {
                request_id,
                subtype,
                response,
                ..
            } => {
                assert_eq!(request_id, "req_1_abcd");
                assert_eq!(subtype, "success");
                let commands = response["response"]["commands"].as_array().unwrap();
                assert_eq!(commands[0]["name"], "commit");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_can_use_tool_and_interrupt_frames() {
        let request = parse_sdk_line(
            r#"{"type":"control_request","request_id":"cli-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"git log -1"},"tool_use_id":"toolu_1"}}"#,
        )
        .unwrap();
        match request {
            SdkMessage::ControlRequest {
                request_id,
                subtype,
                request,
            } => {
                assert_eq!(request_id, "cli-1");
                assert_eq!(subtype, "can_use_tool");
                assert_eq!(request["tool_name"], "Bash");
            }
            other => panic!("{other:?}"),
        }
        let interrupt = json!({
            "type": "control_request",
            "request_id": "req_2_aa",
            "request": { "subtype": "interrupt" }
        });
        assert_eq!(interrupt["request"]["subtype"], "interrupt");
    }

    #[test]
    fn stream_event_extracts_text_delta() {
        let event = json!({
            "type": "content_block_delta",
            "delta": { "type": "text_delta", "text": "Hello" }
        });
        assert_eq!(stream_event_text_delta(&event), Some("Hello"));
    }

    #[test]
    fn end_turn_is_detected_in_stream_and_assistant_frames() {
        assert_eq!(RESULT_AFTER_END_TURN_STALL, Duration::from_secs(30));
        assert!(stream_event_is_end_turn(&json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" }
        })));
        assert!(!stream_event_is_end_turn(&json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use" }
        })));
        assert!(assistant_message_is_end_turn(&json!({
            "type": "assistant",
            "message": { "stop_reason": "end_turn" }
        })));
    }

    #[test]
    fn stdout_reader_rejects_an_oversized_unterminated_record_at_the_cap() {
        let input = vec![b'x'; MAX_STDOUT_LINE_BYTES + 4096];
        let (tx, rx) = mpsc::sync_channel(1);
        read_stdout_lines(std::io::Cursor::new(input), tx);
        let err = rx.recv().unwrap().unwrap_err();
        assert!(err.contains("exceeded 1 MB"), "{err}");
    }

    #[test]
    fn stderr_copy_is_bounded_while_still_draining_input() {
        let input = vec![b'x'; MAX_STDERR_LOG_BYTES + 4096];
        let mut output = Vec::new();
        copy_bounded_stderr(std::io::Cursor::new(input), &mut output);
        assert!(output.len() < MAX_STDERR_LOG_BYTES + 128);
        assert!(String::from_utf8_lossy(&output).contains("stderr log truncated at 4 MB"));
    }

    #[test]
    fn assistant_and_result_frames_preserve_transcript_paths() {
        let assistant = parse_sdk_line(
            r#"{"type":"assistant","session_id":"s","transcript_path":"/tmp/s.jsonl","message":{"content":[]}}"#,
        )
        .unwrap();
        assert!(matches!(
            assistant,
            SdkMessage::Assistant { transcript_path: Some(path), .. } if path == "/tmp/s.jsonl"
        ));
        let result = parse_sdk_line(
            r#"{"type":"result","subtype":"success","transcript_path":"/tmp/s.jsonl"}"#,
        )
        .unwrap();
        assert!(matches!(
            result,
            SdkMessage::Result { transcript_path: Some(path), .. } if path == "/tmp/s.jsonl"
        ));
    }

    #[test]
    fn captured_claude_2_1_240_fixture_matches_the_parser_contract() {
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/claude-sdk-2.1.240.json")).unwrap();
        assert_eq!(fixture["capture"]["claudeCodeVersion"], "2.1.240");
        assert_eq!(fixture["capture"]["sanitized"], true);

        let parsed = fixture["success"]["stdout"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(parse_sdk_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(matches!(
            &parsed[0],
            SdkMessage::ControlResponse { request_id, subtype, .. }
                if request_id == "req_1_fixture" && subtype == "success"
        ));
        assert!(matches!(
            &parsed[1],
            SdkMessage::System { session_id: Some(session_id), capabilities, .. }
                if session_id == "fixture-session"
                    && capabilities.iter().any(|capability| capability == "interrupt_receipt_v1")
        ));
        let streamed_text = parsed
            .iter()
            .filter_map(|message| match message {
                SdkMessage::StreamEvent { event, .. } => stream_event_text_delta(event),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(streamed_text, "fixture-ok");
        assert!(parsed.iter().any(|message| matches!(
            message,
            SdkMessage::StreamEvent { event, .. } if stream_event_is_end_turn(event)
        )));
        assert!(parsed.iter().any(|message| matches!(
            message,
            SdkMessage::Assistant { raw, .. }
                if !assistant_message_is_end_turn(raw)
        )));
        assert!(matches!(
            parsed.last(),
            Some(SdkMessage::Result { subtype, result_text: Some(result), is_error: false, .. })
                if subtype == "success" && result == "fixture-ok"
        ));

        let interrupt_responses = fixture["interrupt"]["stdout"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(parse_sdk_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(matches!(
            interrupt_responses.last(),
            Some(SdkMessage::ControlResponse { request_id, subtype, .. })
                if request_id == "req_2_fixture" && subtype == "success"
        ));
    }

    #[test]
    fn research_can_use_tool_only_allows_read_only_builtins() {
        assert!(research_can_use_tool("Bash", &json!({"command": "git log -1"})).is_err());
        assert!(research_can_use_tool("Bash", &json!({"command": "rm -rf /"})).is_err());
        assert!(research_can_use_tool("mcp__demo__tool", &json!({})).is_err());
        assert!(research_can_use_tool("Task", &json!({})).is_err());
        assert!(research_can_use_tool("Edit", &json!({})).is_err());
        assert!(research_can_use_tool("Write", &json!({})).is_err());
        assert!(research_can_use_tool("Read", &json!({"file_path": "/tmp/x"})).is_ok());
        assert!(research_can_use_tool("MysteryTool", &json!({})).is_err());
    }

    #[test]
    fn encoded_project_dir_replaces_non_alnum_runs() {
        assert_eq!(
            encoded_claude_project_dir(Path::new("/Users/foo/proj")),
            "-Users-foo-proj"
        );
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("qmux-claude-sdk-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fake_claude(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, os, sys
required = ["-p", "--output-format", "stream-json", "--input-format", "stream-json", "--include-partial-messages", "--permission-prompt-tool", "stdio", "--permission-mode", "dontAsk", "--safe-mode", "--setting-sources=", "--disable-slash-commands", "--tools", "Read,Grep,Glob,WebSearch,WebFetch,NotebookRead,TodoWrite", "--allowedTools", "--strict-mcp-config", "--no-chrome"]
for value in required:
    assert value in sys.argv, (value, sys.argv)
for forbidden in ["acceptEdits", "--setting-sources=user,project", "--disallowedTools", "--plugin-dir"]:
    assert forbidden not in sys.argv, (forbidden, sys.argv)
assert "CLAUDECODE" not in os.environ
assert os.environ.get("CLAUDE_CODE_ENTRYPOINT") == "sdk-qmux"
if os.path.exists("expect-optional"):
    assert "--model" in sys.argv and "sonnet" in sys.argv
    assert "--effort" in sys.argv and "high" in sys.argv
    assert "--resume=sess-parent" in sys.argv
    assert "--fork-session" in sys.argv
def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(1)
    return json.loads(line)
init = read()
assert init.get("type") == "control_request"
assert init.get("request", {}).get("subtype") == "initialize"
rid = init["request_id"]
print(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": rid,
        "response": {
            "commands": [{"name": "commit", "description": "Create a git commit"}],
            "agents": [],
            "output_style": "default"
        }
    }
}), flush=True)
user = read()
assert user.get("type") == "user"
print(json.dumps({
    "type": "system",
    "subtype": "init",
    "session_id": "sess-1",
    "capabilities": ["interrupt_receipt_v1"]
}), flush=True)
print(json.dumps({
    "type": "control_request",
    "request_id": "cli-bash",
    "request": {
        "subtype": "can_use_tool",
        "tool_name": "Bash",
        "input": {"command": "git status"},
        "tool_use_id": "toolu_1"
    }
}), flush=True)
perm = read()
assert perm.get("type") == "control_response"
print(json.dumps({
    "type": "assistant",
    "session_id": "sess-1",
    "uuid": "asst-1",
    "message": {"id": "msg-1", "content": [{"type": "text", "text": "done"}]}
}), flush=True)
print(json.dumps({
    "type": "result",
    "subtype": "success",
    "session_id": "sess-1",
    "result": "done"
}), flush=True)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn session_requires_initialize_before_user_and_handles_can_use_tool() {
        let dir = temp_dir();
        let binary = write_fake_claude(&dir);
        fs::write(dir.join("expect-optional"), b"").unwrap();
        let mut session = ClaudeSdkSession::spawn(ClaudeSdkSpawnSpec {
            binary: binary.display().to_string(),
            cwd: dir.clone(),
            model: Some("sonnet".to_string()),
            effort: Some("high".to_string()),
            resume: Some("sess-parent".to_string()),
            fork: true,
            stderr_log: dir.join("stderr.log"),
        })
        .unwrap();
        session.write_initialize().unwrap();
        let mut saw_init_ok = false;
        let mut saw_can_use = false;
        let mut saw_result = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !saw_result {
            let Some(message) = session.recv_timeout(Duration::from_millis(200)).unwrap() else {
                continue;
            };
            match message {
                SdkMessage::ControlResponse { subtype, .. } if subtype == "success" => {
                    saw_init_ok = true;
                    session.send_user_prompt("hello").unwrap();
                }
                SdkMessage::ControlRequest {
                    request_id,
                    subtype,
                    request,
                } if subtype == "can_use_tool" => {
                    saw_can_use = true;
                    let input = request.get("input").cloned().unwrap_or(json!({}));
                    session
                        .reply_can_use_tool(&request_id, true, &input, "")
                        .unwrap();
                }
                SdkMessage::Result { subtype, .. } => {
                    assert_eq!(subtype, "success");
                    saw_result = true;
                }
                _ => {}
            }
        }
        assert!(saw_init_ok);
        assert!(saw_can_use);
        assert!(saw_result);
        assert!(session.initialized);
        assert_eq!(session.session_id.as_deref(), Some("sess-1"));
        session.end_input();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn user_prompt_before_initialize_is_rejected() {
        let dir = temp_dir();
        let binary = write_fake_claude(&dir);
        let mut session = ClaudeSdkSession::spawn(ClaudeSdkSpawnSpec {
            binary: binary.display().to_string(),
            cwd: dir.clone(),
            model: None,
            effort: None,
            resume: None,
            fork: false,
            stderr_log: dir.join("stderr.log"),
        })
        .unwrap();
        let err = session.send_user_prompt("too soon").unwrap_err();
        assert!(err.contains("before initialize"));
        session.kill();
        fs::remove_dir_all(dir).ok();
    }

    fn write_fake_claude_interrupt(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude-interrupt");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys, time
def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(1)
    return json.loads(line)
init = read()
assert init.get("request", {}).get("subtype") == "initialize"
rid = init["request_id"]
print(json.dumps({
    "type": "control_response",
    "response": {"subtype": "success", "request_id": rid, "response": {"commands": []}}
}), flush=True)
user = read()
assert user.get("type") == "user"
print(json.dumps({
    "type": "system",
    "subtype": "init",
    "session_id": "sess-int",
    "capabilities": ["interrupt_receipt_v1"]
}), flush=True)
interrupt = read()
assert interrupt.get("request", {}).get("subtype") == "interrupt"
print(json.dumps({
    "type": "control_response",
    "response": {"subtype": "success", "request_id": interrupt["request_id"], "response": {}}
}), flush=True)
time.sleep(8)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn session_acks_interrupt_after_initialize() {
        let dir = temp_dir();
        let binary = write_fake_claude_interrupt(&dir);
        let mut session = ClaudeSdkSession::spawn(ClaudeSdkSpawnSpec {
            binary: binary.display().to_string(),
            cwd: dir.clone(),
            model: None,
            effort: None,
            resume: None,
            fork: false,
            stderr_log: dir.join("stderr.log"),
        })
        .unwrap();
        session.write_initialize().unwrap();
        let mut saw_init_ok = false;
        let mut saw_interrupt_ack = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !saw_interrupt_ack {
            let Some(message) = session.recv_timeout(Duration::from_millis(200)).unwrap() else {
                continue;
            };
            match message {
                SdkMessage::ControlResponse { subtype, .. } if subtype == "success" => {
                    if !saw_init_ok {
                        saw_init_ok = true;
                        session.send_user_prompt("hello").unwrap();
                        session.write_interrupt().unwrap();
                    } else {
                        saw_interrupt_ack = true;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_init_ok);
        assert!(saw_interrupt_ack);
        session.kill();
        fs::remove_dir_all(dir).ok();
    }
}

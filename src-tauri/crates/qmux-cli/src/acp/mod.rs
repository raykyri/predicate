//! `qmux acp` — an Agent Client Protocol client that runs inside a qmux pane.
//!
//! Every other qmux adapter drives a vendor TUI: qmux spawns it in a pty and
//! reads a transcript the vendor writes. ACP agents have no TUI. They are
//! subprocesses speaking newline-delimited JSON-RPC over stdio, and the *client*
//! owns presentation, the filesystem, permissions, and terminals.
//!
//! So this bridge is the missing TUI. It runs in the pane, speaks ACP to the
//! agent on one side, and on the other side does what the vendor TUIs do for
//! qmux: renders a readable session, accepts typed prompts on stdin (which is
//! how the qmux composer delivers turns), appends a qmux-format JSONL
//! transcript for the sidebar, and posts lifecycle hooks to the control socket
//! so the agent's status tracks the same way every other adapter's does.
//!
//! The payoff is that one adapter covers every ACP agent — a new one is a
//! config entry, not Rust.

mod elicitation;
mod terminal;

use crate::request_silent;
use elicitation::{Field, FieldKind};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use terminal::TerminalRegistry;

/// The ACP major version this client implements.
const PROTOCOL_VERSION: i64 = 1;

/// Set by the SIGINT handler. Polled by the cancel watcher rather than acted on
/// in the handler itself, which must stay async-signal-safe.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// How long to let an agent exit on its own after its stdin closes, before
/// killing it. A second is generous for a process whose only remaining job is
/// to flush state, and the pane is already closing either way.
const SHUTDOWN_POLLS: u32 = 20;
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Launch configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct BridgeConfig {
    display_name: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    /// Where to record turns. A path when the sidebar can see this filesystem;
    /// `None` when it cannot and records must be streamed to qmux instead.
    transcript: Option<PathBuf>,
    /// Where the agent's stderr goes. Derived from the transcript locally, but
    /// a streaming bridge has no local transcript to hang it off.
    log: PathBuf,
    initial_prompt: Option<String>,
    load_session: Option<String>,
}

impl BridgeConfig {
    fn from_env() -> Result<Self, String> {
        let command = required_env("QMUX_ACP_COMMAND")?;
        let cwd = PathBuf::from(required_env("QMUX_ACP_CWD")?);
        if !cwd.is_absolute() {
            // ACP requires an absolute `cwd` on session/new; catching it here
            // names the misconfiguration instead of letting the agent reject it.
            return Err(format!(
                "QMUX_ACP_CWD must be an absolute path, got {}",
                cwd.display()
            ));
        }
        Ok(Self {
            display_name: env::var("QMUX_ACP_NAME").unwrap_or_else(|_| command.clone()),
            command,
            args: json_env_array("QMUX_ACP_ARGS")?,
            env: json_env_object("QMUX_ACP_ENV")?,
            cwd,
            // Streaming is what a remote bridge uses: it has no access to the
            // filesystem the sidebar tails, so a path here would be a file
            // nobody reads.
            transcript: optional_env("QMUX_ACP_TRANSCRIPT")
                .filter(|_| optional_env("QMUX_ACP_TRANSCRIPT_STREAM").is_none())
                .map(PathBuf::from),
            log: log_path(),
            initial_prompt: optional_env("QMUX_ACP_PROMPT"),
            load_session: optional_env("QMUX_ACP_LOAD_SESSION"),
        })
    }
}

/// Where the agent's stderr is parked. Beside the transcript when there is
/// one, otherwise a temp file — the log must land somewhere writable on
/// *this* machine, which a streamed transcript's path is not.
fn log_path() -> PathBuf {
    if let Some(explicit) = optional_env("QMUX_ACP_LOG") {
        return PathBuf::from(explicit);
    }
    match optional_env("QMUX_ACP_TRANSCRIPT")
        .filter(|_| optional_env("QMUX_ACP_TRANSCRIPT_STREAM").is_none())
    {
        Some(transcript) => PathBuf::from(transcript).with_extension("agent.log"),
        None => env::temp_dir().join(format!("qmux-acp-{}.log", std::process::id())),
    }
}

fn required_env(key: &str) -> Result<String, String> {
    optional_env(key).ok_or_else(|| format!("{key} is not set; launch this through qmux"))
}

fn optional_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn json_env_array(key: &str) -> Result<Vec<String>, String> {
    let Some(raw) = optional_env(key) else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|err| format!("{key} is not a JSON array of strings: {err}"))
}

fn json_env_object(key: &str) -> Result<Vec<(String, String)>, String> {
    let Some(raw) = optional_env(key) else {
        return Ok(Vec::new());
    };
    let map: HashMap<String, String> = serde_json::from_str(&raw)
        .map_err(|err| format!("{key} is not a JSON object of strings: {err}"))?;
    let mut pairs: Vec<(String, String)> = map.into_iter().collect();
    pairs.sort();
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// JSON-RPC connection
// ---------------------------------------------------------------------------

/// One in-flight outbound request, waiting for the reader thread to route a
/// response back to whoever called.
type Pending = Sender<Result<Value, String>>;

struct Connection {
    /// `None` once shut down. Held as an `Option` purely so the pipe can be
    /// dropped — the EOF on the agent's stdin is what asks it to exit.
    stdin: Mutex<Option<ChildStdin>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, Pending>>,
}

impl Connection {
    fn new(stdin: ChildStdin) -> Self {
        Self {
            stdin: Mutex::new(Some(stdin)),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// ACP's stdio transport is newline-delimited: one JSON value per line,
    /// never an embedded newline. `to_string` on a `Value` satisfies both.
    fn send(&self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(message)
            .map_err(|err| format!("failed to encode ACP message: {err}"))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().unwrap_or_else(|err| err.into_inner());
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| "the ACP session is shutting down".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("failed to write to the ACP agent: {err}"))
    }

    fn close_stdin(&self) {
        self.stdin
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request_async(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Receiver<Result<Value, String>>, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(id, tx);
        if let Err(err) =
            self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
        {
            self.pending
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .remove(&id);
            return Err(err);
        }
        Ok(rx)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_async(method, params)?
            .recv()
            .map_err(|_| format!("the ACP agent exited while handling {method}"))?
    }

    /// Routes a response to its waiter. An unknown id means the agent answered
    /// something we never asked; drop it rather than tearing down the session.
    fn resolve(&self, id: i64, outcome: Result<Value, String>) {
        if let Some(pending) = self
            .pending
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&id)
        {
            let _ = pending.send(outcome);
        }
    }

    /// Fails every in-flight request. Called when the agent's stdout closes, so
    /// blocked callers get an error instead of hanging forever.
    fn fail_all(&self, reason: &str) {
        let pending =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(|err| err.into_inner()));
        for (_, sender) in pending {
            let _ = sender.send(Err(reason.to_string()));
        }
    }

    fn respond_ok(&self, id: Value, result: Value) {
        let _ = self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_err(&self, id: Value, code: i64, message: String) {
        let _ = self.send(
            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        );
    }
}

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

/// Appends the qmux-format JSONL the sidebar tails. The format is qmux's own —
/// ACP has no transcript-on-disk concept — so the shape here is exactly what
/// `AcpAdapter::parse_transcript_line` reads back.
enum Transcript {
    /// The bridge and the sidebar share a filesystem, so write straight to the
    /// file the tailer watches.
    File(Mutex<Option<File>>),
    /// The bridge is on another machine. Records go to qmux over the control
    /// socket, which appends them to the *local* transcript and reads them back
    /// through the same tail — nothing downstream learns where the agent ran.
    ///
    /// The path is never sent: qmux resolves it from the authenticated pane, so
    /// this side cannot aim writes anywhere.
    Stream,
}

impl Transcript {
    fn open(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Transcript::File(Mutex::new(file))
    }

    fn write(&self, value: Value) {
        // A transcript that can't be recorded is a degraded sidebar, not a
        // broken session: the pane still shows everything either way.
        let Ok(line) = serde_json::to_string(&value) else {
            return;
        };
        match self {
            Transcript::File(file) => {
                let mut guard = file.lock().unwrap_or_else(|err| err.into_inner());
                let Some(file) = guard.as_mut() else {
                    return;
                };
                let _ = file.write_all(line.as_bytes());
                let _ = file.write_all(b"\n");
                let _ = file.flush();
            }
            Transcript::Stream => {
                let _ = request_silent("transcript.append", json!({ "lines": [line] }));
            }
        }
    }

    fn turn(&self, session_id: Option<&str>, role: &str, native_id: Option<&str>, blocks: Value) {
        self.write(json!({
            "type": "turn",
            "sessionId": session_id,
            "role": role,
            "nativeId": native_id,
            "timestamp": now_ms(),
            "blocks": blocks,
        }));
    }

    fn lifecycle(&self, event: &str) {
        self.write(json!({ "type": "lifecycle", "event": event, "timestamp": now_ms() }));
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Bridge state
// ---------------------------------------------------------------------------

/// Something the agent needs a human answer for. Raised from a handler thread
/// and serviced by the main loop, which is the only thread that reads stdin.
/// Something the agent needs a human answer for, raised from a handler thread
/// onto the main thread — the only one that reads the pane's stdin.
struct Interaction {
    heading: String,
    kind: InteractionKind,
    /// The JSON-RPC `result` to answer the agent's request with. Each kind
    /// builds its own shape, so the servicer stays a renderer.
    reply: Sender<Value>,
}

enum InteractionKind {
    /// `session/request_permission`: pick one of the agent's options.
    Permission { options: Vec<(String, String)> },
    /// `elicitation/create` in form mode: fill in a flat schema.
    Form { fields: Vec<Field> },
    /// `elicitation/create` in url mode: consent to opening a link.
    Url { url: String },
}

enum MainEvent {
    PromptDone(Result<Value, String>),
    Interact(Interaction),
}

/// One finished assistant message: its accumulated text and the `messageId` it
/// carried, if any.
type FinishedMessage = (String, Option<String>);

/// Streaming assistant text, buffered so the sidebar gets one turn per message
/// instead of one per chunk. ACP groups chunks by `messageId`; a changed id (or
/// any non-text event) ends the current message.
#[derive(Default)]
struct MessageBuffer {
    message_id: Option<String>,
    text: String,
}

impl MessageBuffer {
    /// Appends a chunk, returning the previous message when this chunk starts a
    /// new one. Agents that never send `messageId` keep appending to a single
    /// message, which the end of the turn flushes.
    fn push(&mut self, message_id: Option<&str>, text: &str) -> Option<FinishedMessage> {
        let changed = message_id.is_some() && self.message_id.as_deref() != message_id;
        let finished = (changed && !self.text.is_empty())
            .then(|| (std::mem::take(&mut self.text), self.message_id.take()));
        if let Some(message_id) = message_id {
            self.message_id = Some(message_id.to_string());
        }
        self.text.push_str(text);
        finished
    }

    /// Ends the current message. `None` when there is nothing worth recording,
    /// so a turn that streamed only whitespace does not produce an empty turn.
    fn take(&mut self) -> Option<FinishedMessage> {
        let native_id = self.message_id.take();
        let text = std::mem::take(&mut self.text);
        (!text.trim().is_empty()).then_some((text, native_id))
    }
}

struct Bridge {
    connection: Connection,
    transcript: Transcript,
    terminals: TerminalRegistry,
    session_id: Mutex<Option<String>>,
    /// Whether a `session/prompt` is outstanding. Gates the SIGINT watcher, so
    /// Ctrl-C at an idle prompt does nothing rather than cancelling a
    /// non-existent turn.
    turn_active: AtomicBool,
    /// True while `session/load` is replaying history. The replay arrives as
    /// ordinary `session/update` notifications, but the transcript it describes
    /// is the very file we are appending to — recording it again would double
    /// every turn in the sidebar on each resume. The pane still renders the
    /// replay; only the write is suppressed.
    replaying: AtomicBool,
    /// Cleared by the reader thread when the agent's stdout closes. Separates
    /// "this turn failed" from "there is no agent left to talk to".
    agent_alive: AtomicBool,
    buffer: Mutex<MessageBuffer>,
    /// Outstanding url-mode elicitation ids. The spec requires ignoring a
    /// `elicitation/complete` for an id we never issued or already closed.
    pending_elicitations: Mutex<HashSet<String>>,
    events: Sender<MainEvent>,
    cwd: PathBuf,
}

impl Bridge {
    fn session(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    /// Appends one turn, unless we are replaying history that is already on disk.
    fn record(&self, role: &str, native_id: Option<&str>, blocks: Value) {
        if self.replaying.load(Ordering::SeqCst) {
            return;
        }
        self.transcript
            .turn(self.session().as_deref(), role, native_id, blocks);
    }

    fn record_message(&self, finished: Option<FinishedMessage>) {
        if let Some((text, native_id)) = finished {
            self.record(
                "assistant",
                native_id.as_deref(),
                json!([{ "type": "text", "text": text }]),
            );
        }
    }

    /// Ends the current streamed message, writing it to the transcript. Called
    /// before any event that interrupts prose (a tool call, a turn ending).
    fn flush_message(&self) {
        let finished = self
            .buffer
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        self.record_message(finished);
    }

    fn push_chunk(&self, message_id: Option<&str>, text: &str) {
        // The buffer mutation happens under one lock so a concurrent
        // end-of-turn flush can't split a message in two; the record happens
        // after it is released, since recording takes other locks.
        let finished = self
            .buffer
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(message_id, text);
        self.record_message(finished.filter(|(text, _)| !text.trim().is_empty()));
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn styled(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn dim(text: &str) -> String {
    styled("2", text)
}

fn bold(text: &str) -> String {
    styled("1", text)
}

fn print_now(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(args: Vec<String>) -> Result<(), String> {
    for arg in &args {
        if arg == "--help" || arg == "-h" {
            println!(
                "usage: qmux acp\n\nRuns the ACP agent selected at launch. Configure agents under\nadapters.acp in qmux.config.json; qmux sets the environment this reads."
            );
            return Ok(());
        }
    }

    let config = BridgeConfig::from_env()?;
    let transcript = match &config.transcript {
        Some(path) => Transcript::open(path),
        None => Transcript::Stream,
    };

    // ACP reserves the agent's stdout for protocol traffic and allows stderr for
    // logging. Sending that log to the pane would corrupt the rendered session,
    // so park it beside the transcript where it stays greppable after a crash.
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.log)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());

    let mut child = spawn_agent(&config, log)?;
    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "the ACP agent did not expose stdin".to_string())?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| "the ACP agent did not expose stdout".to_string())?;

    let (events, event_rx) = channel();
    let bridge = Arc::new(Bridge {
        connection: Connection::new(child_stdin),
        transcript,
        terminals: TerminalRegistry::new(),
        session_id: Mutex::new(None),
        turn_active: AtomicBool::new(false),
        replaying: AtomicBool::new(false),
        agent_alive: AtomicBool::new(true),
        buffer: Mutex::new(MessageBuffer::default()),
        pending_elicitations: Mutex::new(HashSet::new()),
        events,
        cwd: config.cwd.clone(),
    });

    let reader_bridge = Arc::clone(&bridge);
    let reader = thread::spawn(move || read_agent_messages(reader_bridge, child_stdout));

    install_interrupt_handler();
    let watcher_bridge = Arc::clone(&bridge);
    thread::spawn(move || watch_for_interrupts(watcher_bridge));

    let outcome = session_loop(&bridge, &config, &event_rx);

    // ACP's shutdown is "close stdin, then terminate the subprocess". Give the
    // agent that EOF and a moment to exit on its own — many flush state on the
    // way out — before killing it.
    bridge.connection.close_stdin();
    for _ in 0..SHUTDOWN_POLLS {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(SHUTDOWN_POLL_INTERVAL),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    // Only now can in-flight callers be failed: the reader thread ends when the
    // agent's stdout closes, and it fails them itself if it gets there first.
    bridge.connection.fail_all("the session ended");
    let _ = reader.join();

    outcome
}

fn spawn_agent(config: &BridgeConfig, log: Stdio) -> Result<Child, String> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log);
    for (key, value) in &config.env {
        command.env(key, value);
    }
    command.spawn().map_err(|err| {
        format!(
            "failed to start ACP agent '{}': {err}. Check adapters.acp in qmux.config.json.",
            config.command
        )
    })
}

/// Runs initialization, then one turn per line the user (or the qmux composer)
/// sends. Returns when stdin closes.
fn session_loop(
    bridge: &Arc<Bridge>,
    config: &BridgeConfig,
    events: &Receiver<MainEvent>,
) -> Result<(), String> {
    println!(
        "{}",
        dim(&format!(
            "qmux acp · {} · {}",
            config.display_name,
            config.cwd.display()
        ))
    );

    let init = bridge.connection.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true,
                // Agents MUST NOT send `type: "boolean"` config options unless
                // the client says it can render them. qmux only displays them
                // for now, which is enough to advertise: a boolean shown as
                // on/off is rendered correctly even though it isn't yet
                // settable.
                "session": { "configOptions": { "boolean": {} } },
                // Both modes must be named explicitly and non-null; an
                // empty `elicitation` object advertises neither. url mode
                // is the one that matters most — it is where the spec
                // sends anything sensitive, and the browser overlay gives
                // it a context neither qmux nor the model can read.
                "elicitation": { "form": {}, "url": {} },
            },
            "clientInfo": { "name": "qmux", "title": "qmux", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;

    // A version we don't implement is worth saying out loud, but the agent
    // picked it and the overlap is usually workable, so carry on rather than
    // refusing to start.
    if let Some(version) = init.get("protocolVersion").and_then(Value::as_i64)
        && version != PROTOCOL_VERSION
    {
        println!(
            "{}",
            dim(&format!(
                "note: agent negotiated ACP v{version}; qmux implements v{PROTOCOL_VERSION}"
            ))
        );
    }
    if init
        .get("authMethods")
        .and_then(Value::as_array)
        .is_some_and(|methods| !methods.is_empty())
    {
        println!(
            "{}",
            dim(
                "note: this agent advertises auth methods; qmux assumes you are already signed in via its CLI"
            )
        );
    }

    let session = start_session(bridge, config)?;
    *bridge
        .session_id
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(session.id.clone());
    let _ = request_silent(
        "hook.notify",
        hook_payload("SessionStart", json!({ "session_id": session.id })),
    );
    if let Some(options) = session.config_options {
        report_config_options(&options, false);
    }

    if let Some(prompt) = config.initial_prompt.clone() {
        run_turn(bridge, events, &prompt)?;
    }

    let stdin = std::io::stdin();
    loop {
        print_now(&bold("\n› "));
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err) => return Err(format!("failed to read input: {err}")),
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "/exit" | "/quit") {
            return Ok(());
        }
        run_turn(bridge, events, prompt)?;
    }
}

/// A session that is open and ready for prompts, plus whatever configuration
/// the agent chose to expose for it.
struct StartedSession {
    id: String,
    /// The agent's `configOptions`, verbatim. Absent when the agent exposes
    /// none — and, in ACP v1, after a resume: `session/load` answers with
    /// `null`, so a resumed session has no configuration until the agent
    /// pushes a `config_option_update`.
    config_options: Option<Value>,
}

fn start_session(bridge: &Arc<Bridge>, config: &BridgeConfig) -> Result<StartedSession, String> {
    let params = json!({ "cwd": config.cwd.display().to_string(), "mcpServers": [] });

    // Resuming is best-effort: `loadSession` is an optional agent capability and
    // a stale id is common after a restart. Falling back to a fresh session
    // beats refusing to open the pane.
    if let Some(session_id) = config.load_session.as_deref() {
        let mut load = params.clone();
        load["sessionId"] = json!(session_id);
        // The agent answers `session/load` only after it has streamed the whole
        // history back, so this flag brackets exactly the replay.
        bridge.replaying.store(true, Ordering::SeqCst);
        let loaded = bridge.connection.request("session/load", load);
        // The last replayed message is still buffered; drop it rather than
        // recording it, then reopen recording for live turns.
        bridge.flush_message();
        bridge.replaying.store(false, Ordering::SeqCst);
        match loaded {
            Ok(result) => {
                println!("{}", dim(&format!("\nresumed session {session_id}")));
                return Ok(StartedSession {
                    id: session_id.to_string(),
                    config_options: config_options_of(&result),
                });
            }
            Err(err) => println!(
                "{}",
                dim(&format!(
                    "could not resume {session_id} ({err}); starting a new session"
                ))
            ),
        }
    }

    let result = bridge.connection.request("session/new", params)?;
    let id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the ACP agent's session/new response had no sessionId".to_string())?;
    Ok(StartedSession {
        config_options: config_options_of(&result),
        id,
    })
}

/// Pulls a non-empty `configOptions` array out of a session-setup result.
fn config_options_of(result: &Value) -> Option<Value> {
    let options = result.get("configOptions")?.as_array()?;
    (!options.is_empty()).then(|| Value::Array(options.clone()))
}

/// Shows the agent's configuration in the pane and hands it to qmux, which maps
/// the `model` and `thought_level` categories onto the fields it already
/// displays. Both the setup response and `config_option_update` carry the
/// *complete* list, so this always replaces rather than merges.
///
/// `mid_stream` is set when this interrupts a turn: streamed assistant text has
/// no trailing newline, so the summary would otherwise land on the end of the
/// agent's sentence.
fn report_config_options(options: &Value, mid_stream: bool) {
    let entries = options.as_array().map(Vec::as_slice).unwrap_or_default();
    if entries.is_empty() {
        return;
    }
    let summary = entries
        .iter()
        .filter_map(|option| {
            let name = option.get("name").and_then(Value::as_str)?;
            Some(format!("{name}: {}", current_value_label(option)))
        })
        .collect::<Vec<_>>();
    if !summary.is_empty() {
        let lead = if mid_stream { "\n" } else { "" };
        println!("{lead}{}", dim(&summary.join(" · ")));
    }
    let _ = request_silent(
        "hook.notify",
        hook_payload("ConfigOptions", json!({ "configOptions": options })),
    );
}

/// The human-readable form of an option's current value: the matching choice's
/// `name` for a select, the raw value otherwise. Agents pick opaque ids
/// (`"model-1"`), so showing the label is the difference between a useful
/// header and a meaningless one.
///
/// qmux applies the same rule to render these in the UI
/// (`workspace::AcpConfigOption::current_label`). The two live in different
/// crates and cannot share code; change them together.
fn current_value_label(option: &Value) -> String {
    let current = option.get("currentValue").unwrap_or(&Value::Null);
    if let Some(current) = current.as_str() {
        let labelled = option
            .get("options")
            .and_then(Value::as_array)
            .and_then(|choices| {
                choices
                    .iter()
                    .find(|choice| choice.get("value").and_then(Value::as_str) == Some(current))
            })
            .and_then(|choice| choice.get("name").and_then(Value::as_str));
        return labelled.unwrap_or(current).to_string();
    }
    match current {
        Value::Bool(true) => "on".to_string(),
        Value::Bool(false) => "off".to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One prompt turn: submit, then service agent traffic until the response lands.
fn run_turn(
    bridge: &Arc<Bridge>,
    events: &Receiver<MainEvent>,
    prompt: &str,
) -> Result<(), String> {
    let session_id = bridge
        .session()
        .ok_or_else(|| "no ACP session is open".to_string())?;

    bridge.record("user", None, json!([{ "type": "text", "text": prompt }]));
    let _ = request_silent(
        "hook.notify",
        hook_payload("UserPromptSubmit", json!({ "prompt": prompt })),
    );

    INTERRUPTED.store(false, Ordering::SeqCst);
    bridge.turn_active.store(true, Ordering::SeqCst);

    let response = bridge.connection.request_async(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": prompt }],
        }),
    );
    let response = match response {
        Ok(response) => response,
        Err(err) => {
            bridge.turn_active.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    // The reader thread routes the prompt response here so this loop can also
    // service permission requests, which need the same stdin this thread owns.
    let forward = bridge.events.clone();
    thread::spawn(move || {
        let outcome = response
            .recv()
            .unwrap_or_else(|_| Err("the ACP agent exited mid-turn".to_string()));
        let _ = forward.send(MainEvent::PromptDone(outcome));
    });

    let result = loop {
        match events.recv() {
            Ok(MainEvent::PromptDone(outcome)) => break outcome,
            Ok(MainEvent::Interact(interaction)) => service_interaction(interaction),
            Err(_) => break Err("the ACP session ended".to_string()),
        }
    };

    bridge.turn_active.store(false, Ordering::SeqCst);
    bridge.flush_message();

    match &result {
        Ok(value) => {
            let stop_reason = value
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn");
            if stop_reason == "cancelled" {
                bridge.transcript.lifecycle("interrupted");
            }
            if stop_reason != "end_turn" {
                println!("\n{}", dim(&format!("[{}]", stop_reason.replace('_', " "))));
            }
            let _ = request_silent(
                "hook.notify",
                hook_payload("Stop", json!({ "stopReason": stop_reason })),
            );
        }
        Err(err) => {
            println!("\n{}", dim(&format!("error: {err}")));
            let _ = request_silent(
                "hook.notify",
                hook_payload("StopFailure", json!({ "error": err })),
            );
        }
    }

    // A failed turn is not a failed session. An agent is entitled to answer
    // `session/prompt` with an error — rate limited, context too long, refused
    // — and the pane should stay open for the next prompt. Only the agent
    // process actually being gone ends the session, since every later turn
    // would otherwise fail the same way forever.
    if result.is_err() && !bridge.agent_alive.load(Ordering::SeqCst) {
        return Err("the ACP agent exited".to_string());
    }
    Ok(())
}

/// Asks the user an agent's question on the pane's stdin. Only ever called from
/// the main thread.
fn service_interaction(interaction: Interaction) {
    println!("\n{}", bold(&interaction.heading));
    let result = match &interaction.kind {
        InteractionKind::Permission { options } => service_permission(options),
        InteractionKind::Form { fields } => service_form(fields),
        InteractionKind::Url { url } => service_url(url),
    };
    let _ = request_silent("hook.notify", hook_payload("PermissionResolved", json!({})));
    let _ = interaction.reply.send(result);
}

/// Reads one line, or `None` at EOF. EOF means the pane is closing, which every
/// caller treats as "no decision" rather than as an answer.
fn read_answer(prompt: &str) -> Option<String> {
    print_now(&bold(prompt));
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

fn service_permission(options: &[(String, String)]) -> Value {
    for (index, (_, label)) in options.iter().enumerate() {
        println!("  {}. {label}", index + 1);
    }
    loop {
        let Some(choice) = read_answer("choose › ") else {
            return json!({ "outcome": { "outcome": "cancelled" } });
        };
        if choice.is_empty() {
            continue;
        }
        match choice.parse::<usize>() {
            Ok(index) if index >= 1 && index <= options.len() => {
                return json!({
                    "outcome": { "outcome": "selected", "optionId": options[index - 1].0 }
                });
            }
            _ => println!("{}", dim(&format!("enter 1-{}", options.len()))),
        }
    }
}

/// Walks the form's fields. `/decline` refuses outright, `/cancel` (and EOF)
/// backs out without deciding — ACP treats those differently and agents are
/// required to branch on which they got.
fn service_form(fields: &[Field]) -> Value {
    let flagged = elicitation::secret_looking_fields(fields);
    if !flagged.is_empty() {
        // The spec forbids collecting secrets this way — they are supposed to
        // go through url mode, out of band. The agent is the one breaking the
        // rule, but qmux is the one that would hand over the value.
        println!(
            "{}",
            dim(&format!(
                "warning: this form asks for {} — ACP forbids collecting secrets in a form, and the value would pass through the agent. Decline unless you are sure.",
                flagged.join(", ")
            ))
        );
    }
    println!("{}", dim("/decline to refuse, /cancel to back out"));

    let mut answers: Vec<(Field, Option<Value>)> = Vec::new();
    for field in fields {
        if let Some(description) = &field.description {
            println!("{}", dim(&format!("  {description}")));
        }
        if let FieldKind::Choice(choices) = &field.kind {
            for (index, choice) in choices.iter().enumerate() {
                println!("  {}. {choice}", index + 1);
            }
        }
        let value = loop {
            let Some(input) = read_answer(&format!("{} › ", elicitation::prompt_label(field)))
            else {
                return json!({ "action": "cancel" });
            };
            match input.as_str() {
                "/decline" => return json!({ "action": "decline" }),
                "/cancel" => return json!({ "action": "cancel" }),
                // Blank takes the default, or skips an optional field.
                "" if field.default.is_some() || !field.required => break None,
                "" => {
                    println!("{}", dim("this one is required"));
                    continue;
                }
                _ => match elicitation::coerce(field, &input) {
                    Ok(value) => break Some(value),
                    Err(err) => println!("{}", dim(&err)),
                },
            }
        };
        answers.push((field.clone(), value));
    }

    match elicitation::build_content(answers) {
        Ok(content) => json!({ "action": "accept", "content": content }),
        // Unreachable given the loop above enforces required fields, but a
        // wrong answer here would be a silent protocol violation.
        Err(err) => {
            println!("{}", dim(&format!("could not submit: {err}")));
            json!({ "action": "cancel" })
        }
    }
}

/// Shows the full URL, warns about it, and opens it only on explicit consent.
///
/// ACP requires all three, and requires the page open somewhere neither qmux
/// nor the agent's model can read — which is what the browser overlay's
/// isolated tab is. Accepting means "the user agreed to open this", not that
/// the external flow finished; `elicitation/complete` reports that later.
fn service_url(url: &str) -> Value {
    println!("  {url}");
    for warning in elicitation::url_warnings(url) {
        println!("{}", dim(&format!("  warning: {warning}")));
    }

    loop {
        let Some(answer) = read_answer("open in the browser? [y/N, /decline] › ") else {
            return json!({ "action": "cancel" });
        };
        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => break,
            "" | "n" | "no" | "/cancel" => return json!({ "action": "cancel" }),
            "/decline" | "d" | "decline" => return json!({ "action": "decline" }),
            _ => println!("{}", dim("enter y or n")),
        }
    }

    if let Err(err) = request_silent(
        "browser.open",
        json!({
            "target": url,
            "cwd": env::current_dir().ok().map(|path| path.display().to_string()),
        }),
    ) {
        // Consent was given, so this is still an `accept`; the user just has to
        // open the link themselves. Failing the elicitation instead would strand
        // the agent waiting on a flow the user is perfectly able to complete.
        println!(
            "{}",
            dim(&format!(
                "could not open the qmux browser ({err}); open the link above manually"
            ))
        );
    }
    json!({ "action": "accept" })
}

fn hook_payload(event: &str, payload: Value) -> Value {
    json!({
        "event": event,
        "paneId": env::var("QMUX_PANE_ID").ok(),
        "agentId": env::var("QMUX_AGENT_ID").ok(),
        "adapterId": "acp",
        "payload": payload,
    })
}

// ---------------------------------------------------------------------------
// Interrupts
// ---------------------------------------------------------------------------

extern "C" fn handle_sigint(_signal: libc::c_int) {
    // Async-signal-safe: an atomic store and nothing else.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_interrupt_handler() {
    // Via a fn *pointer*: casting the fn item straight to an integer is a
    // different (and lint-flagged) conversion.
    let handler: extern "C" fn(libc::c_int) = handle_sigint;
    unsafe {
        libc::signal(libc::SIGINT, handler as usize as libc::sighandler_t);
    }
}

/// Turns a Ctrl-C into `session/cancel`. Polling keeps the signal handler
/// trivial, and 50ms is well inside human reaction time for "it stopped".
fn watch_for_interrupts(bridge: Arc<Bridge>) {
    loop {
        thread::sleep(Duration::from_millis(50));
        if !INTERRUPTED.swap(false, Ordering::SeqCst) {
            continue;
        }
        if !bridge.turn_active.load(Ordering::SeqCst) {
            continue;
        }
        let Some(session_id) = bridge.session() else {
            continue;
        };
        println!("\n{}", dim("cancelling…"));
        let _ = bridge
            .connection
            .notify("session/cancel", json!({ "sessionId": session_id }));
    }
}

// ---------------------------------------------------------------------------
// Inbound message handling
// ---------------------------------------------------------------------------

fn read_agent_messages(bridge: Arc<Bridge>, stdout: std::process::ChildStdout) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            // The transport says stdout carries nothing but ACP messages. A
            // stray line is the agent's bug; log it and keep the session alive.
            bridge
                .transcript
                .write(json!({ "type": "malformed", "line": line, "timestamp": now_ms() }));
            continue;
        };

        let has_method = message.get("method").is_some();
        match (has_method, message.get("id")) {
            // Request from the agent: needs a response.
            (true, Some(id)) => {
                let id = id.clone();
                let bridge = Arc::clone(&bridge);
                // Handlers can block for a long time — `terminal/wait_for_exit`
                // by design, permission prompts on the user — so none of them
                // may run on the reader thread.
                thread::spawn(move || handle_agent_request(&bridge, id, &message));
            }
            // Notification from the agent.
            (true, None) => handle_agent_notification(&bridge, &message),
            // Response to something we sent.
            (false, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let outcome = match message.get("error") {
                    Some(error) => Err(rpc_error_message(error)),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                bridge.connection.resolve(id, outcome);
            }
            (false, None) => {}
        }
    }
    bridge.agent_alive.store(false, Ordering::SeqCst);
    bridge.connection.fail_all("the ACP agent exited");
}

fn rpc_error_message(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the ACP agent reported an error");
    match error.get("data") {
        Some(data) if !data.is_null() => format!("{message} ({data})"),
        _ => message.to_string(),
    }
}

fn handle_agent_notification(bridge: &Arc<Bridge>, message: &Value) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => handle_session_update(bridge, &params),
        "elicitation/complete" => handle_elicitation_complete(bridge, &params),
        _ => {}
    }
}

/// Closes the loop on a url-mode elicitation. An `accept` only ever meant the
/// user agreed to open the link; this is the agent saying the flow finished.
fn handle_elicitation_complete(bridge: &Arc<Bridge>, params: &Value) {
    let Some(id) = params.get("elicitationId").and_then(Value::as_str) else {
        return;
    };
    // Unknown or already-completed ids are ignored, not reported: the id is
    // opaque and a duplicate says nothing the user needs.
    if !bridge
        .pending_elicitations
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(id)
    {
        return;
    }
    bridge.flush_message();
    println!("\n{}", dim("· the browser step finished"));
}

fn handle_session_update(bridge: &Arc<Bridge>, params: &Value) {
    let Some(update) = params.get("update") else {
        return;
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");

    match kind {
        "agent_message_chunk" => {
            let text = content_text(update.get("content"));
            if text.is_empty() {
                return;
            }
            print_now(&text);
            bridge.push_chunk(update.get("messageId").and_then(Value::as_str), &text);
        }
        "agent_thought_chunk" => {
            let text = content_text(update.get("content"));
            if !text.is_empty() {
                print_now(&dim(&text));
            }
        }
        "user_message_chunk" => {
            // Replayed by session/load. The live path already recorded the
            // user's turn when it was sent, so only a replay should write one.
            let text = content_text(update.get("content"));
            if !text.is_empty() {
                print_now(&dim(&format!("\n› {text}\n")));
            }
        }
        "tool_call" => {
            bridge.flush_message();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("tool call");
            let tool_kind = update
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("other");
            println!("\n{}", dim(&format!("· {title} [{tool_kind}]")));
            bridge.record(
                "assistant",
                update.get("toolCallId").and_then(Value::as_str),
                json!([{
                    "type": "toolUse",
                    "id": update.get("toolCallId").and_then(Value::as_str),
                    "name": title,
                    "input": update.get("rawInput").cloned().unwrap_or(Value::Null),
                }]),
            );
            let _ = request_silent("hook.notify", hook_payload("PreToolUse", update.clone()));
        }
        "tool_call_update" => {
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            if !matches!(status, "completed" | "failed") {
                return;
            }
            let content = update.get("content").cloned().unwrap_or(Value::Null);
            if status == "failed" {
                println!("{}", dim("  failed"));
            }
            bridge.record(
                "assistant",
                update.get("toolCallId").and_then(Value::as_str),
                // These lines are deserialized as `TurnBlock` by the adapter, in
                // a crate that cannot import the type, so the spelling has to
                // match its serde attributes exactly — camelCase fields under a
                // camelCase tag. The adapter's round-trip test is the contract.
                json!([{
                    "type": "toolResult",
                    "toolUseId": update.get("toolCallId").and_then(Value::as_str),
                    "content": content,
                    "isError": status == "failed",
                }]),
            );
            let _ = request_silent("hook.notify", hook_payload("PostToolUse", update.clone()));
        }
        "plan" => {
            bridge.flush_message();
            let entries = update
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if entries.is_empty() {
                return;
            }
            println!("\n{}", dim("plan:"));
            for entry in &entries {
                let content = entry.get("content").and_then(Value::as_str).unwrap_or("");
                let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
                println!("{}", dim(&format!("  [{status}] {content}")));
            }
            bridge.record(
                "assistant",
                None,
                json!([{ "type": "raw", "value": { "plan": entries } }]),
            );
        }
        "config_option_update" => {
            // Agents push these on their own — a model falling back under rate
            // limiting, a mode switching itself. Like the setup response, the
            // payload is the complete list.
            if let Some(options) = update.get("configOptions") {
                bridge.flush_message();
                report_config_options(options, true);
            }
        }
        _ => {}
    }
}

/// Flattens an ACP content block to display text. Non-text blocks (images,
/// audio, embedded resources) get a short placeholder rather than being dropped
/// silently, so the pane shows that something was there.
fn content_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| content_text(Some(item)))
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(_) => match content.get("type").and_then(Value::as_str) {
            Some("text") => content
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Some("resource") => content
                .get("resource")
                .and_then(|resource| resource.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Some("image") => "[image]".to_string(),
            Some("audio") => "[audio]".to_string(),
            Some("resource_link") => content
                .get("uri")
                .and_then(Value::as_str)
                .map(|uri| format!("[{uri}]"))
                .unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn handle_agent_request(bridge: &Arc<Bridge>, id: Value, message: &Value) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    match dispatch_agent_request(bridge, method, &params) {
        Ok(result) => bridge.connection.respond_ok(id, result),
        // -32601 is JSON-RPC's "method not found"; anything else we failed at is
        // an internal error from the agent's point of view.
        Err(RequestError::Unsupported(method)) => {
            bridge
                .connection
                .respond_err(id, -32601, format!("qmux does not implement {method}"));
        }
        Err(RequestError::Failed(message)) => {
            bridge.connection.respond_err(id, -32603, message);
        }
    }
}

enum RequestError {
    Unsupported(String),
    Failed(String),
}

fn dispatch_agent_request(
    bridge: &Arc<Bridge>,
    method: &str,
    params: &Value,
) -> Result<Value, RequestError> {
    let failed = |message: String| RequestError::Failed(message);
    match method {
        "session/request_permission" => request_permission(bridge, params).map_err(failed),
        "elicitation/create" => elicit(bridge, params).map_err(failed),
        "fs/read_text_file" => read_text_file(bridge, params).map_err(failed),
        "fs/write_text_file" => write_text_file(bridge, params).map_err(failed),
        "terminal/create" => create_terminal(bridge, params).map_err(failed),
        "terminal/output" => terminal_output(bridge, params).map_err(failed),
        "terminal/wait_for_exit" => terminal_wait(bridge, params).map_err(failed),
        "terminal/kill" => terminal_kill(bridge, params).map_err(failed),
        "terminal/release" => terminal_release(bridge, params).map_err(failed),
        other => Err(RequestError::Unsupported(other.to_string())),
    }
}

fn request_permission(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let options: Vec<(String, String)> = params
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    let id = option.get("optionId").and_then(Value::as_str)?;
                    let name = option
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string();
                    Some((id.to_string(), name))
                })
                .collect()
        })
        .unwrap_or_default();

    if options.is_empty() {
        return Err("permission request carried no options".to_string());
    }

    let heading = params
        .get("toolCall")
        .and_then(|call| call.get("title"))
        .and_then(Value::as_str)
        .map(|title| format!("Permission needed: {title}"))
        .unwrap_or_else(|| "Permission needed".to_string());

    let _ = request_silent(
        "hook.notify",
        hook_payload("PermissionRequest", params.clone()),
    );

    ask(
        bridge,
        heading,
        InteractionKind::Permission { options },
        json!({ "outcome": { "outcome": "cancelled" } }),
    )
}

/// Raises a question to the main thread and blocks for the answer.
///
/// No timeout: these are questions for a human, and ACP's own escape hatch is
/// `session/cancel`, which ends the turn and resolves them. `abandoned` is the
/// answer to give if the session ends first — never a silent success.
fn ask(
    bridge: &Arc<Bridge>,
    heading: String,
    kind: InteractionKind,
    abandoned: Value,
) -> Result<Value, String> {
    let (reply, answer) = channel();
    bridge
        .events
        .send(MainEvent::Interact(Interaction {
            heading,
            kind,
            reply,
        }))
        .map_err(|_| "the session ended before the question could be answered".to_string())?;
    Ok(answer.recv().unwrap_or(abandoned))
}

/// `elicitation/create` — the agent asking the user for structured input.
fn elicit(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("The agent needs some information");
    // Absent `mode` is treated as `form`, which is the only mode a pre-modes
    // agent could have meant.
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("form");

    // The scope fields (`sessionId`, `toolCallId`, `requestId`) are ignored on
    // purpose: a bridge hosts exactly one session, so there is nothing to route
    // between and the question reaches the one pane either way.
    let mut elicitation_id = None;
    let kind = match mode {
        "form" => {
            let schema = params
                .get("requestedSchema")
                .ok_or_else(|| "form elicitation carried no requestedSchema".to_string())?;
            InteractionKind::Form {
                fields: elicitation::parse_schema(schema)?,
            }
        }
        "url" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "url elicitation carried no url".to_string())?;
            // Track it so a later `elicitation/complete` can be matched, and
            // an unknown or replayed id ignored.
            if let Some(id) = params.get("elicitationId").and_then(Value::as_str) {
                elicitation_id = Some(id.to_string());
                bridge
                    .pending_elicitations
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .insert(id.to_string());
            }
            InteractionKind::Url {
                url: url.to_string(),
            }
        }
        other => return Err(format!("unsupported elicitation mode '{other}'")),
    };

    // Reuses the permission hooks rather than adding parallel ones. An
    // elicitation blocks on the user exactly the way a permission does, and
    // `AwaitingPermission` is the status that makes the composer *queue* a
    // typed turn rather than send it — which matters here, because a sent turn
    // would be swallowed as a form answer.
    let _ = request_silent(
        "hook.notify",
        hook_payload("PermissionRequest", params.clone()),
    );
    let outcome = ask(
        bridge,
        message.to_string(),
        kind,
        json!({ "action": "cancel" }),
    )?;

    // Only an accepted URL flow can still be completed. Dropping the id on any
    // other outcome stops a later stray completion from announcing a step the
    // user declined to take.
    if let Some(id) = elicitation_id
        && outcome.get("action").and_then(Value::as_str) != Some("accept")
    {
        bridge
            .pending_elicitations
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&id);
    }
    Ok(outcome)
}

/// Resolves a path from the agent, which ACP requires to be absolute. Relative
/// paths are still resolved against the session cwd rather than rejected —
/// agents get this wrong, and guessing right is harmless here.
fn session_path(bridge: &Arc<Bridge>, params: &Value) -> Result<PathBuf, String> {
    let raw = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "request had no path".to_string())?;
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        bridge.cwd.join(path)
    })
}

/// Applies ACP's optional `line`/`limit` window to a file's contents. `line` is
/// 1-based; absent means the whole file, which is returned untouched.
fn window_text(contents: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none_or(|line| line <= 1) && limit.is_none() {
        return contents.to_string();
    }
    let start = line.map(|line| line.max(1) as usize - 1).unwrap_or(0);
    let all: Vec<&str> = contents.lines().collect();
    let end = match limit {
        Some(limit) => start.saturating_add(limit as usize).min(all.len()),
        None => all.len(),
    };
    let mut content = all.get(start..end).unwrap_or_default().join("\n");
    // Every selected line is newline-terminated in the source except possibly
    // the file's last one, so the window keeps a trailing newline unless it
    // runs to the end of a file that has none. Getting this wrong silently
    // joins two lines when the agent writes the window back.
    if !content.is_empty() && (end < all.len() || contents.ends_with('\n')) {
        content.push('\n');
    }
    content
}

fn read_text_file(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let path = session_path(bridge, params)?;
    let contents = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    let line = params.get("line").and_then(Value::as_u64);
    let limit = params.get("limit").and_then(Value::as_u64);
    Ok(json!({ "content": window_text(&contents, line, limit) }))
}

fn write_text_file(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let path = session_path(bridge, params)?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "write request had no content".to_string())?;
    // ACP requires the client to create the file if it is missing, which
    // includes its directory when the agent is writing something new.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    std::fs::write(&path, content)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(Value::Null)
}

fn create_terminal(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let command = params
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "terminal/create had no command".to_string())?;
    let args: Vec<String> = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // ACP sends env as an array of {name, value} rather than an object.
    let env: Vec<(String, String)> = params
        .get("env")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Value::as_str)?;
                    let value = entry.get("value").and_then(Value::as_str).unwrap_or("");
                    Some((name.to_string(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| bridge.cwd.clone());
    let limit = params
        .get("outputByteLimit")
        .and_then(Value::as_u64)
        .map(|limit| limit as usize);

    println!(
        "\n{}",
        dim(format!("· $ {command} {}", args.join(" ")).trim_end())
    );
    let terminal_id = bridge
        .terminals
        .create(command, &args, &env, Some(&cwd), limit)?;
    Ok(json!({ "terminalId": terminal_id }))
}

fn terminal_id(params: &Value) -> Result<String, String> {
    params
        .get("terminalId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "request had no terminalId".to_string())
}

/// ACP reports a finished command as `exitStatus`, absent while it runs.
fn exit_status_json(exit: Option<terminal::ExitInfo>) -> Value {
    match exit {
        Some(exit) => json!({ "exitCode": exit.exit_code, "signal": Value::Null }),
        None => Value::Null,
    }
}

fn terminal_output(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let (output, truncated, exit) = bridge.terminals.output(&terminal_id(params)?)?;
    Ok(json!({
        "output": output,
        "truncated": truncated,
        "exitStatus": exit_status_json(exit),
    }))
}

fn terminal_wait(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    let exit = bridge.terminals.wait_for_exit(&terminal_id(params)?)?;
    Ok(json!({ "exitCode": exit.exit_code, "signal": Value::Null }))
}

fn terminal_kill(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    bridge.terminals.kill(&terminal_id(params)?)?;
    Ok(Value::Null)
}

fn terminal_release(bridge: &Arc<Bridge>, params: &Value) -> Result<Value, String> {
    bridge.terminals.release(&terminal_id(params)?)?;
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_text_flattens_the_block_types_a_chunk_can_carry() {
        assert_eq!(
            content_text(Some(&json!({ "type": "text", "text": "hi" }))),
            "hi"
        );
        assert_eq!(
            content_text(Some(&json!({
                "type": "resource",
                "resource": { "uri": "file:///a.rs", "text": "fn main() {}" }
            }))),
            "fn main() {}"
        );
        assert_eq!(
            content_text(Some(&json!({ "type": "image", "data": "…" }))),
            "[image]"
        );
        assert_eq!(content_text(None), "");
        // Unknown block types are skipped rather than rendered as JSON noise.
        assert_eq!(content_text(Some(&json!({ "type": "future" }))), "");
    }

    #[test]
    fn rpc_errors_include_data_when_the_agent_sends_it() {
        assert_eq!(
            rpc_error_message(&json!({ "code": -32603, "message": "boom" })),
            "boom"
        );
        assert_eq!(
            rpc_error_message(&json!({ "code": -32603, "message": "boom", "data": "ctx" })),
            "boom (\"ctx\")"
        );
    }

    #[test]
    fn exit_status_is_null_until_the_command_finishes() {
        assert_eq!(exit_status_json(None), Value::Null);
        assert_eq!(
            exit_status_json(Some(terminal::ExitInfo { exit_code: Some(0) })),
            json!({ "exitCode": 0, "signal": Value::Null })
        );
    }

    #[test]
    fn an_unwindowed_read_returns_the_file_verbatim() {
        for contents in ["a\nb\nc\n", "a\nb\nc", "", "no trailing newline"] {
            assert_eq!(window_text(contents, None, None), contents);
            // line: 1 with no limit is the whole file too.
            assert_eq!(window_text(contents, Some(1), None), contents);
        }
    }

    #[test]
    fn a_windowed_read_keeps_the_newline_that_terminates_each_line() {
        let contents = "one\ntwo\nthree\n";
        assert_eq!(window_text(contents, Some(2), None), "two\nthree\n");
        assert_eq!(window_text(contents, Some(1), Some(2)), "one\ntwo\n");
        assert_eq!(window_text(contents, Some(2), Some(1)), "two\n");
    }

    #[test]
    fn a_window_reaching_a_file_without_a_final_newline_does_not_invent_one() {
        let contents = "one\ntwo\nthree";
        assert_eq!(window_text(contents, Some(3), None), "three");
        // A window stopping short of the end still terminates its last line,
        // or writing it back would splice "two" onto "three".
        assert_eq!(window_text(contents, Some(2), Some(1)), "two\n");
    }

    #[test]
    fn windows_past_the_end_are_empty_rather_than_a_panic() {
        let contents = "one\ntwo\n";
        assert_eq!(window_text(contents, Some(99), None), "");
        assert_eq!(window_text(contents, Some(2), Some(99)), "two\n");
        assert_eq!(window_text(contents, Some(1), Some(0)), "");
        // `line: 0` is out of spec (it is 1-based); clamp rather than underflow.
        assert_eq!(window_text(contents, Some(0), Some(1)), "one\n");
    }

    #[test]
    fn chunks_accumulate_into_one_message_until_the_message_id_changes() {
        let mut buffer = MessageBuffer::default();
        assert_eq!(buffer.push(Some("m1"), "Hel"), None);
        assert_eq!(buffer.push(Some("m1"), "lo"), None);

        // A new id ends the previous message and returns it.
        assert_eq!(
            buffer.push(Some("m2"), "Bye"),
            Some(("Hello".to_string(), Some("m1".to_string())))
        );
        assert_eq!(
            buffer.take(),
            Some(("Bye".to_string(), Some("m2".to_string())))
        );
        assert_eq!(buffer.take(), None);
    }

    #[test]
    fn chunks_without_a_message_id_stay_one_message_until_the_turn_ends() {
        let mut buffer = MessageBuffer::default();
        assert_eq!(buffer.push(None, "a"), None);
        assert_eq!(buffer.push(None, "b"), None);
        assert_eq!(buffer.take(), Some(("ab".to_string(), None)));
    }

    #[test]
    fn a_message_of_only_whitespace_is_not_recorded() {
        let mut buffer = MessageBuffer::default();
        buffer.push(Some("m1"), "  \n ");
        assert_eq!(buffer.take(), None);
        // …and the buffer is genuinely reset, not merely reported empty.
        buffer.push(Some("m2"), "real");
        assert_eq!(
            buffer.take(),
            Some(("real".to_string(), Some("m2".to_string())))
        );
    }

    #[test]
    fn json_env_helpers_reject_malformed_input_but_allow_absence() {
        unsafe {
            env::remove_var("QMUX_TEST_ACP_ARGS");
        }
        assert_eq!(
            json_env_array("QMUX_TEST_ACP_ARGS").unwrap(),
            Vec::<String>::new()
        );

        unsafe {
            env::set_var("QMUX_TEST_ACP_ARGS", "[\"--acp\"]");
        }
        assert_eq!(json_env_array("QMUX_TEST_ACP_ARGS").unwrap(), vec!["--acp"]);

        unsafe {
            env::set_var("QMUX_TEST_ACP_ARGS", "not json");
        }
        assert!(json_env_array("QMUX_TEST_ACP_ARGS").is_err());
        unsafe {
            env::remove_var("QMUX_TEST_ACP_ARGS");
        }
    }
}

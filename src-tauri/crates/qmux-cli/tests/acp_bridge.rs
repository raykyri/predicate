//! End-to-end tests for the `qmux acp` bridge, driven by a scripted fake ACP
//! agent.
//!
//! The bridge's job is to survive whatever an agent does, so the interesting
//! cases are the misbehaving ones: junk on stdout, chatty stderr, dying
//! mid-turn, protocol extensions we've never seen. None of that is reachable
//! through a real vendor CLI on demand, and a real CLI would need a key and a
//! network besides. So this file is both the test suite *and* the agent it
//! tests against: run with `QMUX_ACP_FIXTURE=<scenario>` in the environment,
//! the test binary re-execs as that agent instead of running tests. That is
//! why the target sets `harness = false` — the stock harness would treat the
//! mode switch as a test filter.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Generous enough that a loaded machine won't flake, short enough that a real
/// hang (the failure mode these tests exist to catch) fails the run rather than
/// stalling CI forever.
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);

// ===========================================================================
// Entry point
// ===========================================================================

fn main() {
    if let Ok(scenario) = env::var("QMUX_ACP_FIXTURE") {
        fake_agent(&scenario);
        return;
    }

    let tests: Vec<(&str, fn())> = vec![
        (
            "chunks_sharing_a_message_id_become_one_turn",
            chunks_sharing_a_message_id_become_one_turn,
        ),
        (
            "a_tool_call_and_its_updates_collapse_to_two_blocks",
            a_tool_call_and_its_updates_collapse_to_two_blocks,
        ),
        (
            "a_permission_request_mid_turn_is_answered_from_the_pane",
            a_permission_request_mid_turn_is_answered_from_the_pane,
        ),
        (
            "a_cancelled_turn_is_recorded_as_an_interruption",
            a_cancelled_turn_is_recorded_as_an_interruption,
        ),
        (
            "a_misbehaving_agent_does_not_break_the_session",
            a_misbehaving_agent_does_not_break_the_session,
        ),
        (
            "an_agent_that_dies_mid_turn_fails_the_turn_promptly",
            an_agent_that_dies_mid_turn_fails_the_turn_promptly,
        ),
        (
            "a_huge_single_line_response_survives_the_reader",
            a_huge_single_line_response_survives_the_reader,
        ),
        (
            "terminals_run_on_a_pty_and_honor_their_lifecycle",
            terminals_run_on_a_pty_and_honor_their_lifecycle,
        ),
        (
            "outgoing_frames_are_well_formed_json_rpc",
            outgoing_frames_are_well_formed_json_rpc,
        ),
        (
            "a_refused_resume_falls_back_to_a_new_session",
            a_refused_resume_falls_back_to_a_new_session,
        ),
        (
            "session_config_options_are_reported_and_relabelled",
            session_config_options_are_reported_and_relabelled,
        ),
        (
            "initialize_advertises_boolean_config_support",
            initialize_advertises_boolean_config_support,
        ),
        (
            "a_form_elicitation_collects_typed_answers",
            a_form_elicitation_collects_typed_answers,
        ),
        (
            "a_declined_form_is_distinct_from_a_cancelled_one",
            a_declined_form_is_distinct_from_a_cancelled_one,
        ),
        (
            "a_url_elicitation_needs_consent_and_reports_completion",
            a_url_elicitation_needs_consent_and_reports_completion,
        ),
        (
            "a_refused_url_elicitation_opens_nothing",
            a_refused_url_elicitation_opens_nothing,
        ),
        (
            "initialize_advertises_both_elicitation_modes",
            initialize_advertises_both_elicitation_modes,
        ),
        (
            "a_streaming_bridge_writes_no_local_transcript",
            a_streaming_bridge_writes_no_local_transcript,
        ),
        (
            "a_question_raised_before_the_session_opens_reaches_the_pane",
            a_question_raised_before_the_session_opens_reaches_the_pane,
        ),
        (
            "the_filesystem_is_confined_to_the_session_directory",
            the_filesystem_is_confined_to_the_session_directory,
        ),
        (
            "authenticate_is_prompted_when_auth_methods_are_advertised",
            authenticate_is_prompted_when_auth_methods_are_advertised,
        ),
        (
            "a_saved_auth_method_is_tried_silently_before_prompting",
            a_saved_auth_method_is_tried_silently_before_prompting,
        ),
    ];

    let mut failures = Vec::new();
    println!("\nrunning {} acp bridge tests\n", tests.len());
    for (name, test) in &tests {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(test)) {
            Ok(()) => println!("test {name} ... ok"),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panicked".to_string());
                println!("test {name} ... FAILED");
                failures.push((name.to_string(), message));
            }
        }
    }

    if failures.is_empty() {
        println!("\ntest result: ok. {} passed\n", tests.len());
        return;
    }
    println!("\nfailures:");
    for (name, message) in &failures {
        println!("\n---- {name} ----\n{message}");
    }
    println!(
        "\ntest result: FAILED. {} passed; {} failed\n",
        tests.len() - failures.len(),
        failures.len()
    );
    std::process::exit(1);
}

// ===========================================================================
// Harness
// ===========================================================================

struct Session {
    /// Everything the bridge painted into the pane.
    pane: String,
    /// Parsed transcript lines, in order.
    transcript: Vec<Value>,
    /// Every frame the bridge sent the agent, as the agent received it.
    frames: Vec<String>,
    /// Whatever the fake agent chose to record about the client's answers.
    notes: Vec<Value>,
    /// The agent's stderr, which must never reach the pane.
    agent_log: String,
}

impl Session {
    fn turns(&self) -> Vec<&Value> {
        self.transcript
            .iter()
            .filter(|line| line["type"] == "turn")
            .collect()
    }

    fn note(&self, key: &str) -> &Value {
        self.notes
            .iter()
            .find(|note| note["note"] == key)
            .unwrap_or_else(|| panic!("the fake agent recorded no '{key}' note: {:#?}", self.notes))
    }
}

/// Runs the bridge against `scenario`, feeding `input` on stdin.
fn run_bridge(scenario: &str, input: &str) -> Session {
    run_bridge_inner(scenario, input, false)
}

/// As `run_bridge`, but with the bridge in streaming mode.
fn run_bridge_streaming(scenario: &str, input: &str) -> Session {
    run_bridge_inner(scenario, input, true)
}

fn run_bridge_inner(scenario: &str, input: &str, stream: bool) -> Session {
    let dir = scratch_dir(scenario);
    let transcript = dir.join("session.jsonl");
    let frames = dir.join("frames.ndjson");
    let notes = dir.join("notes.ndjson");

    let mut command = Command::new(env!("CARGO_BIN_EXE_qmux-cli"));
    command
        .arg("acp")
        .current_dir(&dir)
        .env(
            "QMUX_ACP_COMMAND",
            env::current_exe().expect("test binary path"),
        )
        .env("QMUX_ACP_ARGS", "[]")
        .env("QMUX_ACP_CWD", &dir)
        .env("QMUX_ACP_NAME", format!("fixture:{scenario}"))
        .env("QMUX_ACP_TRANSCRIPT", &transcript)
        .env("QMUX_ACP_FIXTURE", scenario)
        .env("QMUX_ACP_FIXTURE_FRAMES", &frames)
        .env("QMUX_ACP_FIXTURE_NOTES", &notes)
        // The bridge posts lifecycle hooks to the qmux control socket. With no
        // socket in the environment those calls fail fast and are ignored,
        // which is what we want: this suite is about the protocol, not qmux.
        .env_remove("QMUX_SOCK")
        .env_remove("QMUX_TOKEN")
        // Keep the pane output free of ANSI so assertions can match plain text.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if scenario == "resume" {
        command.env("QMUX_ACP_LOAD_SESSION", "sess_previous");
    }
    if scenario == "auth_silent" {
        // Prefer a previously successful method so the bridge skips the prompt.
        command.env("QMUX_ACP_AUTH_METHOD", "oauth-personal");
    }
    if stream {
        // What a remote bridge runs as: no access to the filesystem the
        // sidebar tails, so records go to qmux instead of to a file.
        command.env("QMUX_ACP_TRANSCRIPT_STREAM", "1");
        command.env("QMUX_ACP_LOG", dir.join("agent.log"));
    }

    let mut child = command.spawn().expect("the bridge binary runs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("the bridge accepts input");

    let pane = wait_with_timeout(&mut child, scenario);

    Session {
        pane,
        transcript: read_ndjson(&transcript),
        frames: fs::read_to_string(&frames)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect(),
        notes: read_ndjson(&notes),
        agent_log: fs::read_to_string(if stream {
            dir.join("agent.log")
        } else {
            transcript.with_extension("agent.log")
        })
        .unwrap_or_default(),
    }
}

/// Waits for the bridge, killing it if it hangs. A hang is a genuine failure
/// mode here, so it must surface as one rather than as a stuck test run.
fn wait_with_timeout(child: &mut Child, scenario: &str) -> String {
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        let _ = tx.send(buffer);
    });

    match rx.recv_timeout(BRIDGE_TIMEOUT) {
        Ok(pane) => {
            let _ = child.wait();
            pane
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the bridge hung for {BRIDGE_TIMEOUT:?} in scenario '{scenario}'");
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = child.wait();
            String::new()
        }
    }
}

fn read_ndjson(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!("{} has a malformed line: {err}\n{line}", path.display())
            })
        })
        .collect()
}

fn scratch_dir(scenario: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = env::temp_dir().join(format!("qmux-acp-{scenario}-{unique}"));
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn text_of(turn: &Value) -> String {
    turn["blocks"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

// ===========================================================================
// Tests
// ===========================================================================

fn chunks_sharing_a_message_id_become_one_turn() {
    let session = run_bridge("chunks", "hello\n");
    let assistant: Vec<&Value> = session
        .turns()
        .into_iter()
        .filter(|turn| turn["role"] == "assistant")
        .collect();

    // Five chunks arrived across two message ids. The sidebar should show two
    // turns, not five.
    assert_eq!(
        assistant.len(),
        2,
        "expected one turn per message id, got {assistant:#?}"
    );
    assert_eq!(text_of(assistant[0]), "alpha beta gamma");
    assert_eq!(assistant[0]["nativeId"], "m1");
    assert_eq!(text_of(assistant[1]), "delta epsilon");
    assert_eq!(assistant[1]["nativeId"], "m2");

    // The pane still streams every chunk as it lands.
    assert!(
        session.pane.contains("alpha beta gamma"),
        "pane missing streamed text: {}",
        session.pane
    );
}

fn a_tool_call_and_its_updates_collapse_to_two_blocks() {
    let session = run_bridge("tools", "go\n");
    let blocks: Vec<&Value> = session
        .turns()
        .into_iter()
        .flat_map(|turn| turn["blocks"].as_array().cloned().unwrap_or_default())
        .map(|block| Box::leak(Box::new(block)) as &Value)
        .collect();

    let tool_use = blocks
        .iter()
        .find(|block| block["type"] == "toolUse")
        .expect("a toolUse block");
    assert_eq!(tool_use["id"], "c1");
    assert_eq!(tool_use["name"], "Analyze the code");

    // `in_progress` is a status change, not content: it must not produce a
    // block of its own, or every tool call would render twice.
    let results: Vec<&&Value> = blocks
        .iter()
        .filter(|block| block["type"] == "toolResult")
        .collect();
    assert_eq!(results.len(), 1, "expected exactly one result block");
    // camelCase: the transcript has to deserialize as `TurnBlock`, and the
    // frontend reads these same names off the wire.
    assert_eq!(results[0]["toolUseId"], "c1");
    assert_eq!(results[0]["isError"], false);
}

fn a_permission_request_mid_turn_is_answered_from_the_pane() {
    // The permission arrives while the main thread is blocked on the prompt
    // response — the path most likely to deadlock. "2" selects the second
    // option, so a default-to-first bug would show up as the wrong id.
    let session = run_bridge("permission", "go\n2\n");

    assert_eq!(
        session.note("permission")["outcome"],
        json!({ "outcome": "selected", "optionId": "reject" }),
    );
    assert!(
        session.pane.contains("Permission needed: Delete the file"),
        "pane should show the request: {}",
        session.pane
    );
    assert!(session.pane.contains("1. Allow once") && session.pane.contains("2. Reject"));
}

fn a_cancelled_turn_is_recorded_as_an_interruption() {
    let session = run_bridge("cancelled", "go\n");

    assert!(
        session
            .transcript
            .iter()
            .any(|line| line["type"] == "lifecycle" && line["event"] == "interrupted"),
        "a cancelled turn should leave an interruption marker: {:#?}",
        session.transcript
    );
    assert!(
        session.pane.contains("[cancelled]"),
        "pane should say the turn was cancelled: {}",
        session.pane
    );
}

fn a_misbehaving_agent_does_not_break_the_session() {
    let session = run_bridge("junk", "go\n");

    // Junk on stdout, an unknown `sessionUpdate`, and a response to an id we
    // never sent are all forward-compatibility hazards. None may be fatal.
    let assistant: Vec<&Value> = session
        .turns()
        .into_iter()
        .filter(|turn| turn["role"] == "assistant")
        .collect();
    assert_eq!(
        assistant.len(),
        1,
        "the good message should still land: {assistant:#?}"
    );
    assert_eq!(text_of(assistant[0]), "still here");

    // Chatty stderr is explicitly allowed by the spec; it belongs in the log,
    // never in the pane, where it would corrupt the rendered session.
    assert!(
        session.agent_log.contains("noisy diagnostic"),
        "agent stderr should reach the log file, got {:?}",
        session.agent_log
    );
    assert!(
        !session.pane.contains("noisy diagnostic"),
        "agent stderr must not reach the pane: {}",
        session.pane
    );

    // The unparseable line is kept for debugging rather than silently dropped.
    assert!(
        session
            .transcript
            .iter()
            .any(|line| line["type"] == "malformed"),
        "a stray stdout line should be recorded: {:#?}",
        session.transcript
    );
}

fn an_agent_that_dies_mid_turn_fails_the_turn_promptly() {
    // No response to `session/prompt` will ever arrive. The pending request has
    // to resolve when stdout closes, or the pane hangs forever.
    let session = run_bridge("die", "go\n");
    assert!(
        session.pane.contains("error:"),
        "the turn should report an error: {}",
        session.pane
    );
}

fn a_huge_single_line_response_survives_the_reader() {
    // ACP forbids embedded newlines, so a large tool result arrives as one
    // enormous line. The reader must not have an implicit line-length cap.
    let session = run_bridge("bigline", "go\n");
    let result = session
        .turns()
        .into_iter()
        .flat_map(|turn| turn["blocks"].as_array().cloned().unwrap_or_default())
        .find(|block| block["type"] == "toolResult")
        .expect("a tool result block");

    let payload = result["content"][0]["content"]["text"]
        .as_str()
        .expect("result text");
    assert_eq!(payload.len(), 300_000, "the whole line should survive");
    assert!(payload.starts_with("HEAD") && payload.ends_with("TAIL"));
}

fn terminals_run_on_a_pty_and_honor_their_lifecycle() {
    let session = run_bridge("terminal", "go\n");

    // A real pty, not a pipe: the command's own `test -t 1` is the witness.
    let tty = session.note("tty");
    assert!(
        tty["output"]
            .as_str()
            .unwrap_or_default()
            .contains("ISATTY"),
        "the command should see a tty on stdout: {tty:#?}"
    );
    assert_eq!(tty["exitStatus"]["exitCode"], 0);

    // `outputByteLimit` keeps the newest bytes, which is where the useful part
    // of a long build log lives.
    let capped = session.note("capped");
    let output = capped["output"].as_str().unwrap_or_default();
    assert_eq!(capped["truncated"], true, "should report truncation");
    assert!(
        output.contains("LINE200") && !output.contains("LINE1\r"),
        "truncation should keep the tail, got {output:?}"
    );

    // A killed terminal stays readable; a released one is gone.
    assert_eq!(session.note("after_kill")["ok"], true);
    assert_eq!(session.note("after_release")["ok"], false);
}

fn outgoing_frames_are_well_formed_json_rpc() {
    let session = run_bridge("chunks", "hello\n");
    assert!(!session.frames.is_empty(), "the agent received no frames");

    for frame in &session.frames {
        assert!(
            !frame.contains('\n'),
            "frames must be single lines: {frame}"
        );
        let parsed: Value = serde_json::from_str(frame)
            .unwrap_or_else(|err| panic!("frame is not JSON ({err}): {frame}"));
        assert_eq!(parsed["jsonrpc"], "2.0", "every frame carries the version");
        assert!(
            parsed.get("method").is_some() || parsed.get("id").is_some(),
            "a frame is a request, notification, or response: {frame}"
        );
    }

    let initialize: Value = serde_json::from_str(&session.frames[0]).expect("first frame parses");
    assert_eq!(initialize["method"], "initialize");
    let capabilities = &initialize["params"]["clientCapabilities"];
    // The bridge implements all three, and an agent that believes otherwise
    // will simply never exercise them.
    assert_eq!(capabilities["terminal"], true);
    assert_eq!(capabilities["fs"]["readTextFile"], true);
    assert_eq!(capabilities["fs"]["writeTextFile"], true);
    assert_eq!(initialize["params"]["protocolVersion"], 1);
}

fn a_refused_resume_falls_back_to_a_new_session() {
    // `loadSession` is optional, and a stale id is normal after a restart.
    // Refusing to open the pane would be the wrong response to either.
    let session = run_bridge("resume", "go\n");
    assert!(
        session.pane.contains("could not resume"),
        "the fallback should be visible: {}",
        session.pane
    );
    assert!(
        session.turns().iter().any(|turn| turn["sessionId"] == "s1"),
        "the turn should belong to the fresh session: {:#?}",
        session.transcript
    );
}

fn authenticate_is_prompted_when_auth_methods_are_advertised() {
    // "1" picks the first method; a bare prompt line is then the first turn.
    let session = run_bridge("auth", "1\nhello\n");
    assert!(
        session.pane.contains("Sign in required"),
        "the pane should show the auth prompt: {}",
        session.pane
    );
    assert!(
        session.pane.contains("1. Log in with Google"),
        "methods should be numbered: {}",
        session.pane
    );
    assert_eq!(
        session.note("authenticate")["methodId"],
        "oauth-personal",
        "the chosen method should be sent as authenticate: {:#?}",
        session.notes
    );
    assert!(
        session
            .turns()
            .iter()
            .any(|turn| turn["role"] == "user" && text_of(turn).contains("hello")),
        "auth should complete before the first turn: {:#?}",
        session.transcript
    );
}

fn a_saved_auth_method_is_tried_silently_before_prompting() {
    // QMUX_ACP_AUTH_METHOD=oauth-personal is set for this scenario.
    let session = run_bridge("auth_silent", "hello\n");
    assert!(
        !session.pane.contains("Sign in required"),
        "a saved method should not re-prompt: {}",
        session.pane
    );
    assert!(
        session.pane.contains("signing in with saved method"),
        "the silent try should be visible: {}",
        session.pane
    );
    assert_eq!(
        session.note("authenticate")["methodId"],
        "oauth-personal",
        "the saved method should be used: {:#?}",
        session.notes
    );
    assert!(
        session
            .turns()
            .iter()
            .any(|turn| turn["role"] == "user" && text_of(turn).contains("hello")),
        "the session should open after silent auth: {:#?}",
        session.transcript
    );
}

// ===========================================================================
// The fake agent
// ===========================================================================

struct Agent {
    frames: Option<PathBuf>,
    notes: Option<PathBuf>,
    pending: Arc<Mutex<HashMap<i64, Value>>>,
    next_id: Arc<Mutex<i64>>,
    stdout: Arc<Mutex<std::io::Stdout>>,
}

impl Agent {
    fn send(&self, message: Value) {
        let mut stdout = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        let _ = writeln!(stdout, "{message}");
        let _ = stdout.flush();
    }

    /// Writes a raw line, valid ACP or not — used to prove the bridge tolerates
    /// agents that break the transport contract.
    fn send_raw(&self, line: &str) {
        let mut stdout = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }

    fn note(&self, note: &str, mut value: Value) {
        let Some(path) = &self.notes else { return };
        value["note"] = json!(note);
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{value}");
        }
    }

    fn update(&self, update: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": "s1", "update": update },
        }));
    }

    fn chunk(&self, message_id: &str, text: &str) {
        self.update(json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": message_id,
            "content": { "type": "text", "text": text },
        }));
    }

    /// Calls the client and blocks until it answers.
    fn call(&self, method: &str, params: Value) -> Value {
        let id = {
            let mut next = self.next_id.lock().unwrap_or_else(|err| err.into_inner());
            *next += 1;
            *next
        };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        for _ in 0..2000 {
            if let Some(result) = self
                .pending
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .remove(&id)
            {
                return result;
            }
            thread::sleep(Duration::from_millis(5));
        }
        json!({ "error": "the client never answered" })
    }
}

fn fake_agent(scenario: &str) {
    let agent = Arc::new(Agent {
        frames: env::var("QMUX_ACP_FIXTURE_FRAMES").ok().map(PathBuf::from),
        notes: env::var("QMUX_ACP_FIXTURE_NOTES").ok().map(PathBuf::from),
        pending: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(Mutex::new(1000)),
        stdout: Arc::new(Mutex::new(std::io::stdout())),
    });

    let stdin = std::io::stdin();
    for line in BufReader::new(stdin.lock()).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(path) = &agent.frames
            && let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = writeln!(file, "{line}");
        }

        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        // A response to something we asked the client.
        if message.get("method").is_none() {
            if let Some(id) = message["id"].as_i64() {
                let result = message
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| message.get("error").cloned().unwrap_or(Value::Null));
                agent
                    .pending
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .insert(id, result);
            }
            continue;
        }

        let id = message.get("id").cloned();
        match message["method"].as_str().unwrap_or_default() {
            "initialize" => {
                let auth_methods = if matches!(scenario, "auth" | "auth_silent") {
                    json!([
                        {
                            "id": "oauth-personal",
                            "name": "Log in with Google",
                            "description": "OAuth for personal accounts",
                        },
                        {
                            "id": "api-key",
                            "name": "API key",
                            "description": "Use a developer API key",
                        },
                    ])
                } else {
                    json!([])
                };
                agent.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": 1,
                        // Every scenario here declines `session/load`, including
                        // the resume one — that is the fallback under test.
                        "agentCapabilities": { "loadSession": false, "promptCapabilities": {} },
                        "agentInfo": { "name": "acp-fixture", "version": "0.0.1" },
                        "authMethods": auth_methods,
                    },
                }));
            }
            "authenticate" => {
                let method = message
                    .get("params")
                    .and_then(|params| params.get("methodId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                agent.note("authenticate", json!({ "methodId": method }));
                agent.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {},
                }));
            }
            "session/load" => agent.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "this agent cannot load sessions" },
            })),
            // Authentication before the session exists. The client is blocked
            // inside `session/new` here, which used to be a moment when nothing
            // was servicing questions at all.
            "session/new" if scenario == "startup_auth" => {
                let agent = Arc::clone(&agent);
                thread::spawn(move || {
                    let outcome = agent.call(
                        "elicitation/create",
                        json!({
                            "mode": "url",
                            "elicitationId": "startup-001",
                            "url": "https://example.com/sign-in",
                            "message": "Sign in before the session starts.",
                        }),
                    );
                    agent.note("startup_auth", outcome);
                    agent.send(json!({
                        "jsonrpc": "2.0", "id": id, "result": { "sessionId": "s1" },
                    }));
                });
            }
            "session/new" => {
                let mut result = json!({ "sessionId": "s1" });
                if scenario == "config" {
                    result["configOptions"] = config_options("model-1");
                }
                agent.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            "session/prompt" => {
                // Off the read loop: several scenarios call back into the
                // client and would otherwise deadlock against their own reader.
                let agent = Arc::clone(&agent);
                let scenario = scenario.to_string();
                thread::spawn(move || run_scenario(&agent, &scenario, id));
            }
            _ => {}
        }
    }
}

fn run_scenario(agent: &Arc<Agent>, scenario: &str, id: Option<Value>) {
    let mut stop_reason = "end_turn";

    match scenario {
        "chunks" | "resume" => {
            for text in ["alpha ", "beta ", "gamma"] {
                agent.chunk("m1", text);
            }
            for text in ["delta ", "epsilon"] {
                agent.chunk("m2", text);
            }
        }
        "tools" => {
            agent.update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "c1",
                "title": "Analyze the code", "kind": "other", "status": "pending",
            }));
            agent.update(json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress",
            }));
            agent.update(json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "12 issues" } }],
            }));
        }
        "permission" => {
            let outcome = agent.call(
                "session/request_permission",
                json!({
                    "sessionId": "s1",
                    "toolCall": { "toolCallId": "c1", "title": "Delete the file" },
                    "options": [
                        { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                    ],
                }),
            );
            agent.note("permission", outcome);
        }
        "cancelled" => stop_reason = "cancelled",
        "config" => {
            // Agents push their own changes — a model falling back under rate
            // limiting is the canonical case — and the push carries the whole
            // list, not a delta.
            agent.update(json!({
                "sessionUpdate": "config_option_update",
                "configOptions": config_options("model-2"),
            }));
        }
        "junk" => {
            agent.send_raw("this is not JSON at all");
            agent.send_raw("{\"partial\": ");
            eprintln!("noisy diagnostic from the agent");
            // Forward-compatibility: ACP is explicitly extensible, so an
            // unknown update kind and a stray response id must both be ignored.
            agent.update(json!({ "sessionUpdate": "some_future_thing", "payload": 42 }));
            agent
                .send(json!({ "jsonrpc": "2.0", "id": 999_999, "result": { "unexpected": true } }));
            agent.chunk("m1", "still here");
        }
        "die" => {
            agent.chunk("m1", "about to vanish");
            std::process::exit(0);
        }
        "bigline" => {
            let payload = format!("HEAD{}TAIL", "x".repeat(300_000 - 8));
            agent.update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "c1",
                "title": "Dump", "kind": "read", "status": "pending",
            }));
            agent.update(json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": payload } }],
            }));
        }
        "terminal" => run_terminal_scenario(agent),
        "fs" => run_fs_scenario(agent),
        "form" | "form_decline" | "form_cancel" => {
            let outcome = agent.call(
                "elicitation/create",
                json!({
                    "sessionId": "s1", "mode": "form",
                    "message": "How should I approach this refactoring?",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {
                            "strategy": { "type": "string", "enum": ["conservative", "aggressive"] },
                            "runs": { "type": "integer", "default": 3 },
                            "note": { "type": "string" },
                        },
                        "required": ["strategy"],
                    },
                }),
            );
            agent.note("form", outcome);
        }
        "url" | "url_refused" => {
            let outcome = agent.call(
                "elicitation/create",
                json!({
                    "sessionId": "s1", "mode": "url",
                    "elicitationId": "oauth-001",
                    "url": "https://example.com/connect?elicitationId=oauth-001",
                    "message": "Authorize access to your repositories.",
                }),
            );
            let accepted = outcome.get("action").and_then(Value::as_str) == Some("accept");
            agent.note("url", outcome);
            if !accepted {
                // The user refused, so this id is dead; announcing it anyway
                // would report a step they declined to take.
                agent.send(json!({
                    "jsonrpc": "2.0", "method": "elicitation/complete",
                    "params": { "elicitationId": "oauth-001" },
                }));
                thread::sleep(Duration::from_millis(200));
            }
            if accepted {
                // Only after the out-of-band flow finishes; `accept` alone just
                // meant the user agreed to open the link.
                agent.send(json!({
                    "jsonrpc": "2.0", "method": "elicitation/complete",
                    "params": { "elicitationId": "oauth-001" },
                }));
                // Both must be ignored: an id we never issued, and one that
                // has already been completed.
                agent.send(json!({
                    "jsonrpc": "2.0", "method": "elicitation/complete",
                    "params": { "elicitationId": "never-issued" },
                }));
                agent.send(json!({
                    "jsonrpc": "2.0", "method": "elicitation/complete",
                    "params": { "elicitationId": "oauth-001" },
                }));
                thread::sleep(Duration::from_millis(200));
            }
        }
        _ => {}
    }

    agent.send(json!({
        "jsonrpc": "2.0", "id": id, "result": { "stopReason": stop_reason },
    }));
}

fn run_terminal_scenario(agent: &Arc<Agent>) {
    // 1. A real pty: only a tty makes `test -t 1` succeed.
    let created = agent.call(
        "terminal/create",
        json!({
            "sessionId": "s1", "command": "sh",
            "args": ["-c", "test -t 1 && echo ISATTY"],
        }),
    );
    let tty_id = created["terminalId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    agent.call(
        "terminal/wait_for_exit",
        json!({ "sessionId": "s1", "terminalId": tty_id }),
    );
    agent.note(
        "tty",
        agent.call(
            "terminal/output",
            json!({ "sessionId": "s1", "terminalId": tty_id }),
        ),
    );
    agent.call(
        "terminal/release",
        json!({ "sessionId": "s1", "terminalId": tty_id }),
    );

    // 2. `outputByteLimit` truncation keeps the tail.
    let capped = agent.call(
        "terminal/create",
        json!({
            "sessionId": "s1", "command": "sh",
            "args": ["-c", "i=1; while [ $i -le 200 ]; do echo LINE$i; i=$((i+1)); done"],
            "outputByteLimit": 200,
        }),
    );
    let capped_id = capped["terminalId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    agent.call(
        "terminal/wait_for_exit",
        json!({ "sessionId": "s1", "terminalId": capped_id }),
    );
    agent.note(
        "capped",
        agent.call(
            "terminal/output",
            json!({ "sessionId": "s1", "terminalId": capped_id }),
        ),
    );

    // 3. Lifecycle: readable after kill, gone after release.
    let long = agent.call(
        "terminal/create",
        json!({ "sessionId": "s1", "command": "sh", "args": ["-c", "sleep 30"] }),
    );
    let long_id = long["terminalId"].as_str().unwrap_or_default().to_string();
    agent.call(
        "terminal/kill",
        json!({ "sessionId": "s1", "terminalId": long_id }),
    );
    let after_kill = agent.call(
        "terminal/output",
        json!({ "sessionId": "s1", "terminalId": long_id }),
    );
    agent.note(
        "after_kill",
        json!({ "ok": after_kill.get("output").is_some() }),
    );

    agent.call(
        "terminal/release",
        json!({ "sessionId": "s1", "terminalId": long_id }),
    );
    let after_release = agent.call(
        "terminal/output",
        json!({ "sessionId": "s1", "terminalId": long_id }),
    );
    agent.note(
        "after_release",
        json!({ "ok": after_release.get("output").is_some() }),
    );
}

/// Every way an agent might reach for a file, inside the session and out.
///
/// ACP hands the client the filesystem, so nothing but the client stands
/// between "the agent asked for ~/.ssh/id_rsa" and it being read.
fn run_fs_scenario(agent: &Arc<Agent>) {
    let cwd = env::current_dir().expect("the agent runs in the session directory");
    let inside = cwd.join("notes.txt");
    let read = |path: PathBuf| json!({ "sessionId": "s1", "path": path.display().to_string() });

    agent.note(
        "write_inside",
        agent.call(
            "fs/write_text_file",
            json!({
                "sessionId": "s1",
                "path": inside.display().to_string(),
                "content": "kept\n",
            }),
        ),
    );
    agent.note("read_inside", agent.call("fs/read_text_file", read(inside)));

    // `..` is folded before the check, so this is the parent directory rather
    // than something that merely looks like it is under the session.
    let escape = cwd.join("../escape.txt");
    agent.note(
        "read_outside",
        agent.call("fs/read_text_file", read(escape.clone())),
    );
    agent.note(
        "write_outside",
        agent.call(
            "fs/write_text_file",
            json!({
                "sessionId": "s1",
                "path": escape.display().to_string(),
                "content": "leaked",
            }),
        ),
    );

    // A symlink the agent itself planted inside the session is the other way
    // out, and the one a lexical check alone would miss.
    let link = cwd.join("out");
    let _ = std::os::unix::fs::symlink(cwd.parent().unwrap_or(Path::new("/")), &link);
    agent.note(
        "read_symlink",
        agent.call("fs/read_text_file", read(link.join("escape.txt"))),
    );
}

/// The config an agent exposes, with `current` selected as the model. Includes
/// a boolean option, which an agent may only send once the client advertised
/// support for rendering one.
fn config_options(current: &str) -> Value {
    json!([
        {
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": current,
            "options": [
                { "value": "model-1", "name": "Sonnet" },
                { "value": "model-2", "name": "Opus" },
            ],
        },
        {
            "id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select",
            "currentValue": "high",
            "options": [{ "value": "high", "name": "Extra" }],
        },
        { "id": "brave", "name": "Brave Mode", "type": "boolean", "currentValue": true },
    ])
}

fn session_config_options_are_reported_and_relabelled() {
    let session = run_bridge("config", "go\n");

    // Opaque ids are useless in a header; the pane shows the choice names, and
    // a boolean reads as on/off rather than `true`.
    assert!(
        session.pane.contains("Model: Sonnet"),
        "setup config should be shown: {}",
        session.pane
    );
    assert!(
        session.pane.contains("Thinking: Extra") && session.pane.contains("Brave Mode: on"),
        "every option should be summarised: {}",
        session.pane
    );

    // The agent-initiated update replaces the previous state wholesale.
    let last_model = session
        .pane
        .rmatch_indices("Model: ")
        .next()
        .map(|(index, _)| session.pane[index..].lines().next().unwrap_or_default())
        .unwrap_or_default();
    assert!(
        last_model.contains("Model: Opus"),
        "a config_option_update should supersede the setup values, got {last_model:?}"
    );
}

fn initialize_advertises_boolean_config_support() {
    // Agents MUST NOT send boolean options unless the client advertised it, so
    // the capability and the boolean assertion above have to travel together.
    let session = run_bridge("config", "go\n");
    let initialize: Value = serde_json::from_str(&session.frames[0]).expect("first frame parses");
    assert_eq!(
        initialize["params"]["clientCapabilities"]["session"]["configOptions"]["boolean"],
        json!({}),
        "boolean config support should be advertised: {}",
        session.frames[0]
    );
}

fn a_form_elicitation_collects_typed_answers() {
    // strategy by index, runs left blank to take its default, note skipped.
    let session = run_bridge("form", "go\n2\n\n\n");
    let outcome = session.note("form");

    assert_eq!(outcome["action"], "accept");
    assert_eq!(outcome["content"]["strategy"], "aggressive");
    assert_eq!(outcome["content"]["runs"], 3, "the default fills in");
    assert!(
        outcome["content"].get("note").is_none(),
        "a blank optional is absent, not an empty string: {outcome}"
    );
    assert!(
        session
            .pane
            .contains("How should I approach this refactoring?"),
        "the message should be shown: {}",
        session.pane
    );
}

fn a_declined_form_is_distinct_from_a_cancelled_one() {
    // Agents are required to branch on these, so they must not collapse.
    assert_eq!(
        run_bridge("form_decline", "go\n/decline\n").note("form")["action"],
        "decline"
    );
    assert_eq!(
        run_bridge("form_cancel", "go\n/cancel\n").note("form")["action"],
        "cancel"
    );
}

fn a_url_elicitation_needs_consent_and_reports_completion() {
    let session = run_bridge("url", "go\ny\n");
    assert_eq!(session.note("url")["action"], "accept");

    // The spec requires the full URL be shown before consent is asked for.
    assert!(
        session
            .pane
            .contains("https://example.com/connect?elicitationId=oauth-001"),
        "the full URL must be displayed: {}",
        session.pane
    );
    assert!(
        session.pane.contains("the browser step finished"),
        "elicitation/complete should close the loop: {}",
        session.pane
    );
    // The follow-up completions carried an id we never issued and one already
    // closed; both must be ignored rather than reported again.
    assert_eq!(
        session.pane.matches("the browser step finished").count(),
        1,
        "unknown and repeated elicitationIds must be ignored: {}",
        session.pane
    );
}

fn a_refused_url_elicitation_opens_nothing() {
    // Bare enter is the safe default: no consent, no navigation.
    let session = run_bridge("url_refused", "go\n\n");
    assert_eq!(session.note("url")["action"], "cancel");
    assert!(
        !session.pane.contains("the browser step finished"),
        "nothing should have been opened: {}",
        session.pane
    );
}

fn initialize_advertises_both_elicitation_modes() {
    // Each mode must be present and non-null; an empty object advertises
    // neither, and an agent may not use a mode it wasn't offered.
    let session = run_bridge("form", "go\n1\n\n\n");
    let initialize: Value = serde_json::from_str(&session.frames[0]).expect("first frame parses");
    let elicitation = &initialize["params"]["clientCapabilities"]["elicitation"];
    assert_eq!(elicitation["form"], json!({}), "{initialize}");
    assert_eq!(elicitation["url"], json!({}), "{initialize}");
}

fn a_streaming_bridge_writes_no_local_transcript() {
    // A remote bridge cannot see the filesystem the sidebar tails, so it must
    // not write a transcript there — the records go to qmux over the control
    // socket, which appends them to the local file itself.
    let session = run_bridge_streaming("chunks", "hello\n");

    assert!(
        session.transcript.is_empty(),
        "streaming must not write a local transcript: {:#?}",
        session.transcript
    );
    // The session itself is unaffected — the pane still renders everything.
    assert!(
        session.pane.contains("alpha beta gamma"),
        "the pane should still stream: {}",
        session.pane
    );
    // stderr still lands in a log, just one chosen for this machine.
    assert!(
        session
            .frames
            .iter()
            .any(|frame| frame.contains("initialize")),
        "the protocol still runs normally"
    );
}

fn a_question_raised_before_the_session_opens_reaches_the_pane() {
    // The one place a question had no servicer: the main thread was blocked in
    // `session/new`, and the only loop that drained interactions ran inside a
    // turn. The agent waited for an answer that could never arrive, and the
    // pane hung with nothing on it — so a hang here is the regression.
    let session = run_bridge("startup_auth", "y\ngo\n");

    assert_eq!(
        session.note("startup_auth")["action"],
        "accept",
        "the sign-in prompt should have been answered from the pane"
    );
    assert!(
        session.pane.contains("https://example.com/sign-in"),
        "the URL should have been shown before the session opened: {}",
        session.pane
    );
    // And the session goes on to do its job with the input that follows.
    assert!(
        session
            .turns()
            .iter()
            .any(|turn| turn["role"] == "user" && text_of(turn) == "go"),
        "the prompt after the sign-in should still run: {:#?}",
        session.transcript
    );
}

fn the_filesystem_is_confined_to_the_session_directory() {
    let session = run_bridge("fs", "go\n");

    // Inside the session, the agent has the filesystem it was given.
    assert_eq!(session.note("read_inside")["content"], "kept\n");

    // Outside it, nothing — by `..`, by absolute path, or through a symlink
    // the agent planted itself.
    for note in ["read_outside", "write_outside", "read_symlink"] {
        let outcome = session.note(note);
        let message = outcome["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("outside this session's directory"),
            "{note} should have been refused, got {outcome}"
        );
    }
}

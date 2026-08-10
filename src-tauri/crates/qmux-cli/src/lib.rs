//! The qmux in-pane CLI. On the local machine the qmux app binary doubles as
//! the CLI (the app's `main` dispatches through [`run_cli_if_requested`]
//! before starting Tauri); the standalone `qmux-cli` binary built from this
//! crate carries the same commands without the app, so a host that never runs
//! the app — a remote box reached over ssh — can still service hooks, cwd
//! reporting, and forks once a transport exists.

mod acp;
mod mcp;
mod muse;
mod public_cli;

use qmux_proto::{ControlRequest, ControlResponse};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;
use std::time::Duration;

const MCP_USAGE_HINT: &str = "Add `qmux mcp` as a stdio MCP server in your agent CLI.\nRun it inside a qmux agent pane so it inherits the authenticated environment.";
const SYNTAX_ERROR_PREFIX: &str = "\u{1d}qmux-syntax:";

fn syntax_error(message: String) -> String {
    format!("{SYNTAX_ERROR_PREFIX}{message}")
}

pub fn error_report(error: &str) -> (&str, i32) {
    if let Some(message) = error.strip_prefix(SYNTAX_ERROR_PREFIX) {
        (message, 2)
    } else if error.starts_with("usage:") {
        (error, 2)
    } else {
        (error, 1)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreparedAgentLaunch {
    binary: String,
    cwd: String,
    args: Vec<String>,
    envs: Vec<PreparedLaunchEnv>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreparedLaunchEnv {
    key: String,
    value: String,
}

pub fn run_cli_if_requested() -> Result<bool, String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        if env::var_os("QMUX_SOCK").is_some() && env::var_os("QMUX_TOKEN").is_some() {
            println!("{MCP_USAGE_HINT}");
            return Ok(true);
        }
        return Ok(false);
    };
    if command == "--skill" {
        print!("{}", public_cli::SKILL);
        return Ok(true);
    }
    let remaining = args.collect::<Vec<_>>();
    if public_cli::run(&command, remaining.clone())? {
        return Ok(true);
    }
    let mut args = remaining.into_iter();

    match command.as_str() {
        "notify" => {
            let event = args
                .next()
                .ok_or_else(|| "usage: qmux notify <event>".to_string())?;
            let mut stdin = String::new();
            std::io::stdin()
                .read_to_string(&mut stdin)
                .map_err(|err| format!("failed to read stdin: {err}"))?;
            let payload = parse_payload(&stdin);
            request_silent(
                "hook.notify",
                json!({
                    "event": event,
                    "paneId": env::var("QMUX_PANE_ID").ok(),
                    "agentId": env::var("QMUX_AGENT_ID").ok(),
                    "adapterId": env::var("QMUX_ADAPTER_ID").ok(),
                    "payload": payload,
                }),
            )?;
            Ok(true)
        }
        "muse-notify" => {
            // Muse's hook environment has no QMUX_* variables to identify the
            // pane with, so this command resolves it from the payload instead.
            // See the `muse` module.
            let event = args
                .next()
                .ok_or_else(|| "usage: qmux muse-notify <event> [bindings-dir]".to_string())?;
            // The generated shim always supplies the directory, because Muse
            // strips every variable it could otherwise be derived from.
            muse::notify(event, args.next())?;
            Ok(true)
        }
        "cwd" => {
            // Reports the shell pane's current directory so a restart can reopen it
            // where the user left off. The server binds the update to the pane that
            // owns the presented token, so the claimed paneId is advisory only.
            let cwd = env::current_dir()
                .map_err(|err| format!("failed to read current directory: {err}"))?;
            request_silent(
                "pane.set_cwd",
                json!({
                    "paneId": env::var("QMUX_PANE_ID").ok(),
                    "cwd": cwd.display().to_string(),
                }),
            )?;
            Ok(true)
        }
        "pane-write" => {
            let pane_id = args
                .next()
                .ok_or_else(|| "usage: qmux pane-write <pane-id> <text>".to_string())?;
            let data = args.collect::<Vec<_>>().join(" ");
            request_and_print(
                "pane.write",
                json!({
                    "paneId": pane_id,
                    "data": data,
                    "paste": true,
                    "submit": true,
                }),
            )?;
            Ok(true)
        }
        "agent-exec" => {
            let adapter_id = args
                .next()
                .ok_or_else(|| "usage: qmux agent-exec <adapter-id> [args...]".to_string())?;
            run_agent_exec(adapter_id, args.collect())?;
            Ok(true)
        }
        "agent-detach" => {
            // Run by the shell wrapper once an in-shell agent process exits, so the tab
            // reverts to a plain shell rather than keeping the agent's last status. The
            // server detaches the agent bound to this pane's token; the claimed paneId is
            // advisory. Best-effort — failures (e.g. no agent attached) must not surface
            // at the prompt, so the wrapper discards this command's output.
            request_silent(
                "agent.detach_pane",
                json!({ "paneId": env::var("QMUX_PANE_ID").ok() }),
            )?;
            Ok(true)
        }
        "claude" => {
            run_agent_exec("claude".to_string(), args.collect())?;
            Ok(true)
        }
        "codex" => {
            run_agent_exec("codex".to_string(), args.collect())?;
            Ok(true)
        }
        "grok" => {
            run_agent_exec("grok".to_string(), args.collect())?;
            Ok(true)
        }
        "muse" => {
            run_agent_exec("muse".to_string(), args.collect())?;
            Ok(true)
        }
        "mcp" => {
            mcp::run()?;
            Ok(true)
        }
        "acp" => {
            acp::run(args.collect())?;
            Ok(true)
        }
        "fork" => {
            let mut use_worktree = false;
            let mut prompt_parts = Vec::new();
            let mut parse_options = true;
            for arg in args {
                if parse_options && arg == "--" {
                    parse_options = false;
                    continue;
                }
                if parse_options && (arg == "--worktree" || arg == "-w") {
                    use_worktree = true;
                    continue;
                }
                prompt_parts.push(arg);
            }
            let prompt = (!prompt_parts.is_empty()).then(|| prompt_parts.join(" "));
            let pane = request_value(
                "agent.fork",
                json!({ "useWorktree": use_worktree, "prompt": prompt }),
            )?;
            let title = pane
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("the session");
            let suffix = if use_worktree {
                " in a fresh worktree"
            } else {
                ""
            };
            let prompt_suffix = if prompt.is_some() {
                " and submitted the launch message"
            } else {
                ""
            };
            println!("Forked {title} into a new tab nested under this one{suffix}{prompt_suffix}.");
            Ok(true)
        }
        "open" => {
            let target = args
                .next()
                .ok_or_else(|| "usage: qmux open <file|url>".to_string())?;
            let cwd = env::current_dir()
                .ok()
                .map(|path| path.display().to_string());
            let data = request_value("browser.open", json!({ "target": target, "cwd": cwd }))?;
            let url = data
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or(target.as_str());
            println!("Opened {url} in the qmux browser overlay.");
            Ok(true)
        }
        "ping" => {
            request_and_print("ping", json!({}))?;
            Ok(true)
        }
        "help" | "--help" | "-h" => {
            println!("{}", public_cli::HELP);
            Ok(true)
        }
        _ if env::var("QMUX_ENV").ok().as_deref() == Some("1") => {
            Err(syntax_error(format!("unknown qmux command '{command}'")))
        }
        _ => Ok(false),
    }
}

fn run_agent_exec(adapter_id: String, args: Vec<String>) -> Result<(), String> {
    let pane_id = env::var("QMUX_PANE_ID")
        .map_err(|_| "QMUX_PANE_ID is not set; run this from a qmux shell pane".to_string())?;
    let cwd = env::current_dir()
        .map_err(|err| format!("failed to read current directory for agent launch: {err}"))?;
    let supervisor_pid = std::process::id();
    let shell_job_id = format!(
        "shell-job-{supervisor_pid}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    let launch = request_value(
        "agent.prepare_shell_launch",
        json!({
            "adapterId": adapter_id.clone(),
            "paneId": pane_id,
            "cwd": cwd.display().to_string(),
            "args": args,
            "shellJobId": shell_job_id,
            "supervisorPid": supervisor_pid,
            "preparedAgentId": env::var("QMUX_PREPARED_AGENT_ID").ok(),
        }),
    )?;
    let launch = serde_json::from_value::<PreparedAgentLaunch>(launch)
        .map_err(|err| format!("invalid prepared agent launch response: {err}"))?;

    let mut command = Command::new(&launch.binary);
    command.args(launch.args).current_dir(&launch.cwd);
    let agent_id = launch
        .envs
        .iter()
        .find(|env| env.key == "QMUX_AGENT_ID")
        .map(|env| env.value.clone())
        .ok_or_else(|| "prepared shell launch is missing its agent id".to_string())?;
    for env in launch.envs {
        command.env(env.key, env.value);
    }
    // The containing shell is a user principal. The agent process receives the
    // pane token needed by hooks/MCP, but never inherits cross-pane user power.
    command.env_remove("QMUX_USER_TOKEN");
    // Lifecycle notifications normally resolve their adapter through the bound
    // agent id. Preserve an explicit hint as well so an authenticated SessionStart
    // can reconstruct that binding if preparation state was lost.
    command.env("QMUX_ADAPTER_ID", &adapter_id);

    // The agent must own Ctrl-C/Ctrl-\ itself, so restore the default disposition in the
    // child after fork (it inherits the SIG_IGN we install below before exec). SIGTSTP is
    // left untouched so a stop still suspends both processes together.
    unsafe {
        command.pre_exec(|| {
            // Runs in the forked child before exec; only async-signal-safe calls allowed.
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }

    // Ignore SIGINT/SIGQUIT in the supervisor before spawning so a terminal-generated
    // signal (delivered to the whole foreground process group during any cooked-mode
    // window) can't kill us before wait() returns. Surviving the signal is what
    // guarantees the detach cleanup below always runs.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }

    let status = command
        .spawn()
        .and_then(|mut child| child.wait())
        .map_err(|err| format!("failed to run agent binary '{}': {err}", launch.binary));

    // Cleanup belongs to the runner, not the injected shell function. A suspended or
    // backgrounded job can return control to the shell before exiting; detaching here,
    // after wait() reports a real exit, keeps the pane-agent binding alive across
    // job-control stop/continue cycles.
    let _ = request_silent(
        "agent.detach_pane",
        json!({
            "paneId": env::var("QMUX_PANE_ID").ok(),
            "jobId": shell_job_id,
            "agentId": agent_id,
        }),
    );

    let status = status?;
    std::process::exit(exit_code_for_status(status));
}

fn exit_code_for_status(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| status.signal().map(|signal| 128 + signal).unwrap_or(1))
}

pub(crate) fn request_silent(command: &str, payload: Value) -> Result<(), String> {
    request(command, payload).map(|_| ())
}

/// As [`request_silent`], but with the socket and token supplied by the caller
/// rather than read from the environment. The Muse hook path needs this: its
/// shim runs with `QMUX_*` stripped and recovers both values from the pane
/// binding file instead.
pub(crate) fn request_silent_with(
    socket_path: &str,
    token: &str,
    command: &str,
    payload: Value,
) -> Result<(), String> {
    send_request(socket_path, token, command, payload).map(|_| ())
}

fn request_and_print(command: &str, payload: Value) -> Result<(), String> {
    let response = request(command, payload)?;
    println!("{response}");
    Ok(())
}

fn request_value(command: &str, payload: Value) -> Result<Value, String> {
    request_value_with_timeout(command, payload, Duration::from_secs(2))
}

pub(crate) fn request_value_with_timeout(
    command: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let raw = request_with_timeout(command, payload, timeout)?;
    let response = serde_json::from_str::<ControlResponse>(&raw)
        .map_err(|err| format!("invalid qmux response: {err}"))?;
    if response.ok {
        Ok(response.data)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "qmux request failed".to_string()))
    }
}

pub(crate) fn request_public(
    operation: &str,
    arguments: Value,
) -> Result<qmux_proto::PublicControlResponse, String> {
    let socket_path = env::var("QMUX_SOCK").map_err(|_| "QMUX_SOCK is not set".to_string())?;
    let token = env::var("QMUX_USER_TOKEN")
        .or_else(|_| env::var("QMUX_TOKEN"))
        .map_err(|_| "neither QMUX_USER_TOKEN nor QMUX_TOKEN is set".to_string())?;
    let timeout = public_request_timeout(operation, &arguments);
    let raw = send_request_with_timeout(
        &socket_path,
        &token,
        "cli.call",
        json!({ "operation": operation, "arguments": arguments }),
        timeout,
    )?;
    let outer = serde_json::from_str::<ControlResponse>(&raw)
        .map_err(|error| format!("invalid qmux response: {error}"))?;
    if !outer.ok {
        return Err(outer
            .error
            .unwrap_or_else(|| "qmux request failed".to_string()));
    }
    decode_public_response(outer.data)
}

fn decode_public_response(data: Value) -> Result<qmux_proto::PublicControlResponse, String> {
    let response = serde_json::from_value::<qmux_proto::PublicControlResponse>(data)
        .map_err(|error| format!("invalid qmux public response: {error}"))?;
    if response.api_version != qmux_proto::PUBLIC_API_VERSION {
        return Err(format!(
            "unsupported qmux public API version {}; this CLI supports version {}",
            response.api_version,
            qmux_proto::PUBLIC_API_VERSION
        ));
    }
    Ok(response)
}

fn public_request_timeout(operation: &str, arguments: &Value) -> Duration {
    const MAX_WAIT_MS: u64 = 600_000;
    const RESPONSE_GRACE_MS: u64 = 5_000;
    if matches!(operation, "pane.waitOutput" | "agent.wait") {
        let wait_ms = arguments
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .min(MAX_WAIT_MS);
        return Duration::from_millis(wait_ms + RESPONSE_GRACE_MS);
    }
    // Agent launches and native-pane creation can legitimately take longer than
    // the two-second hook/control fast path. Public calls still stay bounded.
    Duration::from_secs(30)
}

fn request(command: &str, payload: Value) -> Result<String, String> {
    request_with_timeout(command, payload, Duration::from_secs(2))
}

fn request_with_timeout(
    command: &str,
    payload: Value,
    timeout: Duration,
) -> Result<String, String> {
    let socket_path = env::var("QMUX_SOCK").map_err(|_| "QMUX_SOCK is not set".to_string())?;
    let token = env::var("QMUX_TOKEN").map_err(|_| "QMUX_TOKEN is not set".to_string())?;
    send_request_with_timeout(&socket_path, &token, command, payload, timeout)
}

fn send_request(
    socket_path: &str,
    token: &str,
    command: &str,
    payload: Value,
) -> Result<String, String> {
    send_request_with_timeout(socket_path, token, command, payload, Duration::from_secs(2))
}

fn send_request_with_timeout(
    socket_path: &str,
    token: &str,
    command: &str,
    payload: Value,
    timeout: Duration,
) -> Result<String, String> {
    let token = token.to_string();
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|err| format!("failed to connect to {socket_path}: {err}"))?;
    let timeout = Some(timeout);
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let request = ControlRequest {
        token,
        command: command.to_string(),
        payload,
    };

    serde_json::to_writer(&mut stream, &request)
        .map_err(|err| format!("failed to encode request: {err}"))?;
    stream
        .write_all(b"\n")
        .map_err(|err| format!("failed to send request: {err}"))?;
    stream
        .flush()
        .map_err(|err| format!("failed to flush request: {err}"))?;

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|err| format!("failed to read response: {err}"))?;
    Ok(response.trim_end().to_string())
}

fn parse_payload(input: &str) -> Value {
    if input.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_usage_hint_is_at_most_two_lines_and_names_the_server_command() {
        assert!(MCP_USAGE_HINT.lines().count() <= 2);
        assert!(MCP_USAGE_HINT.contains("qmux mcp"));
        assert!(MCP_USAGE_HINT.contains("stdio"));
    }

    #[test]
    fn public_parser_failures_are_syntax_errors() {
        let error = public_cli::run("pane", vec!["wat".into()]).unwrap_err();
        let (message, exit_code) = error_report(&error);
        assert_eq!(exit_code, 2);
        assert_eq!(message, "unknown pane command 'wat'");
    }

    #[test]
    fn public_wait_timeout_includes_the_requested_wait_and_response_grace() {
        assert_eq!(
            public_request_timeout("pane.waitOutput", &json!({ "timeoutMs": 120_000 })),
            Duration::from_secs(125)
        );
        assert_eq!(
            public_request_timeout("agent.wait", &json!({ "timeoutMs": 900_000 })),
            Duration::from_secs(605)
        );
        assert_eq!(
            public_request_timeout("pane.list", &json!({})),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn public_response_rejects_an_unknown_api_version() {
        let error = decode_public_response(json!({
            "ok": true,
            "apiVersion": qmux_proto::PUBLIC_API_VERSION + 1,
            "result": {},
            "error": null
        }))
        .unwrap_err();
        assert!(error.contains("unsupported qmux public API version"));
    }
}

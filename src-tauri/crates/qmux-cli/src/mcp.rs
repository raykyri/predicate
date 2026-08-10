//! Minimal MCP stdio bridge for agents launched by qmux.
//!
//! Tool calls are not executed here. They cross the authenticated per-pane
//! control socket, where the app resolves the caller from its unforgeable token
//! and applies lineage/workspace capability checks.

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_WAIT_SECONDS: u64 = 600;

pub fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock(), |name, arguments, timeout| {
        super::request_value_with_timeout(
            "mcp.call",
            json!({ "name": name, "arguments": arguments }),
            timeout,
        )
    })
}

fn serve<R, W, F>(reader: R, mut writer: W, mut call: F) -> Result<(), String>
where
    R: BufRead,
    W: Write,
    F: FnMut(&str, Value, Duration) -> Result<Value, String>,
{
    for line in reader.lines() {
        let line = line.map_err(|err| format!("failed to read MCP request: {err}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Value>(&line) {
            Ok(request) => request,
            Err(err) => {
                write_message(
                    &mut writer,
                    &rpc_error(Value::Null, -32700, &err.to_string()),
                )?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            // MCP notifications, including `notifications/initialized`, have no reply.
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
                let requested_protocol = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION);
                let protocol = if requested_protocol == PROTOCOL_VERSION {
                    requested_protocol
                } else {
                    PROTOCOL_VERSION
                };
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": protocol,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "qmux", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": "You are running inside qmux. Use these tools whenever the user asks you to delegate, parallelize work, inspect delegates, or coordinate another agent. Typical flow: spawn_agent (optionally in a worktree with a prompt), wait_for_children, summarize_children, send_prompt for follow-up, then release_agent. Delegates should finish with report_to_parent. Writes and release are limited to direct relatives; reads stay inside your live workspace lineage."
                    }),
                )
            }
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    write_message(&mut writer, &rpc_error(id, -32602, "missing tool name"))?;
                    continue;
                };
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let timeout_seconds = arguments
                    .get("timeoutSeconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(30)
                    .min(MAX_WAIT_SECONDS);
                let timeout = Duration::from_secs(timeout_seconds.saturating_add(5).max(5));
                match call(name, arguments, timeout) {
                    Ok(value) => rpc_result(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()) }],
                            "structuredContent": value,
                            "isError": false
                        }),
                    ),
                    Err(err) => rpc_result(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": err }],
                            "isError": true
                        }),
                    ),
                }
            }
            _ => rpc_error(id, -32601, &format!("method not found: {method}")),
        };
        write_message(&mut writer, &response)?;
    }
    Ok(())
}

fn write_message(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|err| format!("failed to encode MCP response: {err}"))?;
    writer
        .write_all(b"\n")
        .and_then(|_| writer.flush())
        .map_err(|err| format!("failed to write MCP response: {err}"))
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "whoami",
            "Show your qmux identity, lineage, workspace, and capability policy.",
            object(json!({}), &[]),
        ),
        tool(
            "spawn_agent",
            "Spawn a fresh direct child agent in your workspace and optionally give it an initial prompt.",
            object(
                json!({
                    "adapter": { "type": "string", "description": "Configured adapter id; defaults to your adapter." },
                    "prompt": { "type": "string" },
                    "useWorktree": { "type": "boolean", "default": false }
                }),
                &[],
            ),
        ),
        tool(
            "fork_self",
            "Fork your current provider session as a direct child, preserving conversation context.",
            object(
                json!({
                    "prompt": { "type": "string" },
                    "useWorktree": { "type": "boolean", "default": false }
                }),
                &[],
            ),
        ),
        tool(
            "list_children",
            "List live children; include all live descendants when recursive is true.",
            object(
                json!({
                    "recursive": { "type": "boolean", "default": false }
                }),
                &[],
            ),
        ),
        tool(
            "send_prompt",
            "Send or queue a prompt to your parent or one direct child.",
            object(
                json!({
                    "agentId": { "type": "string" },
                    "text": { "type": "string" }
                }),
                &["agentId", "text"],
            ),
        ),
        tool(
            "wait_for_children",
            "Wait until selected direct children settle, finish, or exit.",
            object(
                json!({
                    "agentIds": { "type": "array", "items": { "type": "string" } },
                    "until": { "type": "string", "enum": ["settled", "done", "exited"], "default": "settled" },
                    "timeoutSeconds": { "type": "integer", "minimum": 0, "maximum": 600, "default": 30 }
                }),
                &[],
            ),
        ),
        tool(
            "summarize_children",
            "Collect bounded non-blank output tails, status, and explicit artifacts for selected live direct children.",
            object(
                json!({
                    "agentIds": { "type": "array", "items": { "type": "string" } },
                    "lines": { "type": "integer", "minimum": 1, "maximum": 200, "default": 40 }
                }),
                &[],
            ),
        ),
        tool(
            "release_agent",
            "Terminate and close one live direct child agent after its work is collected. Refuses while that child has live descendants.",
            object(
                json!({
                    "agentId": { "type": "string" }
                }),
                &["agentId"],
            ),
        ),
        tool(
            "get_artifacts",
            "List explicit artifacts for yourself or a live descendant.",
            object(
                json!({
                    "agentId": { "type": "string" }
                }),
                &[],
            ),
        ),
        tool(
            "report_to_parent",
            "Send a structured result to the agent that spawned you. Include evidence and changed paths so the parent can verify and synthesize it.",
            object(
                json!({
                    "status": { "type": "string", "enum": ["update", "done", "blocked", "failed"], "default": "update" },
                    "summary": { "type": "string" },
                    "details": { "type": "string" },
                    "blockers": { "type": "array", "items": { "type": "string" } },
                    "questions": { "type": "array", "items": { "type": "string" } },
                    "nextSteps": { "type": "array", "items": { "type": "string" } },
                    "changedPaths": { "type": "array", "items": { "type": "string" } },
                    "artifacts": { "type": "array", "items": { "type": "string" } },
                    "proof": { "type": "array", "items": { "type": "string" } }
                }),
                &["summary"],
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn serves_initialize_list_and_call() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"test\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"whoami\",\"arguments\":{}}}\n",
        );
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, |name, _, _| {
            Ok(json!({ "called": name }))
        })
        .unwrap();
        let rows = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(rows[1]["result"]["tools"].as_array().unwrap().len(), 10);
        assert_eq!(rows[2]["result"]["structuredContent"]["called"], "whoami");
    }

    #[test]
    fn tool_failures_are_mcp_tool_errors() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"send_prompt\",\"arguments\":{}}}\n";
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, |_, _, _| {
            Err("denied".to_string())
        })
        .unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(response["result"]["content"][0]["text"], "denied");
    }

    #[test]
    fn initialize_keeps_the_supported_protocol_when_the_client_requests_an_unknown_one() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"future\"}}\n";
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, |_, _, _| unreachable!()).unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
    }
}

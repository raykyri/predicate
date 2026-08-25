use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::codex::CodexAdapter;
use crate::adapters::grok::GrokAdapter;
use crate::adapters::new_uuid_v4;
use crate::config::QmuxConfig;
use crate::headless_process::{JsonlProcess, JsonlReceive};
use crate::research::ResearchNode;
use crate::workspace::GroupInfo;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RESEARCH_TITLE_SOURCE_CHARS: usize = 4_000;
const RESEARCH_TITLE_MAX_CHARS: usize = 80;
const RESEARCH_TITLE_TIMEOUT: Duration = Duration::from_secs(60);
const TITLE_SCHEMA: &str = r#"{"type":"object","properties":{"title":{"type":"string"}},"required":["title"],"additionalProperties":false}"#;

#[cfg(all(target_os = "macos", qmux_foundation_models))]
mod foundation_models {
    use serde::Deserialize;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;

    unsafe extern "C" {
        fn qmux_generate_foundation_title(message: *const c_char) -> *mut c_char;
        fn qmux_free_foundation_title(message: *mut c_char);
    }

    #[derive(Deserialize)]
    struct TitleResponse {
        title: Option<String>,
        error: Option<String>,
    }

    pub fn generate(message: &str) -> Result<String, String> {
        let message = CString::new(message)
            .map_err(|_| "message contains an interior NUL byte".to_string())?;
        let response = unsafe { qmux_generate_foundation_title(message.as_ptr()) };
        if response.is_null() {
            return Err("Apple Foundation Models returned no response".to_string());
        }

        let raw = unsafe { CStr::from_ptr(response).to_string_lossy().into_owned() };
        unsafe { qmux_free_foundation_title(response) };

        let response: TitleResponse = serde_json::from_str(&raw)
            .map_err(|err| format!("Apple Foundation Models returned invalid JSON: {err}"))?;
        if let Some(error) = response.error.filter(|error| !error.trim().is_empty()) {
            return Err(error);
        }
        response
            .title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .ok_or_else(|| "Apple Foundation Models returned no title".to_string())
    }
}

#[cfg(all(target_os = "macos", qmux_foundation_models))]
pub fn generate_foundation_title(message: &str) -> Result<String, String> {
    foundation_models::generate(message)
}

#[cfg(not(all(target_os = "macos", qmux_foundation_models)))]
pub fn generate_foundation_title(_message: &str) -> Result<String, String> {
    Err("Apple Foundation Models are not available in this build".to_string())
}

pub fn foundation_models_available() -> bool {
    cfg!(all(target_os = "macos", qmux_foundation_models))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResearchTitleFlavor {
    Claude,
    Codex,
    Grok,
}

impl ResearchTitleFlavor {
    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
        }
    }
}

/// Runs a fresh, title-only request through the research node's own adapter and
/// model. It deliberately does not resume the research session: title metadata
/// must never become context inherited by later research branches.
pub fn generate_research_agent_title(
    config: &QmuxConfig,
    node: &ResearchNode,
    workspace: &GroupInfo,
) -> Result<String, String> {
    let flavor = match node.adapter.as_str() {
        "claude" => ResearchTitleFlavor::Claude,
        "codex" => ResearchTitleFlavor::Codex,
        "grok" => ResearchTitleFlavor::Grok,
        adapter => return Err(format!("'{adapter}' cannot generate research titles")),
    };
    let binary = match flavor {
        ResearchTitleFlavor::Claude => ClaudeAdapter::new(config).ensure_binary_for_sdk(),
        ResearchTitleFlavor::Codex => CodexAdapter::new(config).ensure_binary(),
        ResearchTitleFlavor::Grok => GrokAdapter::new(config).ensure_binary(),
    }?;
    let source = node
        .prompt
        .chars()
        .take(RESEARCH_TITLE_SOURCE_CHARS)
        .collect::<String>();
    if source.trim().is_empty() {
        return Err("research query has no text to title".to_string());
    }
    let prompt = research_title_prompt(&source);
    let cwd = PathBuf::from(&workspace.dir);
    let schema_file = (flavor == ResearchTitleFlavor::Codex)
        .then(|| TitleSchemaFile::create(config))
        .transpose()?;
    let grok_session_id = (flavor == ResearchTitleFlavor::Grok)
        .then(new_uuid_v4)
        .transpose()?;
    let args = build_research_title_args(
        flavor,
        &cwd,
        &prompt,
        node.model.as_deref(),
        schema_file.as_ref().map(|file| file.path.as_path()),
        grok_session_id.as_deref(),
    );
    let stderr_log = config
        .workspace_root
        .join(".qmux")
        .join("research-logs")
        .join(format!("{}-title.log", node.id));
    run_research_title_process(&binary, &args, &cwd, &stderr_log, flavor)
}

fn research_title_prompt(source: &str) -> String {
    format!(
        "Create a concise title for the research query below. Use 2-6 words in sentence case. Do not answer the query and do not use tools. Return JSON matching the provided schema.\n\n<research_query>\n{source}\n</research_query>"
    )
}

fn build_research_title_args(
    flavor: ResearchTitleFlavor,
    cwd: &Path,
    prompt: &str,
    model: Option<&str>,
    schema_file: Option<&Path>,
    grok_session_id: Option<&str>,
) -> Vec<String> {
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    match flavor {
        ResearchTitleFlavor::Codex => {
            let mut args = vec![
                "--disable".into(),
                "hooks".into(),
                "--ask-for-approval".into(),
                "never".into(),
                "exec".into(),
                "--json".into(),
                "--strict-config".into(),
                "--skip-git-repo-check".into(),
                "--ignore-user-config".into(),
                "--ignore-rules".into(),
                "--ephemeral".into(),
                "--sandbox".into(),
                "read-only".into(),
            ];
            if let Some(model) = model {
                args.extend(["--model".into(), model.into()]);
            }
            args.extend([
                "-c".into(),
                "model_reasoning_effort=\"low\"".into(),
                "--output-schema".into(),
                schema_file
                    .expect("Codex title generation requires a schema file")
                    .display()
                    .to_string(),
                "--".into(),
                prompt.into(),
            ]);
            args
        }
        ResearchTitleFlavor::Claude => {
            let mut args = vec![
                "-p".into(),
                "--output-format".into(),
                "json".into(),
                "--json-schema".into(),
                TITLE_SCHEMA.into(),
                "--no-session-persistence".into(),
                "--permission-mode".into(),
                "dontAsk".into(),
                "--setting-sources=".into(),
                "--strict-mcp-config".into(),
                "--no-chrome".into(),
                "--tools".into(),
                "".into(),
                "--effort".into(),
                "low".into(),
            ];
            if let Some(model) = model {
                args.extend(["--model".into(), model.into()]);
            }
            args.push(prompt.into());
            args
        }
        ResearchTitleFlavor::Grok => {
            let mut args = vec![
                "--no-auto-update".into(),
                "--cwd".into(),
                cwd.display().to_string(),
                "--output-format".into(),
                "json".into(),
                "--json-schema".into(),
                TITLE_SCHEMA.into(),
                "--permission-mode".into(),
                "dontAsk".into(),
                "--sandbox".into(),
                "read-only".into(),
                "--tools".into(),
                "".into(),
                "--no-subagents".into(),
                "--disable-web-search".into(),
                "--reasoning-effort".into(),
                "low".into(),
            ];
            if let Some(model) = model {
                args.extend(["--model".into(), model.into()]);
            }
            args.extend([
                "--session-id".into(),
                grok_session_id
                    .expect("Grok title generation requires a session id")
                    .into(),
                "-p".into(),
                prompt.into(),
            ]);
            args
        }
    }
}

fn run_research_title_process(
    binary: &str,
    args: &[String],
    cwd: &Path,
    stderr_log: &Path,
    flavor: ResearchTitleFlavor,
) -> Result<String, String> {
    let mut process = JsonlProcess::spawn(binary, args, cwd, stderr_log, flavor.label())?;
    let deadline = Instant::now() + RESEARCH_TITLE_TIMEOUT;
    let mut candidate = None;
    loop {
        if Instant::now() >= deadline {
            process.kill();
            return Err(format!("{} title generation timed out", flavor.label()));
        }
        match process.recv_timeout(Duration::from_millis(100))? {
            JsonlReceive::Timeout => continue,
            JsonlReceive::Eof => break,
            JsonlReceive::Value(value) => {
                if json_value_is_error(&value) {
                    process.kill();
                    return Err(format!(
                        "{} title generation failed: {}",
                        flavor.label(),
                        json_value_error(&value)
                    ));
                }
                if let Some(title) = title_candidate_from_value(&value) {
                    candidate = Some(title);
                }
            }
        }
    }
    let status = process.finish(Duration::from_secs(2))?;
    if !status.success() {
        return Err(format!(
            "{} title generation exited with status {status}",
            flavor.label()
        ));
    }
    sanitize_research_title(candidate.as_deref().unwrap_or(""))
        .ok_or_else(|| format!("{} returned no research title", flavor.label()))
}

fn title_candidate_from_value(value: &Value) -> Option<String> {
    if let Some(title) = value.get("title").and_then(Value::as_str) {
        return Some(title.to_string());
    }
    for key in ["structured_output", "structuredOutput", "output"] {
        if let Some(candidate) = value.get(key).and_then(title_candidate_from_value) {
            return Some(candidate);
        }
    }
    if value.get("type").and_then(Value::as_str) == Some("item.completed") {
        return value
            .get("item")
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("agent_message"))
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .and_then(title_candidate_from_text);
    }
    for key in ["result", "result_text", "text", "output_text"] {
        if let Some(candidate) = value
            .get(key)
            .and_then(Value::as_str)
            .and_then(title_candidate_from_text)
        {
            return Some(candidate);
        }
    }
    None
}

fn title_candidate_from_text(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .as_ref()
        .and_then(title_candidate_from_value)
        .or_else(|| (!text.trim().is_empty()).then(|| text.to_string()))
}

fn json_value_is_error(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("error" | "turn.failed")
    ) || value.get("is_error").and_then(Value::as_bool) == Some(true)
        || value.get("subtype").and_then(Value::as_str) == Some("error")
}

fn json_value_error(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("unknown model error")
        .to_string()
}

fn sanitize_research_title(raw: &str) -> Option<String> {
    let normalized = raw
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let without_label = normalized
        .strip_prefix("Title:")
        .or_else(|| normalized.strip_prefix("title:"))
        .unwrap_or(&normalized)
        .trim();
    let unquoted = without_label
        .trim_matches(|character| matches!(character, '"' | '\'' | '`'))
        .trim_end_matches('.')
        .trim();
    if unquoted.is_empty() {
        return None;
    }
    let chars = unquoted.chars().collect::<Vec<_>>();
    if chars.len() <= RESEARCH_TITLE_MAX_CHARS {
        return Some(unquoted.to_string());
    }
    Some(format!(
        "{}…",
        chars[..RESEARCH_TITLE_MAX_CHARS - 1]
            .iter()
            .collect::<String>()
            .trim_end()
    ))
}

struct TitleSchemaFile {
    path: PathBuf,
}

impl TitleSchemaFile {
    fn create(config: &QmuxConfig) -> Result<Self, String> {
        let id = new_uuid_v4()?;
        let directory = config.workspace_root.join(".qmux").join("tmp");
        std::fs::create_dir_all(&directory).map_err(|err| {
            format!(
                "failed to create title schema directory {}: {err}",
                directory.display()
            )
        })?;
        let path = directory.join(format!("research-title-{id}.schema.json"));
        std::fs::write(&path, TITLE_SCHEMA)
            .map_err(|err| format!("failed to write title output schema: {err}"))?;
        Ok(Self { path })
    }
}

impl Drop for TitleSchemaFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod research_title_tests {
    use super::*;

    #[test]
    fn title_args_use_lightweight_isolated_sessions() {
        let cwd = Path::new("/tmp/research");
        let schema = Path::new("/tmp/title.schema.json");
        let codex = build_research_title_args(
            ResearchTitleFlavor::Codex,
            cwd,
            "prompt",
            Some("gpt-test"),
            Some(schema),
            None,
        );
        assert!(codex.iter().any(|arg| arg == "--ephemeral"));
        assert!(codex.iter().any(|arg| arg == "gpt-test"));
        assert!(
            codex
                .iter()
                .any(|arg| arg == "model_reasoning_effort=\"low\"")
        );
        assert!(!codex.iter().any(|arg| arg == "--search"));

        let claude = build_research_title_args(
            ResearchTitleFlavor::Claude,
            cwd,
            "prompt",
            Some("claude-test"),
            None,
            None,
        );
        assert!(claude.iter().any(|arg| arg == "--no-session-persistence"));
        assert!(claude.windows(2).any(|pair| pair == ["--tools", ""]));
        assert!(claude.windows(2).any(|pair| pair == ["--effort", "low"]));

        let grok = build_research_title_args(
            ResearchTitleFlavor::Grok,
            cwd,
            "prompt",
            Some("grok-test"),
            None,
            Some("session-1"),
        );
        assert!(
            grok.windows(2)
                .any(|pair| pair == ["--session-id", "session-1"])
        );
        assert!(grok.windows(2).any(|pair| pair == ["--tools", ""]));
        assert!(grok.iter().any(|arg| arg == "--disable-web-search"));
    }

    #[test]
    fn title_output_parses_structured_and_jsonl_results() {
        assert_eq!(
            title_candidate_from_value(&serde_json::json!({
                "structured_output": { "title": "Research agents" }
            })),
            Some("Research agents".to_string())
        );
        assert_eq!(
            title_candidate_from_value(&serde_json::json!({
                "type": "item.completed",
                "item": { "type": "agent_message", "text": "{\"title\":\"Query titles\"}" }
            })),
            Some("Query titles".to_string())
        );
    }

    #[test]
    fn generated_titles_are_sanitized_and_bounded() {
        assert_eq!(
            sanitize_research_title("  Title: `Research   query titles.` "),
            Some("Research query titles".to_string())
        );
        assert_eq!(sanitize_research_title("\n\t"), None);
        let title = sanitize_research_title(&"x".repeat(100)).unwrap();
        assert_eq!(title.chars().count(), RESEARCH_TITLE_MAX_CHARS);
        assert!(title.ends_with('…'));
    }
}

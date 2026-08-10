use serde_json::{Value, json};

pub const SKILL: &str = include_str!("qmux_skill.md");

pub const HELP: &str = "usage: qmux <command> [options]\n\nControl commands:\n  context\n  workspace <list|get|create|rename>\n  pane <list|current|get|read|create|send|run|wait-output|rename|focus|close>\n  agent <list|get|read|start|fork|prompt|wait|focus|release>\n  artifact <list|open>\n  split <list|join|leave|resize>\n\nIntegrations:\n  mcp, open, fork, claude, codex, grok, muse, acp\n\nRun `qmux <group> --help` for group commands or `qmux --skill` for agent instructions.";

pub fn run(command: &str, args: Vec<String>) -> Result<bool, String> {
    if let Some(help) = group_help(command)
        && (args.is_empty() || args.as_slice() == ["--help"] || args.as_slice() == ["-h"])
    {
        println!("{help}");
        return Ok(true);
    }
    let Some((operation, arguments, human)) = parse(command, args).map_err(super::syntax_error)?
    else {
        return Ok(false);
    };
    let response = super::request_public(&operation, arguments)?;
    let encoded = if human {
        serde_json::to_string_pretty(&response)
    } else {
        serde_json::to_string(&response)
    }
    .map_err(|error| format!("failed to encode qmux CLI response: {error}"))?;
    if response.ok {
        println!("{encoded}");
        Ok(true)
    } else {
        Err(encoded)
    }
}

fn group_help(command: &str) -> Option<&'static str> {
    match command {
        "workspace" => Some(
            "usage: qmux workspace <command>\n\n  list\n  get <workspace-id>\n  create [--name <name>] [--dir <path>]\n  rename <workspace-id> <name>",
        ),
        "pane" => Some(
            "usage: qmux pane <command>\n\n  list\n  current\n  get <pane-id>\n  read <pane-id> [--source terminal|viewport] [--lines <count>]\n  create [--current-workspace] [--cwd <path>] [--no-focus]\n  send <pane-id> <text> [--submit]\n  run <pane-id> <command>\n  wait-output <pane-id> (--match <text>|--regex <pattern>) [--timeout <duration>]\n  rename <pane-id> <name>\n  focus <pane-id>\n  close <pane-id>",
        ),
        "agent" => Some(
            "usage: qmux agent <command>\n\n  list\n  get <agent-id>\n  read <agent-id> [--source transcript|terminal] [--turns <count>] [--lines <count>]\n  start [name] [--adapter <id>] [--prompt <text>] [--worktree] [--model <id>] [--effort <level>] [--cwd <path>] [--no-focus]\n  fork <agent-id> [--prompt <text>] [--worktree] [--no-focus]\n  prompt <agent-id> <text>\n  wait <agent-id> [--until <state>] [--timeout <duration>]\n  focus <agent-id>\n  release <agent-id>",
        ),
        "artifact" => Some("usage: qmux artifact <command>\n\n  list\n  open <artifact-id>"),
        "split" => Some(
            "usage: qmux split <command>\n\n  list\n  join <pane-id> <other-pane-id>\n  leave <pane-id>\n  resize <split-id> <pane-id> <fraction>",
        ),
        _ => None,
    }
}

fn parse(command: &str, mut args: Vec<String>) -> Result<Option<(String, Value, bool)>, String> {
    let human = take_flag(&mut args, "--human");
    let parsed = match command {
        "context" => no_args("context", args, "context")?,
        "workspace" => parse_workspace(args)?,
        "pane" => parse_pane(args)?,
        "agent" => parse_agent(args)?,
        "artifact" => parse_artifact(args)?,
        "split" => parse_split(args)?,
        _ => return Ok(None),
    };
    Ok(Some((parsed.0, parsed.1, human)))
}

fn parse_workspace(mut args: Vec<String>) -> Result<(String, Value), String> {
    match pop_subcommand("workspace", &mut args)?.as_str() {
        "list" => no_args("workspace list", args, "workspace.list"),
        "get" => one_id("workspace get", args, "workspace.get"),
        "create" => parse_workspace_create(args),
        "rename" => parse_rename("workspace", args),
        other => Err(format!("unknown workspace command '{other}'")),
    }
}

fn parse_pane(mut args: Vec<String>) -> Result<(String, Value), String> {
    match pop_subcommand("pane", &mut args)?.as_str() {
        "list" => no_args("pane list", args, "pane.list"),
        "current" => no_args("pane current", args, "pane.current"),
        "get" => one_id("pane get", args, "pane.get"),
        "read" => parse_read("pane", args),
        "create" => parse_pane_create(args),
        "send" => parse_pane_send(args, false),
        "run" => parse_pane_send(args, true),
        "wait-output" => parse_wait_output(args),
        "rename" => parse_rename("pane", args),
        "focus" => one_id("pane focus", args, "pane.focus"),
        "close" => one_id("pane close", args, "pane.close"),
        other => Err(format!("unknown pane command '{other}'")),
    }
}

fn parse_workspace_create(mut args: Vec<String>) -> Result<(String, Value), String> {
    let mut name = None;
    let mut dir = None;
    while !args.is_empty() {
        let flag = args.remove(0);
        let value = take_value(&mut args, &flag)?;
        match flag.as_str() {
            "--name" => name = Some(value),
            "--dir" => dir = Some(value),
            _ => return Err(format!("unknown workspace create option '{flag}'")),
        }
    }
    Ok((
        "workspace.create".into(),
        json!({ "name": name, "dir": dir }),
    ))
}

fn parse_rename(kind: &str, args: Vec<String>) -> Result<(String, Value), String> {
    if args.len() != 2 {
        return Err(format!("usage: qmux {kind} rename <id> <name>"));
    }
    Ok((
        format!("{kind}.rename"),
        json!({ "id": args[0], "name": args[1] }),
    ))
}

fn parse_pane_create(mut args: Vec<String>) -> Result<(String, Value), String> {
    let mut workspace_id = None;
    let mut cwd = None;
    while !args.is_empty() {
        let flag = args.remove(0);
        match flag.as_str() {
            "--current-workspace" | "--no-focus" => {}
            "--workspace" => workspace_id = Some(take_value(&mut args, &flag)?),
            "--cwd" => cwd = Some(take_value(&mut args, &flag)?),
            _ => return Err(format!("unknown pane create option '{flag}'")),
        }
    }
    Ok((
        "pane.create".into(),
        json!({ "workspaceId": workspace_id, "cwd": cwd }),
    ))
}

fn parse_pane_send(mut args: Vec<String>, run: bool) -> Result<(String, Value), String> {
    if args.len() < 2 {
        return Err(format!(
            "usage: qmux pane {} <pane-id> <text>",
            if run { "run" } else { "send" }
        ));
    }
    let id = args.remove(0);
    let submit = if run {
        true
    } else {
        take_flag(&mut args, "--submit")
    };
    strip_option_delimiter(&mut args);
    let text = args.join(" ");
    Ok((
        (if run { "pane.run" } else { "pane.send" }).into(),
        json!({ "id": id, "text": text, "submit": submit }),
    ))
}

fn parse_wait_output(mut args: Vec<String>) -> Result<(String, Value), String> {
    if args.is_empty() {
        return Err("usage: qmux pane wait-output <pane-id> --match <text>".into());
    }
    let id = args.remove(0);
    let mut text = None;
    let mut regex = None;
    let mut timeout_ms = 30_000;
    while !args.is_empty() {
        let flag = args.remove(0);
        let value = take_value(&mut args, &flag)?;
        match flag.as_str() {
            "--match" => text = Some(value),
            "--regex" => regex = Some(value),
            "--timeout" => timeout_ms = parse_duration_ms(&value)?,
            _ => return Err(format!("unknown pane wait-output option '{flag}'")),
        }
    }
    Ok((
        "pane.waitOutput".into(),
        json!({ "id": id, "text": text, "regex": regex, "timeoutMs": timeout_ms }),
    ))
}

fn parse_agent(mut args: Vec<String>) -> Result<(String, Value), String> {
    match pop_subcommand("agent", &mut args)?.as_str() {
        "list" => no_args("agent list", args, "agent.list"),
        "get" => one_id("agent get", args, "agent.get"),
        "read" => parse_read("agent", args),
        "start" => parse_agent_start(args),
        "fork" => parse_agent_fork(args),
        "prompt" => parse_agent_prompt(args),
        "wait" => parse_agent_wait(args),
        "focus" => one_id("agent focus", args, "agent.focus"),
        "release" => one_id("agent release", args, "agent.release"),
        other => Err(format!("unknown agent command '{other}'")),
    }
}

fn parse_agent_start(mut args: Vec<String>) -> Result<(String, Value), String> {
    let name = args.first().filter(|arg| !arg.starts_with('-')).cloned();
    if name.is_some() {
        args.remove(0);
    }
    let mut adapter = None;
    let mut prompt = None;
    let mut use_worktree = false;
    let mut model = None;
    let mut effort = None;
    let mut cwd = None;
    while !args.is_empty() {
        let flag = args.remove(0);
        match flag.as_str() {
            "--worktree" | "-w" => use_worktree = true,
            "--no-focus" | "--current-workspace" => {}
            "--adapter" => adapter = Some(take_value(&mut args, &flag)?),
            "--prompt" => prompt = Some(take_value(&mut args, &flag)?),
            "--model" => model = Some(take_value(&mut args, &flag)?),
            "--effort" => effort = Some(take_value(&mut args, &flag)?),
            "--cwd" => cwd = Some(take_value(&mut args, &flag)?),
            _ => return Err(format!("unknown agent start option '{flag}'")),
        }
    }
    Ok((
        "agent.start".into(),
        json!({
            "name": name,
            "adapter": adapter,
            "prompt": prompt,
            "useWorktree": use_worktree,
            "model": model,
            "effort": effort,
            "cwd": cwd
        }),
    ))
}

fn parse_agent_fork(mut args: Vec<String>) -> Result<(String, Value), String> {
    if args.is_empty() {
        return Err("usage: qmux agent fork <agent-id> [--prompt <prompt>] [--worktree]".into());
    }
    let id = args.remove(0);
    let mut prompt = None;
    let mut use_worktree = false;
    while !args.is_empty() {
        let flag = args.remove(0);
        match flag.as_str() {
            "--worktree" | "-w" => use_worktree = true,
            "--no-focus" => {}
            "--prompt" => prompt = Some(take_value(&mut args, &flag)?),
            _ => return Err(format!("unknown agent fork option '{flag}'")),
        }
    }
    Ok((
        "agent.fork".into(),
        json!({ "id": id, "prompt": prompt, "useWorktree": use_worktree }),
    ))
}

fn parse_agent_prompt(mut args: Vec<String>) -> Result<(String, Value), String> {
    if args.len() < 2 {
        return Err("usage: qmux agent prompt <agent-id> <text>".into());
    }
    let id = args.remove(0);
    strip_option_delimiter(&mut args);
    Ok((
        "agent.prompt".into(),
        json!({ "id": id, "text": args.join(" ") }),
    ))
}

fn parse_agent_wait(mut args: Vec<String>) -> Result<(String, Value), String> {
    if args.is_empty() {
        return Err(
            "usage: qmux agent wait <agent-id> [--until <state>] [--timeout <duration>]".into(),
        );
    }
    let id = args.remove(0);
    let mut until = "settled".to_string();
    let mut timeout_ms = 30_000;
    while !args.is_empty() {
        let flag = args.remove(0);
        let value = take_value(&mut args, &flag)?;
        match flag.as_str() {
            "--until" => until = value,
            "--timeout" => timeout_ms = parse_duration_ms(&value)?,
            _ => return Err(format!("unknown agent wait option '{flag}'")),
        }
    }
    Ok((
        "agent.wait".into(),
        json!({ "id": id, "until": until, "timeoutMs": timeout_ms }),
    ))
}

fn parse_artifact(mut args: Vec<String>) -> Result<(String, Value), String> {
    match pop_subcommand("artifact", &mut args)?.as_str() {
        "list" => no_args("artifact list", args, "artifact.list"),
        "open" => one_id("artifact open", args, "artifact.open"),
        other => Err(format!("unknown artifact command '{other}'")),
    }
}

fn parse_split(mut args: Vec<String>) -> Result<(String, Value), String> {
    match pop_subcommand("split", &mut args)?.as_str() {
        "list" => no_args("split list", args, "split.list"),
        "join" => {
            if args.len() != 2 {
                return Err("usage: qmux split join <pane-id> <other-pane-id>".into());
            }
            Ok((
                "split.join".into(),
                json!({ "id": args[0], "other": args[1] }),
            ))
        }
        "leave" => one_id("split leave", args, "split.leave"),
        "resize" => {
            if args.len() != 3 {
                return Err("usage: qmux split resize <split-id> <pane-id> <fraction>".into());
            }
            let fraction = args[2]
                .parse::<f64>()
                .map_err(|_| "split fraction must be a number".to_string())?;
            if !fraction.is_finite() {
                return Err("split fraction must be finite".into());
            }
            Ok((
                "split.resize".into(),
                json!({ "id": args[0], "pane": args[1], "fraction": fraction }),
            ))
        }
        other => Err(format!("unknown split command '{other}'")),
    }
}

fn parse_read(kind: &str, mut args: Vec<String>) -> Result<(String, Value), String> {
    if args.is_empty() {
        return Err(format!("usage: qmux {kind} read <id> [--source <source>]"));
    }
    let id = args.remove(0);
    let mut source = None;
    let mut lines = None;
    let mut turns = None;
    while !args.is_empty() {
        let flag = args.remove(0);
        let value = args
            .first()
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        args.remove(0);
        match flag.as_str() {
            "--source" => source = Some(value),
            "--lines" => lines = Some(parse_usize(&value, "--lines")?),
            "--turns" if kind == "agent" => turns = Some(parse_usize(&value, "--turns")?),
            _ => return Err(format!("unknown {kind} read option '{flag}'")),
        }
    }
    Ok((
        format!("{kind}.read"),
        json!({ "id": id, "source": source, "lines": lines, "turns": turns }),
    ))
}

fn pop_subcommand(group: &str, args: &mut Vec<String>) -> Result<String, String> {
    if args.is_empty() {
        return Err(format!("usage: qmux {group} <command>"));
    }
    Ok(args.remove(0))
}

fn no_args(label: &str, args: Vec<String>, operation: &str) -> Result<(String, Value), String> {
    if !args.is_empty() {
        return Err(format!("usage: qmux {label}"));
    }
    Ok((operation.to_string(), json!({})))
}

fn one_id(label: &str, args: Vec<String>, operation: &str) -> Result<(String, Value), String> {
    if args.len() != 1 {
        return Err(format!("usage: qmux {label} <id>"));
    }
    Ok((operation.to_string(), json!({ "id": args[0] })))
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let option_end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let found = args[..option_end].iter().position(|arg| arg == flag);
    if let Some(index) = found {
        args.remove(index);
        true
    } else {
        false
    }
}

fn strip_option_delimiter(args: &mut Vec<String>) {
    if let Some(index) = args.iter().position(|arg| arg == "--") {
        args.remove(index);
    }
}

fn parse_usize(value: &str, flag: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{flag} requires a positive integer"))?;
    if parsed == 0 {
        Err(format!("{flag} requires a positive integer"))
    } else {
        Ok(parsed)
    }
}

fn take_value(args: &mut Vec<String>, flag: &str) -> Result<String, String> {
    if args.is_empty() {
        Err(format!("{flag} requires a value"))
    } else {
        Ok(args.remove(0))
    }
}

fn parse_duration_ms(value: &str) -> Result<u64, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else {
        (value, 1_000)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
        .ok_or_else(|| format!("invalid duration '{value}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_current_follows_pane_list_in_the_surface_and_parses_without_a_target() {
        assert_eq!(
            parse("pane", vec!["list".into()]).unwrap().unwrap().0,
            "pane.list"
        );
        assert_eq!(
            parse("pane", vec!["current".into()]).unwrap().unwrap().0,
            "pane.current"
        );
    }

    #[test]
    fn agent_read_parses_bounded_source_options() {
        let parsed = parse(
            "agent",
            vec![
                "read".into(),
                "agent-1".into(),
                "--source".into(),
                "transcript".into(),
                "--turns".into(),
                "3".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.0, "agent.read");
        assert_eq!(parsed.1["turns"], 3);
    }

    #[test]
    fn pane_wait_output_parses_regex_and_duration() {
        let parsed = parse(
            "pane",
            vec![
                "wait-output".into(),
                "pane-1".into(),
                "--regex".into(),
                "tests? passed".into(),
                "--timeout".into(),
                "2m".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.0, "pane.waitOutput");
        assert_eq!(parsed.1["timeoutMs"], 120_000);
    }

    #[test]
    fn text_commands_preserve_flag_like_text_after_the_option_delimiter() {
        let pane = parse(
            "pane",
            vec![
                "send".into(),
                "pane-1".into(),
                "--".into(),
                "--human".into(),
                "--submit".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert!(!pane.2);
        assert_eq!(pane.1["text"], "--human --submit");
        assert_eq!(pane.1["submit"], false);

        let agent = parse(
            "agent",
            vec![
                "prompt".into(),
                "agent-1".into(),
                "--".into(),
                "--human".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert!(!agent.2);
        assert_eq!(agent.1["text"], "--human");
    }

    #[test]
    fn agent_start_parses_name_and_launch_options() {
        let parsed = parse(
            "agent",
            vec![
                "start".into(),
                "reviewer".into(),
                "--adapter".into(),
                "codex".into(),
                "--prompt".into(),
                "review this".into(),
                "--worktree".into(),
                "--effort".into(),
                "high".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.0, "agent.start");
        assert_eq!(parsed.1["name"], "reviewer");
        assert_eq!(parsed.1["adapter"], "codex");
        assert_eq!(parsed.1["useWorktree"], true);
        assert_eq!(parsed.1["effort"], "high");
    }

    #[test]
    fn agent_wait_parses_state_and_duration() {
        let parsed = parse(
            "agent",
            vec![
                "wait".into(),
                "agent-1".into(),
                "--until".into(),
                "permission".into(),
                "--timeout".into(),
                "45s".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.0, "agent.wait");
        assert_eq!(parsed.1["until"], "permission");
        assert_eq!(parsed.1["timeoutMs"], 45_000);
    }

    #[test]
    fn split_resize_parses_an_absolute_fraction() {
        let parsed = parse(
            "split",
            vec![
                "resize".into(),
                "split-1".into(),
                "pane-1".into(),
                "0.6".into(),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.0, "split.resize");
        assert_eq!(parsed.1["fraction"], 0.6);
    }

    #[test]
    fn built_in_skill_has_frontmatter_environment_gate_and_core_workflows() {
        assert!(SKILL.starts_with("---\nname: qmux\n"));
        assert!(SKILL.contains("test \"${QMUX_ENV:-}\" = 1"));
        assert!(SKILL.contains("qmux pane wait-output"));
        assert!(SKILL.contains("qmux agent wait"));
        assert!(SKILL.contains("qmux split join"));
    }

    #[test]
    fn help_keeps_pane_current_after_pane_list() {
        let help = group_help("pane").unwrap();
        assert!(help.find("  list").unwrap() < help.find("  current").unwrap());
    }
}

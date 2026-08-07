//! Where an agent's work actually runs.
//!
//! Everything in qmux has always assumed the agent, its git worktree, and the
//! control socket share one machine. This module is the seam that stops
//! assuming it: a [`Host`] is either the local machine (byte-for-byte today's
//! behaviour) or an ssh destination, and every command that has to run *where
//! the code lives* goes through it.
//!
//! The reason this is a seam rather than an ssh call bolted onto the ACP
//! adapter is that `prepare_agent_workspace` runs for *every* adapter. Teaching
//! the workspace layer where it is buys remote panes for Claude and Codex too,
//! not just ACP.
//!
//! ## Quoting is the whole problem
//!
//! `ssh host git worktree add -- <path> <ref>` does not pass an argv. ssh joins
//! everything after the destination with spaces and hands the string to a shell
//! on the far side, which re-splits it. qmux already defends the worktree path
//! and base ref against option-injection with `--` and `--end-of-options`; a
//! second shell would undo that by splitting on whitespace an attacker
//! controls. So every argument is single-quoted here before it is joined, and
//! the tests below are mostly about that.

use crate::adapters::shell_quote_arg;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Command;

/// Refuse to sit on a dead connection: a worktree call is on the path to
/// opening a pane, so failing fast beats hanging the launch.
const CONNECT_TIMEOUT_SECONDS: u32 = 10;

/// A machine qmux can run agents on, as declared in `qmux.config.json`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    /// The ssh destination, e.g. `devbox` or `user@10.0.0.4`. Passed to `ssh`
    /// as-is so `~/.ssh/config` aliases work.
    pub ssh: String,
    /// How to invoke the qmux CLI on that machine. It services hooks and, later,
    /// runs the ACP bridge.
    #[serde(default = "default_remote_cli")]
    pub qmux_cli: String,
    /// Where agent worktrees live on that machine. Falls back to the remote
    /// user's home when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// Extra `ssh` flags, for hosts needing a jump box, identity file, or port.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_options: Vec<String>,
}

fn default_remote_cli() -> String {
    "qmux-cli".to_string()
}

/// Resolved execution target. `Local` is the default everywhere and behaves
/// exactly as qmux did before this module existed.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Host {
    #[default]
    Local,
    Remote {
        name: String,
        config: HostConfig,
    },
}

/// How a remote command should be run: interactive commands get a tty and may
/// prompt, background ones must never block waiting for a human.
///
/// Only `Batch` has a caller today — the worktree git calls. `Interactive` is
/// here because the distinction is a correctness one worth pinning down with a
/// test now: the pane spawn that lands next must *not* run in batch mode, or
/// ssh will fail outright instead of letting the user answer a passphrase.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Interaction {
    /// A pane's foreground process: allocate a remote tty, allow ssh to prompt
    /// for a passphrase.
    Interactive,
    /// A backend call (git, probes). `BatchMode=yes` turns a would-be password
    /// prompt into an immediate failure rather than a hung launch.
    Batch,
}

/// A local socket to expose on the remote side, as `ssh -R`.
///
/// Unused until the pane spawn lands; it is how a remote agent's lifecycle
/// hooks reach the local control socket without the pane token ever becoming a
/// network credential.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct SocketForward {
    pub remote_path: String,
    pub local_path: String,
}

/// Everything needed to build one remote invocation.
#[derive(Clone, Debug, Default)]
pub struct RemoteCommand<'a> {
    pub program: &'a str,
    pub args: Vec<String>,
    /// Prefixed onto the remote command as `env K=V …`. ssh will not forward
    /// arbitrary variables — `SendEnv` needs cooperation from the server's
    /// sshd config — so setting them explicitly is the only portable way.
    pub envs: Vec<(String, String)>,
    pub forwards: Vec<SocketForward>,
}

impl Host {
    #[allow(dead_code)]
    pub fn is_local(&self) -> bool {
        matches!(self, Host::Local)
    }

    /// The host's name for error messages; `"local"` for the local machine.
    pub fn label(&self) -> &str {
        match self {
            Host::Local => "local",
            Host::Remote { name, .. } => name,
        }
    }

    #[allow(dead_code)]
    pub fn config(&self) -> Option<&HostConfig> {
        match self {
            Host::Local => None,
            Host::Remote { config, .. } => Some(config),
        }
    }

    /// Builds a `git` invocation that runs on this host.
    pub fn git<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string())
            .collect();
        self.command(RemoteCommand {
            program: "git",
            args,
            ..Default::default()
        })
    }

    /// Builds a runnable [`Command`]. Local hosts get the program directly;
    /// remote hosts get `ssh`, in batch mode.
    pub fn command(&self, remote: RemoteCommand<'_>) -> Command {
        match self {
            Host::Local => {
                let mut command = Command::new(remote.program);
                command.args(&remote.args);
                for (key, value) in &remote.envs {
                    command.env(key, value);
                }
                command
            }
            Host::Remote { .. } => {
                let argv = self
                    .ssh_argv(&remote, Interaction::Batch)
                    .expect("a remote host always yields an ssh argv");
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
        }
    }

    /// The full `ssh …` argv for `remote`, or `None` on a local host.
    ///
    /// Exposed separately from [`Host::command`] because a pane is spawned
    /// through qmux's pty layer, which wants a program and args rather than a
    /// built `Command`.
    pub fn ssh_argv(
        &self,
        remote: &RemoteCommand<'_>,
        interaction: Interaction,
    ) -> Option<Vec<String>> {
        let Host::Remote { config, .. } = self else {
            return None;
        };
        let mut argv = vec!["ssh".to_string()];

        if interaction == Interaction::Batch {
            argv.push("-o".to_string());
            argv.push("BatchMode=yes".to_string());
        } else {
            // Without a remote tty the agent's process sees a pipe: no job
            // control, no window size, and anything checking `isatty` takes its
            // non-interactive branch.
            argv.push("-t".to_string());
        }
        argv.push("-o".to_string());
        argv.push(format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"));

        if !remote.forwards.is_empty() {
            // A unix-socket forward fails outright if the remote path already
            // exists, which it will after any unclean disconnect. Let ssh
            // remove it rather than stranding the host until someone cleans up
            // by hand.
            argv.push("-o".to_string());
            argv.push("StreamLocalBindUnlink=yes".to_string());
        }
        for forward in &remote.forwards {
            argv.push("-R".to_string());
            argv.push(format!("{}:{}", forward.remote_path, forward.local_path));
        }
        argv.extend(config.ssh_options.iter().cloned());

        // Ends ssh's own option parsing, so a destination that begins with "-"
        // is treated as a destination.
        argv.push("--".to_string());
        argv.push(config.ssh.clone());
        argv.push(remote_command_line(remote));
        Some(argv)
    }

    /// Resolves a path on this host. Remote paths are returned verbatim: they
    /// are the far side's, and the local filesystem has no opinion about them.
    #[allow(dead_code)]
    pub fn workspace_root(&self, local_default: &str) -> String {
        match self {
            Host::Local => local_default.to_string(),
            Host::Remote { config, .. } => config
                .workspace_root
                .clone()
                .unwrap_or_else(|| "~/.qmux/workspaces".to_string()),
        }
    }
}

/// The single shell-safe string ssh will hand to the remote shell.
///
/// Every token is quoted, including the program: a command line is re-split by
/// that shell, so anything left unquoted is an injection point.
fn remote_command_line(remote: &RemoteCommand<'_>) -> String {
    let mut parts = Vec::new();
    if !remote.envs.is_empty() {
        parts.push("env".to_string());
        for (key, value) in &remote.envs {
            // The name is not quoted — `env` requires a literal NAME=VALUE — so
            // reject anything that isn't a plain identifier rather than letting
            // it through into the command line.
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                continue;
            }
            parts.push(format!("{key}={}", shell_quote_arg(value)));
        }
    }
    parts.push(shell_quote_arg(remote.program));
    parts.extend(remote.args.iter().map(|arg| shell_quote_arg(arg)));
    parts.join(" ")
}

/// The declared hosts, keyed by the short name a launch refers to.
pub type HostConfigs = BTreeMap<String, HostConfig>;

/// Resolves a launch's requested host name.
///
/// `None` and an empty name both mean local, so every existing caller and every
/// stored agent keeps working without a migration.
pub fn resolve(hosts: &HostConfigs, requested: Option<&str>) -> Result<Host, String> {
    let Some(name) = requested.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(Host::Local);
    };
    if name == "local" {
        return Ok(Host::Local);
    }
    let config = hosts.get(name).cloned().ok_or_else(|| {
        if hosts.is_empty() {
            format!("unknown host '{name}'; no hosts are configured in qmux.config.json")
        } else {
            format!(
                "unknown host '{name}'; configured hosts: {}",
                hosts.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        }
    })?;
    if config.ssh.trim().is_empty() {
        return Err(format!("host '{name}' has no ssh destination"));
    }
    Ok(Host::Remote {
        name: name.to_string(),
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_host() -> Host {
        Host::Remote {
            name: "devbox".to_string(),
            config: HostConfig {
                ssh: "user@devbox".to_string(),
                qmux_cli: "qmux-cli".to_string(),
                workspace_root: Some("/srv/work".to_string()),
                ssh_options: vec!["-p".to_string(), "2222".to_string()],
            },
        }
    }

    fn argv(host: &Host, remote: RemoteCommand<'_>, interaction: Interaction) -> Vec<String> {
        host.ssh_argv(&remote, interaction).expect("remote host")
    }

    #[test]
    fn a_local_host_runs_the_program_directly() {
        assert!(Host::Local.is_local());
        assert_eq!(Host::Local.label(), "local");
        assert!(
            Host::Local
                .ssh_argv(&RemoteCommand::default(), Interaction::Batch)
                .is_none()
        );

        let command = Host::Local.git(["status"]);
        assert_eq!(command.get_program(), "git");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["status"]);
    }

    #[test]
    fn a_remote_git_call_is_wrapped_in_ssh_in_batch_mode() {
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "git",
                args: vec!["status".to_string()],
                ..Default::default()
            },
            Interaction::Batch,
        );

        assert_eq!(argv[0], "ssh");
        // Batch mode is what turns a password prompt into a fast failure rather
        // than a pane that hangs forever on launch.
        assert!(argv.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert!(argv.iter().any(|arg| arg == "ConnectTimeout=10"));
        assert!(!argv.contains(&"-t".to_string()), "batch needs no tty");
        assert!(
            !argv.iter().any(|arg| arg == "StreamLocalBindUnlink=yes"),
            "only a forwarding session should unlink sockets"
        );
        // Configured options survive, and `--` guards the destination.
        assert!(argv.windows(2).any(|pair| pair == ["-p", "2222"]));
        let end = argv.iter().position(|arg| arg == "--").expect("-- present");
        assert_eq!(argv[end + 1], "user@devbox");
        assert_eq!(argv[end + 2], "'git' 'status'");
        assert_eq!(argv.len(), end + 3, "the command line is one argument");
    }

    #[test]
    fn an_interactive_remote_command_asks_for_a_tty_and_may_prompt() {
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "qmux-cli",
                args: vec!["acp".to_string()],
                ..Default::default()
            },
            Interaction::Interactive,
        );
        assert!(argv.contains(&"-t".to_string()));
        assert!(
            !argv.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]),
            "a pane may legitimately prompt for a passphrase"
        );
    }

    #[test]
    fn every_argument_is_quoted_so_the_remote_shell_cannot_resplit_it() {
        // This is the security property of the module. A worktree path or ref
        // is user-influenced, and the remote shell would otherwise word-split
        // it — undoing the `--`/`--end-of-options` guards git relies on.
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "git",
                args: vec![
                    "worktree".to_string(),
                    "add".to_string(),
                    "--".to_string(),
                    "/tmp/a b; rm -rf ~".to_string(),
                    "--detach".to_string(),
                ],
                ..Default::default()
            },
            Interaction::Batch,
        );
        let line = argv.last().expect("command line");
        assert_eq!(
            line,
            "'git' 'worktree' 'add' '--' '/tmp/a b; rm -rf ~' '--detach'"
        );
        // The dangerous characters are inside quotes, so the remote shell sees
        // one argument, not a second command.
        assert!(!line.contains("; rm -rf ~'") || line.contains("'/tmp/a b; rm -rf ~'"));
    }

    #[test]
    fn an_embedded_single_quote_cannot_escape_its_quoting() {
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "git",
                args: vec!["it's; touch /tmp/pwned".to_string()],
                ..Default::default()
            },
            Interaction::Batch,
        );
        // The classic escape: close the quote, inject, reopen. The `'\''`
        // sequence keeps it a single argument.
        assert_eq!(argv.last().unwrap(), r"'git' 'it'\''s; touch /tmp/pwned'");
    }

    #[test]
    fn environment_is_set_through_env_since_ssh_will_not_forward_it() {
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "qmux-cli",
                args: vec!["acp".to_string()],
                envs: vec![
                    ("QMUX_TOKEN".to_string(), "tok'en".to_string()),
                    ("QMUX_ACP_CWD".to_string(), "/a b".to_string()),
                ],
                ..Default::default()
            },
            Interaction::Batch,
        );
        assert_eq!(
            argv.last().unwrap(),
            r"env QMUX_TOKEN='tok'\''en' QMUX_ACP_CWD='/a b' 'qmux-cli' 'acp'"
        );
    }

    #[test]
    fn a_variable_name_that_is_not_an_identifier_is_dropped() {
        // `env` needs a literal NAME=VALUE, so the name cannot be quoted. Rather
        // than emit an unquoted attacker-influenced token, drop it.
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "true",
                args: Vec::new(),
                envs: vec![
                    ("GOOD_1".to_string(), "x".to_string()),
                    ("bad name; rm -rf ~".to_string(), "y".to_string()),
                    (String::new(), "z".to_string()),
                ],
                ..Default::default()
            },
            Interaction::Batch,
        );
        let line = argv.last().unwrap();
        assert_eq!(line, "env GOOD_1='x' 'true'");
        assert!(!line.contains("rm -rf"));
    }

    #[test]
    fn socket_forwards_become_ssh_reverse_forwards() {
        // How a remote agent's hooks reach the local control socket without the
        // pane token ever becoming a network credential.
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "qmux-cli",
                args: vec!["acp".to_string()],
                forwards: vec![SocketForward {
                    remote_path: "/tmp/qmux-remote.sock".to_string(),
                    local_path: "/run/qmux.sock".to_string(),
                }],
                ..Default::default()
            },
            Interaction::Interactive,
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-R", "/tmp/qmux-remote.sock:/run/qmux.sock"]),
            "{argv:?}"
        );
        // Without this a reconnect after an unclean disconnect fails, because
        // the socket the last session left behind is still on the remote.
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "StreamLocalBindUnlink=yes"]),
            "{argv:?}"
        );
    }

    #[test]
    fn an_absent_or_local_host_name_resolves_to_the_local_machine() {
        let hosts = HostConfigs::new();
        for requested in [None, Some(""), Some("   "), Some("local")] {
            assert_eq!(resolve(&hosts, requested).unwrap(), Host::Local);
        }
    }

    #[test]
    fn a_configured_host_resolves_and_an_unknown_one_lists_the_options() {
        let hosts = HostConfigs::from([
            (
                "devbox".to_string(),
                HostConfig {
                    ssh: "user@devbox".to_string(),
                    qmux_cli: "qmux-cli".to_string(),
                    ..Default::default()
                },
            ),
            (
                "builder".to_string(),
                HostConfig {
                    ssh: "builder".to_string(),
                    ..Default::default()
                },
            ),
        ]);

        let host = resolve(&hosts, Some("devbox")).expect("configured");
        assert_eq!(host.label(), "devbox");
        assert!(!host.is_local());

        let err = resolve(&hosts, Some("nope")).expect_err("unknown");
        assert!(err.contains("builder") && err.contains("devbox"), "{err}");
    }

    #[test]
    fn a_host_without_an_ssh_destination_is_rejected_before_it_is_used() {
        let hosts = HostConfigs::from([("broken".to_string(), HostConfig::default())]);
        let err = resolve(&hosts, Some("broken")).expect_err("no destination");
        assert!(err.contains("no ssh destination"), "{err}");
    }

    #[test]
    fn an_unknown_host_says_so_even_with_nothing_configured() {
        let err = resolve(&HostConfigs::new(), Some("devbox")).expect_err("unknown");
        assert!(err.contains("no hosts are configured"), "{err}");
    }

    #[test]
    fn host_config_defaults_the_remote_cli_but_keeps_an_override() {
        let parsed: HostConfig =
            serde_json::from_str(r#"{"ssh":"devbox"}"#).expect("minimal config parses");
        assert_eq!(parsed.qmux_cli, "qmux-cli");
        assert_eq!(parsed.workspace_root, None);

        let parsed: HostConfig = serde_json::from_str(
            r#"{"ssh":"devbox","qmuxCli":"/opt/qmux-cli","workspaceRoot":"/srv"}"#,
        )
        .expect("full config parses");
        assert_eq!(parsed.qmux_cli, "/opt/qmux-cli");
        assert_eq!(parsed.workspace_root.as_deref(), Some("/srv"));
    }

    #[test]
    fn the_workspace_root_follows_the_host() {
        assert_eq!(Host::Local.workspace_root("/local/root"), "/local/root");
        assert_eq!(remote_host().workspace_root("/local/root"), "/srv/work");

        let bare = Host::Remote {
            name: "b".to_string(),
            config: HostConfig {
                ssh: "b".to_string(),
                ..Default::default()
            },
        };
        assert_eq!(bare.workspace_root("/local/root"), "~/.qmux/workspaces");
    }
}

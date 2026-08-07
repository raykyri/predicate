//! Where an agent's work actually runs.
//!
//! Everything in qmux has always assumed the agent, its git worktree, and the
//! control socket share one machine. This module is the seam that stops
//! assuming it: a [`Host`] is either the local machine (byte-for-byte today's
//! behaviour) or an ssh destination, and every command that has to run *where
//! the code lives* goes through it.
//!
//! A host is derived from the group's [`RemoteRef`], never declared separately.
//! Remoteness is a property of the workspace — the directory, its repository,
//! and every pane opened against it are all on one machine — so binding it to
//! the group is what keeps an agent from ending up on a different machine from
//! the code it is editing.
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
use crate::workspace::{RemoteMultiplexer, RemoteRef};
use std::process::Command;

/// Refuse to sit on a dead connection: a worktree call is on the path to
/// opening a pane, so failing fast beats hanging the launch.
const CONNECT_TIMEOUT_SECONDS: u32 = 10;
/// Where worktrees land on a host that does not name a `workspaceRoot`.
const DEFAULT_REMOTE_WORKSPACE_ROOT: &str = "~/.qmux/workspaces";
/// How the qmux CLI is invoked on a host that does not name one.
const DEFAULT_REMOTE_CLI: &str = "qmux-cli";

/// A remote host, flattened out of the group's [`RemoteRef`] into the shape the
/// transport actually needs.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteTarget {
    /// Display name, for error messages.
    pub label: String,
    /// Connection target: an ssh-config alias or `user@host`. Auth and address
    /// resolution belong to the system `ssh` client, never to qmux.
    pub ssh: String,
    /// How to invoke the qmux CLI over there. It services hooks and runs the
    /// ACP bridge.
    pub qmux_cli: String,
    /// Where agent worktrees live on that machine.
    pub workspace_root: Option<String>,
    /// Chooses how a pane survives a dropped connection.
    pub multiplexer: RemoteMultiplexer,
}

/// Resolved execution target. `Local` is the default everywhere and behaves
/// exactly as qmux did before this module existed.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Host {
    #[default]
    Local,
    Remote(RemoteTarget),
}

/// Derives the host from a group's remote binding.
///
/// `None` — a group with no remote — is the local machine, which is every group
/// qmux has ever created, so nothing needs migrating.
pub fn for_group(remote: Option<&RemoteRef>) -> Host {
    match remote {
        None => Host::Local,
        Some(remote) => Host::Remote(RemoteTarget {
            label: remote.label.clone(),
            ssh: remote.host.clone(),
            qmux_cli: remote
                .qmux_cli
                .clone()
                .unwrap_or_else(|| DEFAULT_REMOTE_CLI.to_string()),
            workspace_root: remote.workspace_root.clone(),
            multiplexer: remote.multiplexer,
        }),
    }
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
            Host::Remote(target) => &target.label,
        }
    }

    pub fn remote(&self) -> Option<&RemoteTarget> {
        match self {
            Host::Local => None,
            Host::Remote(target) => Some(target),
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
            Host::Remote(_) => {
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
        let Host::Remote(target) = self else {
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

        // Ends ssh's own option parsing, so a destination that begins with "-"
        // is treated as a destination.
        argv.push("--".to_string());
        argv.push(target.ssh.clone());
        argv.push(remote_command_line(remote));
        Some(argv)
    }

    /// Where agent worktrees live on a remote host, or `None` locally.
    ///
    /// `None` is the signal that the caller's own local placement logic
    /// applies. A remote path must never be run through it — canonicalizing or
    /// stat'ing the far side's path here resolves against *this* filesystem and
    /// silently produces a local directory.
    pub fn remote_workspace_root(&self) -> Option<String> {
        let target = self.remote()?;
        Some(
            target
                .workspace_root
                .clone()
                .unwrap_or_else(|| DEFAULT_REMOTE_WORKSPACE_ROOT.to_string()),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_ref(workspace_root: Option<&str>, qmux_cli: Option<&str>) -> RemoteRef {
        RemoteRef {
            id: "saved-1".to_string(),
            label: "devbox".to_string(),
            host: "user@devbox".to_string(),
            multiplexer: RemoteMultiplexer::Tmux,
            qmux_cli: qmux_cli.map(str::to_string),
            workspace_root: workspace_root.map(str::to_string),
        }
    }

    fn remote_host() -> Host {
        for_group(Some(&remote_ref(Some("/srv/work"), None)))
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
        // `--` guards a destination that begins with "-".
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
    fn a_group_without_a_remote_is_the_local_machine() {
        // Every group qmux has ever created, so this is what keeps the change
        // from needing a migration.
        assert_eq!(for_group(None), Host::Local);
        assert!(Host::Local.remote().is_none());
    }

    #[test]
    fn a_groups_remote_becomes_its_host() {
        let host = for_group(Some(&remote_ref(Some("/srv/work"), Some("/opt/qmux-cli"))));
        let target = host.remote().expect("remote");

        assert_eq!(host.label(), "devbox");
        assert_eq!(target.ssh, "user@devbox");
        assert_eq!(target.qmux_cli, "/opt/qmux-cli");
        assert_eq!(target.multiplexer, RemoteMultiplexer::Tmux);
    }

    #[test]
    fn a_remote_that_names_no_cli_gets_the_default() {
        let host = for_group(Some(&remote_ref(None, None)));
        assert_eq!(host.remote().unwrap().qmux_cli, DEFAULT_REMOTE_CLI);
    }

    #[test]
    fn only_a_remote_host_overrides_where_worktrees_live() {
        // `None` locally is what keeps the existing placement logic — global
        // vs project-local — in charge on this machine.
        assert_eq!(Host::Local.remote_workspace_root(), None);
        assert_eq!(
            remote_host().remote_workspace_root().as_deref(),
            Some("/srv/work")
        );

        let bare = for_group(Some(&remote_ref(None, None)));
        assert_eq!(
            bare.remote_workspace_root().as_deref(),
            Some(DEFAULT_REMOTE_WORKSPACE_ROOT)
        );
    }
}

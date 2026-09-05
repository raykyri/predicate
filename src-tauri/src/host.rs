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
//! This is a shared seam because `prepare_agent_workspace` runs for every
//! adapter. Teaching the workspace layer where it is keeps remote execution
//! concerns out of individual adapters.
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
use crate::state::RemoteSessionIdentity;
use crate::workspace::{RemoteMultiplexer, RemoteRef};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

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
    /// Stable id snapshotted into both the group and every remote pane.
    pub id: String,
    /// Display name, for error messages.
    pub label: String,
    /// Connection target: an ssh-config alias or `user@host`. Auth and address
    /// resolution belong to the system `ssh` client, never to qmux.
    pub ssh: String,
    /// How to invoke the qmux CLI over there. It services hooks for remote panes.
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
            id: remote.id.clone(),
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

/// Fully quoted SSH invocations for one durable qmux tmux session.
///
/// Creation and attachment are deliberately separate. Only `create_argv` may
/// create the named session; every recovery path uses `attach_argv`, so a dead
/// remote process can never be silently replaced by an empty shell.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteTmuxCommands {
    pub version_argv: Vec<String>,
    pub create_argv: Vec<String>,
    pub configure_argv: Vec<String>,
    pub attach_argv: Vec<String>,
    pub probe_argv: Vec<String>,
    pub clients_argv: Vec<String>,
    pub capture_argv: Vec<String>,
    pub capture_full_argv: Vec<String>,
    pub activity_argv: Vec<String>,
    pub kill_argv: Vec<String>,
    pub forward_cleanup_argv: Vec<String>,
    pub support_cleanup_argv: Vec<String>,
    pub remote_socket_path: String,
}

impl Host {
    fn validated_tmux_target(
        &self,
        identity: &RemoteSessionIdentity,
    ) -> Result<&RemoteTarget, String> {
        let target = self
            .remote()
            .ok_or_else(|| "tmux session commands require a remote host".to_string())?;
        if target.multiplexer != RemoteMultiplexer::Tmux {
            return Err(format!(
                "remote '{}' uses the herdr multiplexer, which qmux cannot drive yet; use tmux for now",
                target.label
            ));
        }
        if target.id != identity.remote_id {
            return Err(format!(
                "remote session belongs to '{}' but workspace uses '{}'",
                identity.remote_id, target.id
            ));
        }
        validate_tmux_name("server", &identity.tmux_server)?;
        validate_tmux_name("session", &identity.tmux_session)?;
        Ok(target)
    }

    fn tmux_control_commands(
        &self,
        identity: &RemoteSessionIdentity,
        local_socket: &str,
    ) -> Result<RemoteTmuxCommands, String> {
        self.validated_tmux_target(identity)?;

        let remote_socket_path = format!("/tmp/{}.sock", identity.tmux_session);
        let exact_target = format!("={}", identity.tmux_session);
        let mut attach_args = tmux_server_args(identity);
        attach_args.extend([
            "attach-session".to_string(),
            "-t".to_string(),
            exact_target.clone(),
        ]);
        let mut probe_args = tmux_server_args(identity);
        probe_args.extend([
            "has-session".to_string(),
            "-t".to_string(),
            exact_target.clone(),
        ]);
        let mut clients_args = tmux_server_args(identity);
        clients_args.extend([
            "list-clients".to_string(),
            "-t".to_string(),
            exact_target.clone(),
            "-F".to_string(),
            "#{client_pid}".to_string(),
        ]);
        let capture_base = [
            "capture-pane".to_string(),
            "-p".to_string(),
            "-e".to_string(),
            "-t".to_string(),
            format!("{exact_target}:0.0"),
        ];
        let mut capture_args = tmux_server_args(identity);
        capture_args.extend(capture_base.clone());
        capture_args.extend([
            "-S".to_string(),
            "-8192".to_string(),
            // Only capture lines that have scrolled above the visible pane.
            // The attach stream redraws the current screen itself.
            "-E".to_string(),
            "-1".to_string(),
        ]);
        let mut capture_full_args = tmux_server_args(identity);
        capture_full_args.extend(capture_base);
        capture_full_args.extend([
            "-S".to_string(),
            "-".to_string(),
            "-E".to_string(),
            "-1".to_string(),
        ]);
        let mut activity_args = tmux_server_args(identity);
        activity_args.extend([
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            format!("{exact_target}:0.0"),
            "#{pane_current_command}\t#{pane_pid}\t#{pane_dead}".to_string(),
        ]);
        // set-option resolves a window target even for session options. The
        // colon makes it parse the session component and honor its exact-match
        // marker; without it, tmux looks for a session literally named "=…".
        let option_target = format!("{exact_target}:");
        let mut configure_args = tmux_server_args(identity);
        configure_args.extend([
            // Our attachment advertises xterm-256color but renders in Ghostty,
            // which supports DEC 2026 synchronized updates. Specify Sync
            // directly: tmux 3.2's `sync` feature uses the older DCS protocol.
            // Reserve a high array slot on our dedicated (-f /dev/null) server
            // to preserve defaults and avoid appending on every reconnect.
            "set-option".to_string(),
            "-s".to_string(),
            "terminal-overrides[100]".to_string(),
            // Escape the final semicolon for tmux's command parser as well as
            // shell quoting below, so it remains part of the terminfo program.
            "xterm-256color:Sync=\\E[?2026%?%p1%{1}%-%tl%eh%\\;".to_string(),
            ";".to_string(),
            // tmux is a durability layer for qmux, not a second interactive
            // multiplexer. Disabling both prefixes lets every control byte
            // reach the pane, while hiding the status line keeps the managed
            // session visually indistinguishable from a direct terminal.
            "set-option".to_string(),
            "-t".to_string(),
            option_target.clone(),
            "prefix".to_string(),
            "None".to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "-t".to_string(),
            option_target.clone(),
            "prefix2".to_string(),
            "None".to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "-t".to_string(),
            option_target.clone(),
            "status".to_string(),
            "off".to_string(),
            ";".to_string(),
            // Let tmux own remote history scrolling. Without mouse reporting,
            // Ghostty's alternate-screen scrolling becomes Up/Down key input.
            // The default wheel bindings enter copy-mode -e for shell history
            // (exiting at the bottom) and forward events to interactive apps.
            "set-option".to_string(),
            "-t".to_string(),
            option_target.clone(),
            "mouse".to_string(),
            "on".to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "-w".to_string(),
            "-t".to_string(),
            format!("{exact_target}:0"),
            "history-limit".to_string(),
            "50000".to_string(),
        ]);
        let mut kill_args = tmux_server_args(identity);
        kill_args.extend(["kill-session".to_string(), "-t".to_string(), exact_target]);
        let support_cleanup_argv = if let Some(support_dir) = identity.support_dir.as_deref() {
            self.remote_support_cleanup_argv(support_dir)?
        } else {
            Vec::new()
        };
        // OpenSSH's client-side StreamLocalBindUnlink does not reliably unlink
        // the listening path of a *remote* Unix-socket forward (the server owns
        // that bind). A killed ssh process can therefore strand this exact path
        // and make fail-closed reconnect loop forever. The session component is
        // validated above, so removing this one qmux-owned /tmp entry is bounded.
        let forward_cleanup_argv = self
            .ssh_argv(
                &RemoteCommand {
                    program: "rm",
                    args: vec![
                        "-f".to_string(),
                        "--".to_string(),
                        remote_socket_path.clone(),
                    ],
                    ..Default::default()
                },
                Interaction::Batch,
            )
            .expect("validated remote host");

        let attach_argv = self
            .ssh_argv(
                &RemoteCommand {
                    program: "tmux",
                    args: attach_args,
                    forwards: vec![SocketForward {
                        remote_path: remote_socket_path.clone(),
                        local_path: local_socket.to_string(),
                    }],
                    ..Default::default()
                },
                Interaction::Interactive,
            )
            .expect("validated remote host");
        let batch_argv = |args| {
            self.ssh_argv(
                &RemoteCommand {
                    program: "tmux",
                    args,
                    ..Default::default()
                },
                Interaction::Batch,
            )
            .expect("validated remote host")
        };

        Ok(RemoteTmuxCommands {
            version_argv: self
                .ssh_argv(
                    &RemoteCommand {
                        program: "tmux",
                        args: vec!["-V".to_string()],
                        ..Default::default()
                    },
                    Interaction::Batch,
                )
                .expect("validated remote host"),
            create_argv: Vec::new(),
            configure_argv: batch_argv(configure_args),
            attach_argv,
            probe_argv: batch_argv(probe_args),
            clients_argv: batch_argv(clients_args),
            capture_argv: batch_argv(capture_args),
            capture_full_argv: batch_argv(capture_full_args),
            activity_argv: batch_argv(activity_args),
            kill_argv: batch_argv(kill_args),
            forward_cleanup_argv,
            support_cleanup_argv,
            remote_socket_path,
        })
    }

    /// Builds attach-only commands for an already-created durable session.
    /// The returned create argv is empty by construction, making it impossible
    /// for reconnect or app recovery callers to recreate a missing process.
    pub fn existing_tmux_session_commands(
        &self,
        identity: &RemoteSessionIdentity,
        local_socket: &str,
    ) -> Result<RemoteTmuxCommands, String> {
        self.tmux_control_commands(identity, local_socket)
    }

    /// Builds every transport command needed to own and revisit a remote pane.
    #[allow(clippy::too_many_arguments)]
    pub fn tmux_session_commands(
        &self,
        identity: &RemoteSessionIdentity,
        local_socket: &str,
        remote_token: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
    ) -> Result<RemoteTmuxCommands, String> {
        self.validated_tmux_target(identity)?;
        if program.trim().is_empty() {
            return Err("a remote tmux session requires a program".to_string());
        }

        let mut commands = self.tmux_control_commands(identity, local_socket)?;
        let pane_envs = self.tmux_pane_envs(identity, remote_token, envs)?;
        let mut create_args = tmux_server_args(identity);
        create_args.extend([
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            identity.tmux_session.clone(),
            "-x".to_string(),
            cols.max(1).to_string(),
            "-y".to_string(),
            rows.max(1).to_string(),
            "-c".to_string(),
            cwd.to_string(),
        ]);
        for (key, value) in pane_envs {
            create_args.push("-e".to_string());
            create_args.push(format!("{key}={value}"));
        }
        create_args.push("--".to_string());
        create_args.push(program.to_string());
        create_args.extend(args.iter().cloned());

        commands.create_argv = self
            .ssh_argv(
                &RemoteCommand {
                    program: "tmux",
                    args: create_args,
                    ..Default::default()
                },
                Interaction::Batch,
            )
            .expect("validated remote host");

        Ok(commands)
    }

    /// Rewrites a prepared pane environment for execution inside an existing
    /// remote tmux session. This is also used by shell-level agent launches:
    /// their command is prepared by the local control server, so its freshly
    /// minted credentials initially name the local qmux CLI/socket and must be
    /// rebound to the already-forwarded remote endpoint before exec.
    pub fn tmux_pane_envs(
        &self,
        identity: &RemoteSessionIdentity,
        remote_token: &str,
        envs: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        if remote_token.is_empty() || remote_token.chars().any(char::is_control) {
            return Err("remote pane control token is invalid".to_string());
        }
        let target = self.validated_tmux_target(identity)?;
        let remote_workspace_root = self.remote_workspace_root()?;
        Ok(remote_pane_envs(
            target,
            &format!("/tmp/{}.sock", identity.tmux_session),
            remote_token,
            remote_workspace_root.as_deref(),
            envs,
        ))
    }

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
        // Detect half-open links (sleep/NAT/firewall drops) so the PTY reader
        // reaches EOF and the durable tmux session can be reattached.
        argv.push("-o".to_string());
        argv.push("ServerAliveInterval=15".to_string());
        argv.push("-o".to_string());
        argv.push("ServerAliveCountMax=3".to_string());
        // Reuse one authenticated transport for the launch's probes, uploads,
        // create/configure calls, and attachment. ~/.ssh is owner-only under
        // normal OpenSSH operation; %C hashes the full connection tuple and
        // avoids Unix-socket path-length blowups.
        argv.push("-o".to_string());
        argv.push("ControlMaster=auto".to_string());
        argv.push("-o".to_string());
        argv.push("ControlPersist=60".to_string());
        argv.push("-o".to_string());
        argv.push("ControlPath=~/.ssh/qmux-%C".to_string());

        if !remote.forwards.is_empty() {
            // Ask OpenSSH to unlink stale stream sockets where the selected
            // implementation supports it. Remote tmux attachments also run an
            // explicit, bounded cleanup command because many servers apply
            // this client option only to locally-bound sockets.
            argv.push("-o".to_string());
            argv.push("StreamLocalBindUnlink=yes".to_string());
            // The forwarded control socket carries a pane capability. Keep it
            // owner-only on the remote host, and refuse to report an attached
            // terminal when the hook transport could not be established.
            argv.push("-o".to_string());
            argv.push("StreamLocalBindMask=0177".to_string());
            argv.push("-o".to_string());
            argv.push("ExitOnForwardFailure=yes".to_string());
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
    pub fn remote_workspace_root(&self) -> Result<Option<String>, String> {
        let Some(target) = self.remote() else {
            return Ok(None);
        };
        let root = target
            .workspace_root
            .clone()
            .unwrap_or_else(|| DEFAULT_REMOTE_WORKSPACE_ROOT.to_string());
        self.expand_home(&root).map(Some)
    }

    pub fn remote_support_root(&self) -> Result<Option<String>, String> {
        self.remote_workspace_root().map(|root| {
            root.map(|root| {
                Path::new(&root)
                    .join(".qmux")
                    .join("support")
                    .display()
                    .to_string()
            })
        })
    }

    /// Builds cleanup for exactly one direct child of the managed support root.
    /// Persisted state is untrusted input here: reject parent components and
    /// punctuation before constructing the otherwise destructive remote argv.
    pub fn remote_support_cleanup_argv(&self, support_dir: &str) -> Result<Vec<String>, String> {
        let support_root = self
            .remote_support_root()?
            .ok_or_else(|| "remote support cleanup requires a remote host".to_string())?;
        let relative = Path::new(support_dir)
            .strip_prefix(&support_root)
            .map_err(|_| "remote support directory escapes its managed root".to_string())?;
        if relative.components().count() != 1
            || !relative.to_str().is_some_and(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
        {
            return Err("remote support directory is not a managed pane directory".to_string());
        }
        Ok(self
            .ssh_argv(
                &RemoteCommand {
                    program: "rm",
                    args: vec!["-rf".to_string(), "--".to_string(), support_dir.to_string()],
                    ..Default::default()
                },
                Interaction::Batch,
            )
            .expect("validated remote host"))
    }

    /// The login shell reported by sshd for this account. A remote pane must
    /// not inherit the local Mac user's shell path (commonly `/bin/zsh`) and
    /// assume that binary exists on a Linux builder.
    pub fn interactive_shell(&self, local_default: &str) -> Result<String, String> {
        let Some(target) = self.remote() else {
            return Ok(local_default.to_string());
        };
        if let Some(shell) = REMOTE_SHELLS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&target.ssh)
            .cloned()
        {
            return Ok(shell);
        }
        let output = self
            .command(RemoteCommand {
                program: "sh",
                args: vec![
                    "-lc".to_string(),
                    "printf '%s' \"${SHELL:-/bin/sh}\"".to_string(),
                ],
                ..Default::default()
            })
            .output()
            .map_err(|err| format!("failed to resolve shell on {}: {err}", target.label))?;
        let shell = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success()
            || !Path::new(&shell).is_absolute()
            || shell.contains(['\r', '\n'])
        {
            return Err(format!(
                "remote '{}' did not report a usable login shell",
                target.label
            ));
        }
        REMOTE_SHELLS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target.ssh.clone(), shell.clone());
        Ok(shell)
    }

    /// Expands a leading `~` against the *remote's* home directory.
    ///
    /// Necessary because every token this module sends is single-quoted — that
    /// is its security property, and it applies to paths as much as to refs —
    /// so a `~/…` path reaches the far side's shell as a literal tilde. Left
    /// alone, the default workspace root produces a directory actually named
    /// `~`, and the path qmux then hands the agent is not absolute.
    pub fn expand_home(&self, path: &str) -> Result<String, String> {
        let Some(target) = self.remote() else {
            return Ok(path.to_string());
        };
        let rest = if path == "~" {
            ""
        } else if let Some(rest) = path.strip_prefix("~/") {
            rest
        } else {
            return Ok(path.to_string());
        };
        let home = remote_home(target)?;
        Ok(match rest {
            "" => home,
            rest => format!("{}/{rest}", home.trim_end_matches('/')),
        })
    }

    /// Creates `dir` (and its parents) on this host.
    ///
    /// A remote group's directories are remote: `std::fs` here would make a
    /// stray tree on the machine qmux runs on and leave the agent pointed at a
    /// path that does not exist.
    pub fn create_dir_all(&self, dir: &Path) -> Result<(), String> {
        if self.is_local() {
            return std::fs::create_dir_all(dir)
                .map_err(|err| format!("failed to create {}: {err}", dir.display()));
        }
        let output = self
            .command(RemoteCommand {
                program: "mkdir",
                args: vec![
                    "-p".to_string(),
                    "--".to_string(),
                    dir.display().to_string(),
                ],
                ..Default::default()
            })
            .output()
            .map_err(|err| {
                format!(
                    "failed to create {} on {}: {err}",
                    dir.display(),
                    self.label()
                )
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "failed to create {} on {}: {}",
            dir.display(),
            self.label(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn tmux_server_args(identity: &RemoteSessionIdentity) -> Vec<String> {
    vec![
        "-L".to_string(),
        identity.tmux_server.clone(),
        // The dedicated server must not execute arbitrary user tmux config.
        // Passing this on every command is harmless once the server exists and
        // deterministic when this command starts it.
        "-f".to_string(),
        "/dev/null".to_string(),
    ]
}

fn validate_tmux_name(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "remote tmux {kind} name must be 1-80 ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

fn remote_pane_envs(
    target: &RemoteTarget,
    remote_socket: &str,
    remote_token: &str,
    remote_workspace_root: Option<&str>,
    envs: &[(String, String)],
) -> Vec<(String, String)> {
    let mut resolved = BTreeMap::new();
    for (key, value) in envs {
        // Never copy a local pane credential or an interactive-user credential
        // across SSH. The explicit token below belongs to the restricted remote
        // namespace and is the only control authority a remote process receives.
        if valid_env_name(key) && !matches!(key.as_str(), "QMUX_TOKEN" | "QMUX_USER_TOKEN") {
            resolved.insert(key.clone(), value.clone());
        }
    }
    // Local paths mean nothing on the far side. Apply these after caller envs
    // so an untrusted or stale launch specification cannot override them.
    resolved.insert("QMUX_SOCK".to_string(), remote_socket.to_string());
    resolved.insert("QMUX_CLI".to_string(), target.qmux_cli.clone());
    resolved.insert("QMUX_TOKEN".to_string(), remote_token.to_string());
    if let Some(root) = remote_workspace_root {
        resolved.insert("QMUX_WORKSPACE_ROOT".to_string(), root.to_string());
    }
    resolved.into_iter().collect()
}

fn valid_env_name(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The remote's `$HOME`, probed once per destination.
///
/// Cached because it cannot change under a live connection and every worktree
/// allocation would otherwise pay for another round trip.
static REMOTE_HOMES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REMOTE_SHELLS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn remote_home(target: &RemoteTarget) -> Result<String, String> {
    if let Some(home) = REMOTE_HOMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&target.ssh)
    {
        return Ok(home.clone());
    }

    // Deliberately *not* built through `remote_command_line`: this is the one
    // command whose point is that the far side's shell expands it, and it is
    // safe to leave unquoted precisely because it is a fixed literal with no
    // user-influenced part.
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
            "--",
            &target.ssh,
            "printf %s \"$HOME\"",
        ])
        .output()
        .map_err(|err| format!("failed to reach remote '{}': {err}", target.label))?;
    if !output.status.success() {
        return Err(format!(
            "could not read the home directory on remote '{}': {}",
            target.label,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let home = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !home.starts_with('/') {
        return Err(format!(
            "remote '{}' reported an unusable home directory ({home:?}); set workspaceRoot for it in qmux.config.json",
            target.label
        ));
    }
    REMOTE_HOMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(target.ssh.clone(), home.clone());
    Ok(home)
}

/// Seeds the home-directory cache so tests can exercise `~` expansion without
/// an ssh connection.
#[cfg(test)]
pub(crate) fn seed_remote_home(ssh: &str, home: &str) {
    REMOTE_HOMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(ssh.to_string(), home.to_string());
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
            if !valid_env_name(key) {
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

    fn remote_identity() -> RemoteSessionIdentity {
        RemoteSessionIdentity {
            remote_id: "saved-1".to_string(),
            tmux_server: "qmux".to_string(),
            tmux_session: "qmux-pane-7-deadbeef".to_string(),
            support_dir: None,
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
        assert!(argv.iter().any(|arg| arg == "ServerAliveInterval=15"));
        assert!(argv.iter().any(|arg| arg == "ServerAliveCountMax=3"));
        assert!(argv.iter().any(|arg| arg == "ControlMaster=auto"));
        assert!(argv.iter().any(|arg| arg == "ControlPersist=60"));
        assert!(argv.iter().any(|arg| arg == "ControlPath=~/.ssh/qmux-%C"));
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
                args: vec!["agent".to_string()],
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
                args: vec!["agent".to_string()],
                envs: vec![
                    ("QMUX_TOKEN".to_string(), "tok'en".to_string()),
                    ("AGENT_CWD".to_string(), "/a b".to_string()),
                ],
                ..Default::default()
            },
            Interaction::Batch,
        );
        assert_eq!(
            argv.last().unwrap(),
            r"env QMUX_TOKEN='tok'\''en' AGENT_CWD='/a b' 'qmux-cli' 'agent'"
        );
    }

    #[test]
    fn existing_remote_pane_env_rebinds_local_control_paths() {
        let host = remote_host();
        let envs = host
            .tmux_pane_envs(
                &remote_identity(),
                "remote-token",
                &[
                    ("QMUX_SOCK".to_string(), "/local/qmux.sock".to_string()),
                    ("QMUX_CLI".to_string(), "/Applications/qmux".to_string()),
                    ("QMUX_TOKEN".to_string(), "pane-token".to_string()),
                    ("QMUX_USER_TOKEN".to_string(), "user-token".to_string()),
                ],
            )
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            envs.get("QMUX_SOCK").map(String::as_str),
            Some("/tmp/qmux-pane-7-deadbeef.sock")
        );
        assert_eq!(envs.get("QMUX_CLI").map(String::as_str), Some("qmux-cli"));
        assert_eq!(
            envs.get("QMUX_WORKSPACE_ROOT").map(String::as_str),
            Some("/srv/work")
        );
        assert_eq!(
            envs.get("QMUX_TOKEN").map(String::as_str),
            Some("remote-token")
        );
        assert!(!envs.contains_key("QMUX_USER_TOKEN"));
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
    #[ignore = "requires local tmux and permission to create a Unix socket"]
    fn managed_tmux_configuration_resolves_exact_session() {
        let mut identity = remote_identity();
        identity.tmux_server = format!("qmux-test-options-{}", std::process::id());
        struct Cleanup(RemoteSessionIdentity);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("tmux")
                    .args(tmux_server_args(&self.0))
                    .arg("kill-server")
                    .output();
            }
        }
        let _cleanup = Cleanup(identity.clone());
        let commands = remote_host()
            .tmux_session_commands(
                &identity,
                "/unused.sock",
                "test-token",
                "/tmp",
                80,
                24,
                "sleep",
                &["60".to_string()],
                &[],
            )
            .unwrap();
        // Execute the actual remote shell commands locally, bypassing only SSH.
        let mut default_overrides = String::new();
        for argv in [
            &commands.create_argv,
            &commands.configure_argv,
            // Recovery reapplies the same configuration on the shared server.
            &commands.configure_argv,
            &commands.probe_argv,
        ] {
            let output = Command::new("sh")
                .args(["-c", argv.last().unwrap()])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if std::ptr::eq(argv, &commands.create_argv) {
                let output = Command::new("tmux")
                    .args(tmux_server_args(&identity))
                    .args(["show-options", "-s", "-v", "terminal-overrides"])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                default_overrides = String::from_utf8(output.stdout).unwrap();
            }
        }
        for (option, expected) in [
            ("prefix", "None"),
            ("prefix2", "None"),
            ("status", "off"),
            ("mouse", "on"),
        ] {
            let output = Command::new("tmux")
                .args(tmux_server_args(&identity))
                .args([
                    "show-options",
                    "-v",
                    "-t",
                    &format!("={}:", identity.tmux_session),
                    option,
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
        let output = Command::new("tmux")
            .args(tmux_server_args(&identity))
            .args(["show-options", "-s", "-v", "terminal-overrides"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let overrides = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            overrides
                .lines()
                .filter(|line| line.starts_with("xterm-256color:Sync="))
                .count(),
            1,
            "reconfiguration must not accumulate Sync overrides: {overrides}"
        );
        assert!(
            overrides
                .lines()
                .any(|line| line == "xterm-256color:Sync=\\E[?2026%?%p1%{1}%-%tl%eh%;")
        );
        for default in default_overrides.lines() {
            assert!(
                overrides.lines().any(|line| line == default),
                "default override must survive: {default}"
            );
        }
    }

    #[test]
    fn managed_tmux_commands_separate_create_from_attach_and_probe() {
        let commands = remote_host()
            .tmux_session_commands(
                &remote_identity(),
                "/local/run/qmux.sock",
                "remote-token",
                "/srv/work/a b",
                120,
                40,
                "/opt/qmux agent",
                &["--prompt".to_string(), "it's safe".to_string()],
                &[
                    ("QMUX_SOCK".to_string(), "/wrong/local.sock".to_string()),
                    ("QMUX_CLI".to_string(), "/wrong/local/qmux".to_string()),
                    ("QMUX_TOKEN".to_string(), "token value".to_string()),
                    ("BAD-NAME".to_string(), "dropped".to_string()),
                ],
            )
            .unwrap();

        let create = commands.create_argv.last().unwrap();
        assert!(create.contains("'tmux' '-L' 'qmux' '-f' '/dev/null' 'new-session' '-d'"));
        assert!(create.contains("'-s' 'qmux-pane-7-deadbeef'"));
        assert!(create.contains("'-x' '120' '-y' '40' '-c' '/srv/work/a b'"));
        assert!(create.contains("'-e' 'QMUX_CLI=qmux-cli'"));
        assert!(create.contains("'-e' 'QMUX_SOCK=/tmp/qmux-pane-7-deadbeef.sock'"));
        assert!(create.contains("'-e' 'QMUX_TOKEN=remote-token'"));
        assert!(!create.contains("QMUX_USER_TOKEN"));
        assert!(!create.contains("BAD-NAME"));
        assert!(create.ends_with("'--' '/opt/qmux agent' '--prompt' 'it'\\''s safe'"));
        assert!(
            commands
                .create_argv
                .windows(2)
                .any(|pair| pair == ["-o", "BatchMode=yes"])
        );
        let configure = commands.configure_argv.last().unwrap();
        assert!(
            configure.contains("'set-option' '-t' '=qmux-pane-7-deadbeef:' 'prefix' 'None' ';'")
        );
        assert!(
            configure.contains("'set-option' '-t' '=qmux-pane-7-deadbeef:' 'prefix2' 'None' ';'")
        );
        assert!(
            configure.contains("'set-option' '-t' '=qmux-pane-7-deadbeef:' 'status' 'off' ';'")
        );
        assert!(configure.contains("'set-option' '-t' '=qmux-pane-7-deadbeef:' 'mouse' 'on' ';'"));
        assert!(
            configure.contains(
                "'set-option' '-w' '-t' '=qmux-pane-7-deadbeef:0' 'history-limit' '50000'"
            )
        );

        let attach = commands.attach_argv.last().unwrap();
        assert!(attach.contains("'attach-session' '-t' '=qmux-pane-7-deadbeef'"));
        assert!(!attach.contains("new-session"));
        assert!(commands.attach_argv.contains(&"-t".to_string()));
        assert!(
            commands.attach_argv.windows(2).any(|pair| {
                pair == ["-R", "/tmp/qmux-pane-7-deadbeef.sock:/local/run/qmux.sock"]
            })
        );

        let probe = commands.probe_argv.last().unwrap();
        assert!(probe.contains("'has-session' '-t' '=qmux-pane-7-deadbeef'"));
        assert!(!probe.contains("new-session"));
        assert!(commands.capture_argv.last().unwrap().contains(
            "'capture-pane' '-p' '-e' '-t' '=qmux-pane-7-deadbeef:0.0' '-S' '-8192' '-E' '-1'"
        ));
        assert!(commands.capture_full_argv.last().unwrap().contains(
            "'capture-pane' '-p' '-e' '-t' '=qmux-pane-7-deadbeef:0.0' '-S' '-' '-E' '-1'"
        ));
        assert!(
            commands
                .activity_argv
                .last()
                .unwrap()
                .contains("'display-message' '-p'")
        );
        assert!(
            commands
                .kill_argv
                .last()
                .unwrap()
                .contains("'kill-session' '-t' '=qmux-pane-7-deadbeef'")
        );
        assert!(
            commands
                .forward_cleanup_argv
                .last()
                .unwrap()
                .contains("'rm' '-f' '--' '/tmp/qmux-pane-7-deadbeef.sock'")
        );
    }

    #[test]
    fn managed_tmux_commands_reject_cross_host_and_forged_names() {
        let mut identity = remote_identity();
        identity.remote_id = "another-host".to_string();
        let error = remote_host()
            .tmux_session_commands(
                &identity,
                "/local.sock",
                "remote-token",
                "/srv",
                80,
                24,
                "sh",
                &[],
                &[],
            )
            .unwrap_err();
        assert!(error.contains("belongs to 'another-host'"));

        identity = remote_identity();
        identity.tmux_session = "bad/name; new-session -s injected".to_string();
        let error = remote_host()
            .tmux_session_commands(
                &identity,
                "/local.sock",
                "remote-token",
                "/srv",
                80,
                24,
                "sh",
                &[],
                &[],
            )
            .unwrap_err();
        assert!(error.contains("tmux session name"));

        assert!(
            Host::Local
                .tmux_session_commands(
                    &remote_identity(),
                    "/local.sock",
                    "remote-token",
                    "/srv",
                    80,
                    24,
                    "sh",
                    &[],
                    &[]
                )
                .is_err()
        );
    }

    #[test]
    fn existing_session_commands_cannot_create_a_missing_session() {
        let commands = remote_host()
            .existing_tmux_session_commands(&remote_identity(), "/local/run/qmux.sock")
            .unwrap();

        assert!(commands.create_argv.is_empty());
        assert!(
            commands
                .attach_argv
                .last()
                .unwrap()
                .contains("'attach-session' '-t' '=qmux-pane-7-deadbeef'")
        );
        for argv in [
            &commands.attach_argv,
            &commands.configure_argv,
            &commands.probe_argv,
            &commands.capture_argv,
            &commands.kill_argv,
            &commands.forward_cleanup_argv,
        ] {
            assert!(!argv.iter().any(|arg| arg.contains("new-session")));
        }
    }

    #[test]
    fn remote_support_cleanup_is_confined_to_one_managed_pane_directory() {
        let mut identity = remote_identity();
        identity.support_dir = Some("/srv/work/.qmux/support/pane-7".to_string());
        let commands = remote_host()
            .existing_tmux_session_commands(&identity, "/local/run/qmux.sock")
            .unwrap();
        assert!(
            commands
                .support_cleanup_argv
                .last()
                .unwrap()
                .contains("'rm' '-rf' '--' '/srv/work/.qmux/support/pane-7'")
        );

        identity.support_dir = Some("/".to_string());
        assert!(
            remote_host()
                .existing_tmux_session_commands(&identity, "/local/run/qmux.sock")
                .unwrap_err()
                .contains("escapes")
        );

        identity.support_dir = Some("/srv/work/.qmux/support/..".to_string());
        assert!(
            remote_host()
                .existing_tmux_session_commands(&identity, "/local/run/qmux.sock")
                .unwrap_err()
                .contains("not a managed pane directory")
        );
    }

    #[test]
    fn an_unsupported_multiplexer_fails_the_launch_rather_than_guessing() {
        let mut reference = remote_ref(None, None);
        reference.multiplexer = RemoteMultiplexer::Herdr;
        let err = for_group(Some(&reference))
            .tmux_session_commands(
                &remote_identity(),
                "/run/qmux.sock",
                "remote-token",
                "/srv/work",
                80,
                24,
                "qmux",
                &[],
                &[],
            )
            .expect_err("herdr is not driveable yet");

        // Guessing an attach flag would silently start a second session on every
        // reconnect instead of reattaching, which is worse than refusing.
        assert!(err.contains("herdr"), "{err}");
        assert!(err.contains("tmux for now"), "{err}");
    }

    #[test]
    fn socket_forwards_become_ssh_reverse_forwards() {
        // How a remote agent's hooks reach the local control socket without the
        // pane token ever becoming a network credential.
        let argv = argv(
            &remote_host(),
            RemoteCommand {
                program: "qmux-cli",
                args: vec!["agent".to_string()],
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
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "StreamLocalBindMask=0177"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "ExitOnForwardFailure=yes"]),
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
        assert_eq!(Host::Local.remote_workspace_root(), Ok(None));
        assert_eq!(
            remote_host().remote_workspace_root().unwrap().as_deref(),
            Some("/srv/work")
        );
    }

    #[test]
    fn the_default_workspace_root_is_expanded_against_the_remotes_home() {
        // Every token this module sends is quoted, so a `~` would arrive at the
        // far side's shell literally: the worktree would land in a directory
        // actually *named* `~`, and the path handed to the agent would not be
        // absolute. Resolving it here is what stops that.
        seed_remote_home("user@devbox", "/home/dev");
        let bare = for_group(Some(&remote_ref(None, None)));
        assert_eq!(
            bare.remote_workspace_root().unwrap().as_deref(),
            Some("/home/dev/.qmux/workspaces")
        );
        assert_eq!(bare.expand_home("~").unwrap(), "/home/dev");
        // Only a leading `~/` is a home reference; nothing else is touched.
        assert_eq!(bare.expand_home("/srv/x").unwrap(), "/srv/x");
        assert_eq!(bare.expand_home("~x/y").unwrap(), "~x/y");
        // A local host never has a home to expand against.
        assert_eq!(Host::Local.expand_home("~/x").unwrap(), "~/x");
    }
}

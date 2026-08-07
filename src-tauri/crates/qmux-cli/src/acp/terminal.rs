//! The ACP `terminal/*` client capability, hosted on a real pty.
//!
//! ACP lets an Agent run commands in the Client's environment. Most clients
//! back that with piped stdio, which quietly changes what the command does:
//! `git`, `ls`, test runners and anything using `isatty` all take their
//! non-interactive branch, drop color, and sometimes buffer differently than
//! they would for a human. qmux already owns pty machinery, so terminals
//! created here run under one and the agent sees the same output the user
//! would.

use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// Cap on retained output when the agent does not set `outputByteLimit`. The
/// buffer keeps the newest bytes, so a chatty build still reports its tail (the
/// part that says what failed) rather than its first megabyte of banner.
const DEFAULT_OUTPUT_BYTE_LIMIT: usize = 1024 * 1024;

/// A pty large enough that commands laying out columns (`ls`, `git log
/// --graph`, test reporters) don't wrap to something unreadable. The agent
/// never sees a resize, so one fixed generous size is the whole policy.
const TERMINAL_ROWS: u16 = 40;
const TERMINAL_COLS: u16 = 160;

#[derive(Clone, Copy, Debug)]
pub struct ExitInfo {
    pub exit_code: Option<i32>,
}

struct OutputBuffer {
    bytes: Vec<u8>,
    truncated: bool,
    limit: usize,
}

impl OutputBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() <= self.limit {
            return;
        }
        let overflow = self.bytes.len() - self.limit;
        self.bytes.drain(..overflow);
        self.truncated = true;
    }

    /// The retained bytes as text. Draining by byte count can slice a
    /// multi-byte character in half, so decode lossily rather than dropping
    /// the whole buffer when the cut lands mid-codepoint.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

struct Terminal {
    output: Mutex<OutputBuffer>,
    /// `None` until the child is reaped. Guarded by `exit_signal` so
    /// `wait_for_exit` can block without polling.
    exit: Mutex<Option<ExitInfo>>,
    exit_signal: Condvar,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl Terminal {
    fn snapshot(&self) -> (String, bool, Option<ExitInfo>) {
        let output = self.output.lock().unwrap_or_else(|err| err.into_inner());
        let exit = self.exit.lock().unwrap_or_else(|err| err.into_inner());
        (output.text(), output.truncated, *exit)
    }

    fn wait(&self) -> ExitInfo {
        let mut exit = self.exit.lock().unwrap_or_else(|err| err.into_inner());
        while exit.is_none() {
            exit = self
                .exit_signal
                .wait(exit)
                .unwrap_or_else(|err| err.into_inner());
        }
        exit.expect("loop exits only once set")
    }

    fn kill(&self) {
        // Killing an already-reaped child is not an error worth surfacing: ACP
        // explicitly allows `terminal/kill` after exit, and the terminal stays
        // valid for `output`/`wait_for_exit` either way.
        let mut killer = self.killer.lock().unwrap_or_else(|err| err.into_inner());
        let _ = killer.kill();
    }
}

#[derive(Default)]
pub struct TerminalRegistry {
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    next_id: AtomicU64,
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(
        &self,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&Path>,
        output_byte_limit: Option<usize>,
    ) -> Result<String, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: TERMINAL_ROWS,
                cols: TERMINAL_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| format!("failed to open pty: {err}"))?;

        let mut builder = CommandBuilder::new(command);
        for arg in args {
            builder.arg(arg);
        }
        for (key, value) in env {
            builder.env(key, value);
        }
        if let Some(cwd) = cwd {
            builder.cwd(cwd);
        }
        // Commands inherit this from the pty, not from the bridge's own
        // environment, so set something sane for anything that reads $TERM.
        builder.env("TERM", "xterm-256color");

        let mut child = pair
            .slave
            .spawn_command(builder)
            .map_err(|err| format!("failed to run '{command}': {err}"))?;
        // The slave fd must close in this process or the reader below never
        // sees EOF after the child exits.
        drop(pair.slave);

        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| format!("failed to read pty output: {err}"))?;

        let terminal = Arc::new(Terminal {
            output: Mutex::new(OutputBuffer {
                bytes: Vec::new(),
                truncated: false,
                limit: output_byte_limit
                    .filter(|limit| *limit > 0)
                    .unwrap_or(DEFAULT_OUTPUT_BYTE_LIMIT),
            }),
            exit: Mutex::new(None),
            exit_signal: Condvar::new(),
            killer: Mutex::new(killer),
        });

        let id = format!("term_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.terminals
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(id.clone(), Arc::clone(&terminal));

        let reader_terminal = Arc::clone(&terminal);
        thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => reader_terminal
                        .output
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .push(&chunk[..count]),
                }
            }
        });

        // `master` is held by the waiter so the pty outlives the child; dropping
        // it early would hand the reader an EOF (and the child a SIGHUP) the
        // moment `create` returned.
        let master = pair.master;
        let waiter_terminal = Arc::clone(&terminal);
        thread::spawn(move || {
            let status = child.wait().ok();
            let mut exit = waiter_terminal
                .exit
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            *exit = Some(ExitInfo {
                exit_code: status.map(|status| status.exit_code() as i32),
            });
            waiter_terminal.exit_signal.notify_all();
            drop(exit);
            drop(master);
        });

        Ok(id)
    }

    fn get(&self, terminal_id: &str) -> Result<Arc<Terminal>, String> {
        self.terminals
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| format!("unknown terminal '{terminal_id}'"))
    }

    pub fn output(&self, terminal_id: &str) -> Result<(String, bool, Option<ExitInfo>), String> {
        Ok(self.get(terminal_id)?.snapshot())
    }

    pub fn wait_for_exit(&self, terminal_id: &str) -> Result<ExitInfo, String> {
        Ok(self.get(terminal_id)?.wait())
    }

    pub fn kill(&self, terminal_id: &str) -> Result<(), String> {
        self.get(terminal_id)?.kill();
        Ok(())
    }

    pub fn release(&self, terminal_id: &str) -> Result<(), String> {
        let terminal = self
            .terminals
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(terminal_id)
            .ok_or_else(|| format!("unknown terminal '{terminal_id}'"))?;
        terminal.kill();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limited(limit: usize) -> OutputBuffer {
        OutputBuffer {
            bytes: Vec::new(),
            truncated: false,
            limit,
        }
    }

    #[test]
    fn output_buffer_keeps_the_tail_and_flags_truncation() {
        let mut buffer = limited(4);
        buffer.push(b"abc");
        assert_eq!(buffer.text(), "abc");
        assert!(!buffer.truncated);

        buffer.push(b"defg");
        assert_eq!(buffer.text(), "defg");
        assert!(buffer.truncated);
    }

    #[test]
    fn output_buffer_survives_a_cut_through_a_multibyte_character() {
        let mut buffer = limited(4);
        // "é" is two bytes; trimming to the last four bytes slices it in half.
        buffer.push("éa".as_bytes());
        buffer.push("bc".as_bytes());
        assert!(buffer.truncated);
        // Lossy decoding keeps the intact tail rather than discarding the buffer.
        assert!(buffer.text().ends_with("abc"));
    }

    #[test]
    fn a_terminal_runs_under_a_pty_and_reports_its_exit() {
        let registry = TerminalRegistry::new();
        let id = registry
            .create(
                "sh",
                &[
                    "-c".to_string(),
                    "test -t 1 && echo tty; exit 3".to_string(),
                ],
                &[],
                None,
                None,
            )
            .expect("terminal spawns");

        let exit = registry.wait_for_exit(&id).expect("terminal is known");
        assert_eq!(exit.exit_code, Some(3));

        // The reader thread may still be draining after the child is reaped.
        let mut output = String::new();
        for _ in 0..100 {
            output = registry.output(&id).expect("terminal is known").0;
            if output.contains("tty") {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            output.contains("tty"),
            "command should see a tty on stdout, got {output:?}"
        );

        registry.release(&id).expect("terminal is known");
        assert!(registry.output(&id).is_err());
    }
}

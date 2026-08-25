//! Bounded JSONL subprocess transport shared by pane-less research runners.

use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

const MAX_STDOUT_LINE_BYTES: usize = 1024 * 1024;
const MAX_STDERR_LOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_PENDING_EVENTS: usize = 256;

pub fn reconcile_session_id(
    expected: &mut Option<String>,
    observed: Option<&str>,
    label: &str,
) -> Result<(), String> {
    let Some(observed) = observed else {
        return Ok(());
    };
    let observed = validate_session_id(observed, label)?;
    if expected
        .as_deref()
        .is_some_and(|expected| expected != observed)
    {
        return Err(format!(
            "{label} reported a different session id than qmux requested"
        ));
    }
    *expected = Some(observed);
    Ok(())
}

pub fn validate_session_id(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("{label} reported an invalid session id"));
    }
    Ok(value.to_string())
}

pub enum JsonlReceive {
    Value(Value),
    Timeout,
    Eof,
}

pub struct JsonlProcess {
    child: Child,
    events: Option<Receiver<Result<Value, String>>>,
    stdout_reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
}

impl JsonlProcess {
    pub fn spawn(
        binary: &str,
        args: &[String],
        cwd: &Path,
        stderr_log: &Path,
        label: &str,
    ) -> Result<Self, String> {
        if let Some(parent) = stderr_log.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create research log dir {}: {err}",
                    parent.display()
                )
            })?;
        }
        let mut log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(stderr_log)
            .map_err(|err| {
                format!(
                    "failed to open research log {}: {err}",
                    stderr_log.display()
                )
            })?;
        let _ = writeln!(
            log,
            "qmux: {label} research spawn binary={binary} cwd={}",
            cwd.display()
        );

        let mut child = Command::new(binary)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|err| format!("failed to spawn {label} research process: {err}"))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            terminate_after_setup_failure(&mut child);
            format!("{label} research stdout was not piped")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            terminate_after_setup_failure(&mut child);
            format!("{label} research stderr was not piped")
        })?;
        let (tx, events) = mpsc::sync_channel(MAX_PENDING_EVENTS);
        let stdout_label = label.to_string();
        let stdout_reader = thread::spawn(move || read_jsonl(stdout, tx, &stdout_label));
        let stderr_reader = thread::spawn(move || copy_bounded(stderr, log));
        Ok(Self {
            child,
            events: Some(events),
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<JsonlReceive, String> {
        let Some(events) = self.events.as_ref() else {
            return Ok(JsonlReceive::Eof);
        };
        match events.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(JsonlReceive::Value(value)),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => Ok(JsonlReceive::Timeout),
            Err(RecvTimeoutError::Disconnected) => Ok(JsonlReceive::Eof),
        }
    }

    pub fn finish(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    self.kill();
                    return Err(
                        "headless research process did not exit after closing stdout".into(),
                    );
                }
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(err) => {
                    self.kill();
                    return Err(format!(
                        "failed to wait for headless research process: {err}"
                    ));
                }
            }
        };
        self.join_readers();
        Ok(status)
    }

    pub fn kill(&mut self) {
        crate::claude_sdk::terminate_process_tree(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_readers();
    }

    fn join_readers(&mut self) {
        // Dropping the receiver also releases a stdout reader blocked on the
        // bounded queue when a run is cancelled or its owner is dropped.
        self.events.take();
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for JsonlProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill();
        } else {
            self.join_readers();
        }
    }
}

fn read_jsonl(stdout: impl Read, tx: SyncSender<Result<Value, String>>, label: &str) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut buf = Vec::new();
        match reader
            .by_ref()
            .take((MAX_STDOUT_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => break,
            Ok(_) if buf.len() > MAX_STDOUT_LINE_BYTES => {
                let _ = tx.send(Err(format!("{label} stdout line exceeded 1 MB")));
                break;
            }
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let parsed = serde_json::from_str(line)
                    .map_err(|err| format!("invalid {label} research JSON: {err}"));
                if tx.send(parsed).is_err() {
                    break;
                }
            }
            Err(err) => {
                let _ = tx.send(Err(format!(
                    "failed to read {label} research stdout: {err}"
                )));
                break;
            }
        }
    }
}

fn copy_bounded(mut source: impl Read, mut target: impl Write) {
    let mut written = 0usize;
    let mut marker_written = false;
    let mut buf = [0u8; 8192];
    loop {
        let read = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        let mut overflowed = written >= MAX_STDERR_LOG_BYTES;
        if written < MAX_STDERR_LOG_BYTES {
            let keep = read.min(MAX_STDERR_LOG_BYTES - written);
            let _ = target.write_all(&buf[..keep]);
            written += keep;
            overflowed = keep < read;
        }
        if overflowed && !marker_written {
            let _ = target.write_all(b"\nqmux: stderr log truncated at 4 MB\n");
            marker_written = true;
        }
    }
    let _ = target.flush();
}

fn terminate_after_setup_failure(child: &mut Child) {
    crate::claude_sdk::terminate_process_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn session_ids_must_be_safe_and_consistent() {
        let mut session_id = Some("requested-1".to_string());
        reconcile_session_id(&mut session_id, Some("requested-1"), "test").unwrap();
        assert_eq!(session_id.as_deref(), Some("requested-1"));

        let mismatch = reconcile_session_id(&mut session_id, Some("different-1"), "test")
            .expect_err("a CLI must not redirect qmux to a different session");
        assert!(mismatch.contains("different session id"));

        let mut empty = None;
        assert!(reconcile_session_id(&mut empty, Some("../../unsafe"), "test").is_err());
        assert!(validate_session_id("--dangerously-bypass-approvals", "test").is_err());
    }

    #[test]
    fn streams_json_lines_and_captures_stderr() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("qmux-jsonl-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-jsonl");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"ok\"}'\nprintf '%s\\n' warning >&2\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let log = dir.join("stderr.log");
        let mut process =
            JsonlProcess::spawn(script.to_str().unwrap(), &[], &dir, &log, "test").unwrap();
        assert!(matches!(
            process.recv_timeout(Duration::from_secs(1)).unwrap(),
            JsonlReceive::Value(value) if value["type"] == "ok"
        ));
        loop {
            if matches!(
                process.recv_timeout(Duration::from_secs(1)).unwrap(),
                JsonlReceive::Eof
            ) {
                break;
            }
        }
        assert!(process.finish(Duration::from_secs(1)).unwrap().success());
        assert!(fs::read_to_string(log).unwrap().contains("warning"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stderr_truncation_continues_draining_the_pipe() {
        let bytes = vec![b'x'; MAX_STDERR_LOG_BYTES + 8192];
        let mut source = std::io::Cursor::new(bytes);
        let mut target = Vec::new();

        copy_bounded(&mut source, &mut target);

        assert_eq!(source.position(), (MAX_STDERR_LOG_BYTES + 8192) as u64);
        assert!(target.len() < MAX_STDERR_LOG_BYTES + 128);
        assert!(target.ends_with(b"qmux: stderr log truncated at 4 MB\n"));
    }

    #[test]
    fn cancellation_releases_a_full_event_queue() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("qmux-jsonl-cancel-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-jsonl-flood");
        fs::write(
            &script,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 1000 ]; do\n  printf '%s\\n' '{\"type\":\"event\"}'\n  i=$((i + 1))\ndone\nsleep 5\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let log = dir.join("stderr.log");
        let mut process =
            JsonlProcess::spawn(script.to_str().unwrap(), &[], &dir, &log, "test").unwrap();
        thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        process.kill();

        assert!(started.elapsed() < Duration::from_secs(2));
        fs::remove_dir_all(dir).ok();
    }
}

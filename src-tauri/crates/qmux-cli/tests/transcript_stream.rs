use qmux_cli::transcript_stream::{Cursor, Frame};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

struct Stream(Child, Receiver<Frame>);
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn launch(adapter: &str, session: &str, path: &Path, home: &Path, cursor: &Cursor) -> Stream {
    let mut child = Command::new(env!("CARGO_BIN_EXE_qmux-cli"))
        .args([
            "transcript-stream",
            adapter,
            session,
            path.to_str().unwrap(),
            &serde_json::to_string(cursor).unwrap(),
        ])
        .env("CODEX_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let frame = serde_json::from_str(&line.unwrap()).unwrap();
            if tx.send(frame).is_err() {
                break;
            }
        }
    });
    Stream(child, rx)
}
fn next_data(stream: &Stream) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = stream
            .1
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if !frame.data.is_empty() {
            return frame;
        }
    }
}
#[test]
fn claude_partial_line_and_reconnect_resume_without_duplicate_records() {
    let dir = std::env::temp_dir().join(format!("qmux-stream-process-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.jsonl");
    fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n{\"type\":",
    )
    .unwrap();
    let stream = launch("claude", "session", &path, &dir, &Cursor::default());
    let first = next_data(&stream);
    assert_eq!(first.data.lines().count(), 1);
    assert_eq!(first.historical_end, fs::metadata(&path).unwrap().len());
    drop(stream);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\"assistant\",\"message\":{\"content\":\"reply\"}}\n")
        .unwrap();
    let resumed = launch("claude", "session", &path, &dir, &first.cursor);
    let second = next_data(&resumed);
    assert_eq!(second.start, first.cursor.offset);
    assert!(!second.reset);
    assert!(!second.data.contains("hello"));
    assert!(second.data.contains("reply"));
    assert_eq!(second.historical_end, fs::metadata(&path).unwrap().len());
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{}\n")
        .unwrap();
    let live = next_data(&resumed);
    assert_eq!(live.historical_end, second.historical_end);
    assert!(live.cursor.offset > live.historical_end);
    drop(resumed);
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn codex_discovers_rollout_by_session_and_rejects_wrong_hint() {
    let dir = std::env::temp_dir().join(format!("qmux-stream-discovery-{}", std::process::id()));
    let sessions = dir.join("sessions/2026/09/04");
    fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("rollout-date-codex-session.jsonl");
    fs::write(
        &path,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-session\"}}\n",
    )
    .unwrap();
    let wrong = dir.join("wrong.jsonl");
    fs::write(
        &wrong,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"another\"}}\n",
    )
    .unwrap();
    let stream = launch("codex", "codex-session", &wrong, &dir, &Cursor::default());
    let frame = next_data(&stream);
    assert_eq!(frame.path, path.to_str().unwrap());
    assert!(frame.data.contains("codex-session"));
    drop(stream);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn large_image_record_resumes_mid_record_and_delivers_following_messages() {
    use qmux_cli::transcript_stream::MAX_CHUNK;
    let dir = std::env::temp_dir().join(format!("qmux-large-stream-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("large-session.jsonl");
    let record = serde_json::json!({"type":"user", "message":{"content":[
        {"type":"image", "source":{"type":"base64", "data":"A".repeat(5 * 1024 * 1024)}},
        {"type":"text", "text":"image prompt"}
    ]}})
    .to_string()
        + "\n";
    let contents =
        record + "{\"type\":\"assistant\",\"message\":{\"content\":\"following response\"}}\n";
    fs::write(&path, &contents).unwrap();
    let stream = launch("claude", "large-session", &path, &dir, &Cursor::default());
    let first = next_data(&stream);
    assert_eq!(first.data.len(), MAX_CHUNK);
    assert!(!first.data.ends_with('\n'));
    let mut assembled = first.data.clone();
    drop(stream);
    let resumed = launch("claude", "large-session", &path, &dir, &first.cursor);
    let mut cursor = first.cursor;
    while assembled.len() < contents.len() {
        let frame = next_data(&resumed);
        assert!(!frame.reset);
        assert_eq!(frame.start, cursor.offset);
        assert!(frame.data.len() <= MAX_CHUNK);
        assembled.push_str(&frame.data);
        cursor = frame.cursor;
    }
    assert_eq!(assembled, contents);
    drop(resumed);
    fs::remove_dir_all(dir).unwrap();
}

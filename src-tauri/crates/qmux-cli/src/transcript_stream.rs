//! Read-only, bounded JSONL transport. Invoked by the desktop over SSH, never
//! through the forwarded control socket. Cursor offsets refer to source bytes.
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_SESSION_META: usize = 4 * 1024 * 1024;
pub const MAX_CHUNK: usize = 128 * 1024;
// JSON escaping expands a byte by at most six characters.
pub const MAX_FRAME: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Cursor {
    pub offset: u64,
    pub identity: String,
    pub anchor: Vec<u8>,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Frame {
    pub session: String,
    pub path: String,
    pub start: u64,
    pub reset: bool,
    /// Source size at connection/rotation; bytes through here are historical.
    pub historical_end: u64,
    pub cursor: Cursor,
    pub data: String,
}

fn open_regular(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err("transcript must be an owned regular file".into());
    }
    Ok(file)
}

fn belongs(path: &Path, adapter: &str, session: &str) -> bool {
    let Ok(mut file) = open_regular(path) else {
        return false;
    };
    matches_session(&mut file, path, adapter, session)
}

fn matches_session(file: &mut File, path: &Path, adapter: &str, session: &str) -> bool {
    if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
        return false;
    }
    if adapter == "claude" {
        return path.file_stem().and_then(|s| s.to_str()) == Some(session);
    }
    let mut first = String::new();
    if BufReader::new(file)
        .take(MAX_SESSION_META as u64 + 1)
        .read_line(&mut first)
        .is_err()
        || first.len() > MAX_SESSION_META
    {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&first) else {
        return false;
    };
    value["type"] == "session_meta" && value["payload"]["id"].as_str() == Some(session)
}

pub fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 120
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn discover(adapter: &str, session: &str, hint: Option<&Path>) -> Result<PathBuf, String> {
    if !matches!(adapter, "claude" | "codex") || !valid_session(session) {
        return Err("invalid transcript adapter or session".into());
    }
    if let Some(path) = hint
        && belongs(path, adapter, session)
    {
        return Ok(path.into());
    }
    let home = dirs::home_dir().ok_or("home directory unavailable")?;
    let root = if adapter == "claude" {
        std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or(home.join(".claude"))
            .join("projects")
    } else {
        std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or(home.join(".codex"))
            .join("sessions")
    };
    let mut pending = vec![(root, 0)];
    let mut examined = 0;
    while let Some((dir, depth)) = pending.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            examined += 1;
            if examined > 100_000 {
                return Err("transcript discovery limit exceeded".into());
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file()
                && entry.file_name().to_string_lossy().contains(session)
                && belongs(&entry.path(), adapter, session)
            {
                return Ok(entry.path());
            }
        }
    }
    Err("transcript is not available yet".into())
}

fn anchor(file: &mut File, offset: u64) -> Result<Vec<u8>, String> {
    let len = offset.min(256);
    file.seek(SeekFrom::Start(offset - len))
        .map_err(|e| e.to_string())?;
    let mut data = vec![0; len as usize];
    file.read_exact(&mut data).map_err(|e| e.to_string())?;
    Ok(data)
}

pub fn read_frame(path: &Path, session: &str, previous: &Cursor) -> Result<Frame, String> {
    read_open_frame(open_regular(path)?, path, session, previous)
}

fn read_open_frame(
    mut file: File,
    path: &Path,
    session: &str,
    previous: &Cursor,
) -> Result<Frame, String> {
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let identity = format!("{}:{}", meta.dev(), meta.ino());
    let reset = previous.identity != identity
        || previous.offset > meta.len()
        || anchor(&mut file, previous.offset).ok().as_ref() != Some(&previous.anchor);
    let start = if reset { 0 } else { previous.offset };
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    // A record may contain multi-megabyte image data. Ship bounded UTF-8
    // fragments and let the local JSONL tail assemble records on disk.
    let mut bytes = Vec::with_capacity(MAX_CHUNK);
    Read::by_ref(&mut file)
        .take(MAX_CHUNK as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let read_len = bytes.len();
    match std::str::from_utf8(&bytes) {
        Ok(_) => {}
        Err(error) if error.error_len().is_none() => bytes.truncate(error.valid_up_to()),
        Err(error) => return Err(error.to_string()),
    }
    // Hold the incomplete last record at EOF. Full chunks can split a record;
    // a cursor in that record resumes byte-for-byte, including after reconnect.
    if read_len < MAX_CHUNK {
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1);
        bytes.truncate(complete);
    }
    let data = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let offset = start + data.len() as u64;
    let cursor = Cursor {
        offset,
        identity,
        anchor: anchor(&mut file, offset)?,
    };
    Ok(Frame {
        session: session.into(),
        path: path.to_string_lossy().into(),
        start,
        reset,
        historical_end: meta.len(),
        cursor,
        data,
    })
}

pub fn run(args: Vec<String>) -> Result<(), String> {
    if args.len() != 4 {
        return Err("usage: transcript-stream ADAPTER SESSION PATH CURSOR_JSON".into());
    }
    let mut cursor: Cursor = serde_json::from_str(&args[3]).map_err(|e| e.to_string())?;
    let path = discover(
        &args[0],
        &args[1],
        (!args[2].is_empty()).then(|| Path::new(&args[2])),
    )?;
    let mut historical_end = open_regular(&path)?
        .metadata()
        .map_err(|e| e.to_string())?
        .len();
    let mut output = std::io::stdout().lock();
    loop {
        // Recheck identity on every batch: replacing a rollout must not attach
        // this reader to a different Codex session at the same path.
        let mut file = open_regular(&path)?;
        if !matches_session(&mut file, &path, &args[0], &args[1]) {
            return Err("transcript session changed".into());
        }
        let mut frame = read_open_frame(file, &path, &args[1], &cursor)?;
        if frame.reset {
            historical_end = frame.historical_end;
        }
        frame.historical_end = historical_end;
        serde_json::to_writer(&mut output, &frame).map_err(|e| e.to_string())?;
        output
            .write_all(b"\n")
            .and_then(|_| output.flush())
            .map_err(|e| e.to_string())?;
        cursor = frame.cursor;
        if frame.data.len() < MAX_CHUNK - 3 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_character_split_at_chunk_boundary_is_preserved() {
        let path =
            std::env::temp_dir().join(format!("qmux-chunk-utf8-{}.jsonl", std::process::id()));
        let contents = format!("{{\"text\":\"{}😀tail\"}}\n", "a".repeat(MAX_CHUNK - 10));
        fs::write(&path, &contents).unwrap();
        let first = read_frame(&path, "s", &Cursor::default()).unwrap();
        assert_eq!(first.data.len(), MAX_CHUNK - 1);
        let second = read_frame(&path, "s", &first.cursor).unwrap();
        assert_eq!(format!("{}{}", first.data, second.data), contents);
        assert!(!second.reset);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn incomplete_utf8_waits_for_a_complete_record() {
        let path =
            std::env::temp_dir().join(format!("qmux-stream-utf8-{}.jsonl", std::process::id()));
        fs::write(&path, b"{}\n{\"text\":\"\xf0\x9f").unwrap();
        let first = read_frame(&path, "s", &Cursor::default()).unwrap();
        assert_eq!(first.data, "{}\n");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\x98\x80\"}\n")
            .unwrap();
        let next = read_frame(&path, "s", &first.cursor).unwrap();
        assert_eq!(next.data, "{\"text\":\"😀\"}\n");
        assert!(!next.reset);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn partial_records_resume_and_rewrites_reset() {
        let path = std::env::temp_dir().join(format!("qmux-stream-{}.jsonl", std::process::id()));
        fs::write(&path, "{\"a\":1}\n{\"b\":").unwrap();
        let first = read_frame(&path, "session", &Cursor::default()).unwrap();
        assert_eq!(first.data, "{\"a\":1}\n");
        assert!(first.reset);
        fs::write(&path, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let next = read_frame(&path, "session", &first.cursor).unwrap();
        assert_eq!(next.data, "{\"b\":2}\n");
        assert!(!next.reset);
        assert!(
            read_frame(&path, "session", &next.cursor)
                .unwrap()
                .data
                .is_empty()
        );
        fs::write(&path, "{\"c\":3}\n").unwrap();
        let reset = read_frame(&path, "session", &next.cursor).unwrap();
        assert!(reset.reset);
        assert_eq!(reset.start, 0);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn rejects_symlinks_and_wrong_sessions() {
        let dir = std::env::temp_dir().join(format!("qmux-stream-safe-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"a\"}}\n",
        )
        .unwrap();
        assert!(belongs(&path, "codex", "a"));
        assert!(!belongs(&path, "codex", "b"));
        let link = dir.join("link.jsonl");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_regular(&link).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}

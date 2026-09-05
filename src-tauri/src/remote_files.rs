//! Local snapshots uploaded by `qmux open` from SSH-backed panes.
//!
//! Each upload gets its own owner-only directory and only the completed file is
//! granted to the pane's preview token. The directory is never a file-server
//! root, so relative references from HTML cannot pull in sibling remote files.

use qmux_proto::MAX_REMOTE_OPEN_FILE_BYTES;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const STORE_DIR: &str = "remote-files";
const ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);

pub fn store_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".qmux").join(STORE_DIR)
}

fn store_root_is_directory(root: &Path) -> bool {
    fs::symlink_metadata(root)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if !qmux_proto::is_safe_browser_preview_name(name) {
        return Err(format!("'{name}' is not a browser-previewable file"));
    }
    Ok(())
}

/// Streams exactly `size` bytes into a private temporary file, fsyncs it, and
/// atomically publishes the snapshot. The caller supplies a server-generated
/// `upload_id`; no remote-controlled path component is used above the basename.
pub fn stage<R: Read>(
    workspace_root: &Path,
    upload_id: &str,
    name: &str,
    size: u64,
    reader: &mut R,
) -> Result<PathBuf, String> {
    validate_name(name)?;
    if size > MAX_REMOTE_OPEN_FILE_BYTES {
        return Err(format!(
            "remote preview is larger than the {} MiB limit",
            MAX_REMOTE_OPEN_FILE_BYTES / (1024 * 1024)
        ));
    }

    let root = store_root(workspace_root);
    fs::create_dir_all(&root)
        .map_err(|err| format!("failed to create remote preview store: {err}"))?;
    if !store_root_is_directory(&root) {
        return Err("remote preview store is not a safe directory".to_string());
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("failed to secure remote preview store: {err}"))?;
    let upload_dir = root.join(upload_id);
    fs::create_dir(&upload_dir)
        .map_err(|err| format!("failed to reserve remote preview storage: {err}"))?;
    if let Err(err) = fs::set_permissions(&upload_dir, fs::Permissions::from_mode(0o700)) {
        let _ = fs::remove_dir(&upload_dir);
        return Err(format!("failed to secure remote preview storage: {err}"));
    }

    let temporary = upload_dir.join(".upload");
    let final_path = upload_dir.join(name);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|err| format!("failed to create remote preview: {err}"))?;
        let copied = std::io::copy(&mut reader.take(size), &mut file)
            .map_err(|err| format!("failed to receive remote preview: {err}"))?;
        if copied != size {
            return Err(format!(
                "remote preview ended after {copied} bytes; expected {size}"
            ));
        }
        file.flush()
            .map_err(|err| format!("failed to flush remote preview: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("failed to sync remote preview: {err}"))?;
        drop(file);
        fs::rename(&temporary, &final_path)
            .map_err(|err| format!("failed to publish remote preview: {err}"))?;
        let _ = OpenOptions::new()
            .read(true)
            .open(&upload_dir)
            .and_then(|directory| directory.sync_all());
        Ok(final_path.clone())
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&upload_dir);
    }
    result
}

/// Returns the canonical path only when it is one complete file in qmux's
/// managed remote-preview store.
pub fn resolve_staged_file(workspace_root: &Path, path: &Path) -> Option<PathBuf> {
    let store = store_root(workspace_root);
    if !store_root_is_directory(&store) {
        return None;
    }
    let root = fs::canonicalize(store).ok()?;
    let canonical = fs::canonicalize(path).ok()?;
    (canonical.is_file() && canonical.starts_with(&root)).then_some(canonical)
}

/// Removes old upload directories that no persisted artifact references. The
/// grace period preserves the frontend's undo window and avoids racing a newly
/// completed shell-pane upload during startup.
pub fn remove_orphaned(workspace_root: &Path, referenced: &[PathBuf]) {
    let root = store_root(workspace_root);
    if !store_root_is_directory(&root) {
        return;
    }
    let Ok(canonical_root) = fs::canonicalize(&root) else {
        return;
    };
    let kept = referenced
        .iter()
        .filter_map(|path| resolve_staged_file(workspace_root, path))
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<HashSet<_>>();
    let Ok(entries) = fs::read_dir(&canonical_root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let old_enough = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= ORPHAN_GRACE);
        if old_enough && !kept.contains(&path) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::symlink;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qmux-remote-files-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn stages_exact_bytes_owner_only() {
        let root = test_root("stage");
        let mut source = Cursor::new(b"<h1>hello</h1>".to_vec());
        let path = stage(&root, "upload-1", "report.html", 14, &mut source).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"<h1>hello</h1>");
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            resolve_staged_file(&root, &path),
            fs::canonicalize(&path).ok()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_upload_is_removed() {
        let root = test_root("partial");
        let error = stage(
            &root,
            "upload-1",
            "report.html",
            5,
            &mut Cursor::new(b"abc".to_vec()),
        )
        .unwrap_err();
        assert!(error.contains("ended after 3 bytes"));
        assert!(!store_root(&root).join("upload-1").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_paths_unknown_extensions_and_oversize_headers() {
        assert!(validate_name("../report.html").is_err());
        assert!(validate_name("archive.zip").is_err());
        let root = test_root("large");
        let error = stage(
            &root,
            "upload-1",
            "report.html",
            MAX_REMOTE_OPEN_FILE_BYTES + 1,
            &mut Cursor::new(Vec::new()),
        )
        .unwrap_err();
        assert!(error.contains("larger than"));
        assert!(!store_root(&root).exists());
    }

    #[test]
    fn accepts_the_full_ten_mib_limit() {
        let root = test_root("limit");
        let mut source = std::io::repeat(0).take(MAX_REMOTE_OPEN_FILE_BYTES);
        let path = stage(
            &root,
            "upload-1",
            "limit.pdf",
            MAX_REMOTE_OPEN_FILE_BYTES,
            &mut source,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            MAX_REMOTE_OPEN_FILE_BYTES
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_a_symlinked_store_root() {
        let root = test_root("symlink");
        let outside = test_root("outside");
        fs::create_dir_all(root.join(".qmux")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, store_root(&root)).unwrap();
        let error = stage(
            &root,
            "upload-1",
            "report.html",
            3,
            &mut Cursor::new(b"abc".to_vec()),
        )
        .unwrap_err();
        assert!(error.contains("not a safe directory"));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}

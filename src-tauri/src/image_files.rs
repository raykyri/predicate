//! Reads pasted transcript images for inline rendering in the webview.
//!
//! Claude Code stores pasted image bytes in `~/.claude/image-cache` and writes a
//! literal `[Image: source: <path>]` marker into the transcript. The webview CSP
//! only allows `data:`/`blob:` image sources, so the frontend asks this module to
//! read the file and return a `data:` URL it can hand straight to an `<img>` tag.
//!
//! The read is confined to the user's home directory (where the image cache
//! lives). The command is reachable only from the trusted webview, but confining
//! it keeps a compromised renderer from repurposing it as a general file-read
//! oracle: the path is canonicalized first, so a symlink whose name merely ends
//! in `.png` cannot smuggle out a secret — the location and (re-checked on the
//! canonical target) extension tests both apply to the real target.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Cap on a single rendered image. Pasted screenshots are a few MB at most;
/// anything larger would balloon the webview heap once base64-inflated.
pub const MAX_TRANSCRIPT_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Raster formats a webview `<img>` renders natively. SVG is deliberately
/// excluded: the image cache only ever holds raster pastes, and keeping markup
/// formats out avoids reasoning about scriptable content entirely.
fn image_mime_for_extension(extension: &str) -> Option<&'static str> {
    if extension.eq_ignore_ascii_case("png") {
        Some("image/png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("image/jpeg")
    } else if extension.eq_ignore_ascii_case("gif") {
        Some("image/gif")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("image/webp")
    } else if extension.eq_ignore_ascii_case("bmp") {
        Some("image/bmp")
    } else {
        None
    }
}

pub fn read_transcript_image(path: &Path) -> Result<String, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or_else(|| "cannot determine your home directory to validate the image".to_string())?;
    read_transcript_image_within(path, &home)
}

/// Confinement-root-injectable core of [`read_transcript_image`], kept separate
/// so tests can point `allowed_root` at a scratch directory. Mirrors
/// `research::read_markdown_document_file_within`: canonicalize (resolving
/// symlinks and `..`) before any check, verify location, extension, file type,
/// and size against the real target, and never buffer more than the cap even if
/// the file grows between inspection and reading.
fn read_transcript_image_within(path: &Path, allowed_root: &Path) -> Result<String, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|err| format!("failed to resolve {}: {err}", path.display()))?;
    let root = fs::canonicalize(allowed_root).unwrap_or_else(|_| allowed_root.to_path_buf());
    if !canonical.starts_with(&root) {
        return Err("only images under your home directory can be displayed".to_string());
    }

    let mime = canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(image_mime_for_extension)
        .ok_or_else(|| "only PNG, JPEG, GIF, WebP, and BMP images can be displayed".to_string())?;

    let metadata = fs::metadata(&canonical)
        .map_err(|err| format!("failed to inspect {}: {err}", canonical.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", canonical.display()));
    }
    if metadata.len() > MAX_TRANSCRIPT_IMAGE_BYTES as u64 {
        return Err(format!(
            "images are limited to {} MB",
            MAX_TRANSCRIPT_IMAGE_BYTES / (1024 * 1024)
        ));
    }

    let file = fs::File::open(&canonical)
        .map_err(|err| format!("failed to open {}: {err}", canonical.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_TRANSCRIPT_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("failed to read {}: {err}", canonical.display()))?;
    if bytes.len() > MAX_TRANSCRIPT_IMAGE_BYTES {
        return Err(format!(
            "images are limited to {} MB",
            MAX_TRANSCRIPT_IMAGE_BYTES / (1024 * 1024)
        ));
    }

    Ok(format!("data:{mime};base64,{}", STANDARD.encode(&bytes)))
}

/// Persists a base64-encoded image the user pasted into a composer/queue and
/// returns its absolute path, for referencing in the prompt as `[Image: <path>]`
/// (which the queue renders as a thumbnail and, once delivered as text, the agent
/// loads from the path). Written into `~/.claude/image-cache` — the same directory
/// Claude Code's own pastes live in, and inside the home-confined root that
/// [`read_transcript_image`] will later read the thumbnail from.
pub fn save_pasted_image(data_base64: &str, extension: &str) -> Result<String, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or_else(|| "cannot determine your home directory to store the image".to_string())?;
    let cache_dir = home.join(".claude").join("image-cache");
    save_pasted_image_within(data_base64, extension, &cache_dir)
}

/// Cache-dir-injectable core of [`save_pasted_image`], kept separate so tests can
/// point it at a scratch directory.
fn save_pasted_image_within(
    data_base64: &str,
    extension: &str,
    cache_dir: &Path,
) -> Result<String, String> {
    let ext = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    // Reuse the render allow-list as the write allow-list: only formats the webview
    // can later display as a thumbnail may be stored.
    if image_mime_for_extension(&ext).is_none() {
        return Err("only PNG, JPEG, GIF, WebP, and BMP images can be pasted".to_string());
    }
    let bytes = STANDARD
        .decode(data_base64.trim())
        .map_err(|err| format!("failed to decode pasted image data: {err}"))?;
    if bytes.is_empty() {
        return Err("the pasted image was empty".to_string());
    }
    if bytes.len() > MAX_TRANSCRIPT_IMAGE_BYTES {
        return Err(format!(
            "images are limited to {} MB",
            MAX_TRANSCRIPT_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    fs::create_dir_all(cache_dir)
        .map_err(|err| format!("failed to create the image cache directory: {err}"))?;
    // A monotonic-clock timestamp plus a process-lifetime counter names the file
    // uniquely without collision even for two pastes within the same millisecond.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = cache_dir.join(format!("qmux-paste-{nanos}-{seq}.{ext}"));
    fs::write(&path, &bytes).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    let absolute = fs::canonicalize(&path).unwrap_or(path);
    Ok(absolute.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_folder() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-image-files-{nanos}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn image_read_returns_a_data_url_with_the_extension_mime() {
        let folder = temp_folder();
        // Content is opaque bytes to this module, which trusts the
        // (canonical-target) extension for the mime type.
        let bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let png = folder.join("paste.PNG");
        fs::write(&png, bytes).unwrap();
        let url = read_transcript_image_within(&png, &folder).unwrap();
        assert_eq!(
            url,
            format!("data:image/png;base64,{}", STANDARD.encode(bytes))
        );

        let jpeg = folder.join("photo.jpeg");
        fs::write(&jpeg, [0xff, 0xd8]).unwrap();
        let url = read_transcript_image_within(&jpeg, &folder).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");

        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn save_pasted_image_writes_a_readable_file_and_rejects_bad_input() {
        let folder = temp_folder();
        let cache = folder.join("image-cache");
        let bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

        // A leading dot on the extension is tolerated, and the written file reads
        // back through the confined reader as the same bytes.
        let path = save_pasted_image_within(&STANDARD.encode(bytes), ".png", &cache).unwrap();
        assert!(path.ends_with(".png"), "{path}");
        let url = read_transcript_image_within(Path::new(&path), &folder).unwrap();
        assert_eq!(
            url,
            format!("data:image/png;base64,{}", STANDARD.encode(bytes))
        );

        let bad_ext = save_pasted_image_within(&STANDARD.encode(bytes), "svg", &cache).unwrap_err();
        assert!(bad_ext.contains("PNG"), "{bad_ext}");

        let empty = save_pasted_image_within("", "png", &cache).unwrap_err();
        assert!(empty.contains("empty"), "{empty}");

        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn image_read_rejects_unknown_extensions_and_oversized_files() {
        let folder = temp_folder();

        let svg = folder.join("vector.svg");
        fs::write(&svg, "<svg/>").unwrap();
        let error = read_transcript_image_within(&svg, &folder).unwrap_err();
        assert!(error.contains("PNG"), "{error}");

        let oversized = folder.join("huge.png");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_TRANSCRIPT_IMAGE_BYTES as u64 + 1).unwrap();
        let error = read_transcript_image_within(&oversized, &folder).unwrap_err();
        assert!(error.contains("20 MB"), "{error}");

        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn image_read_is_confined_to_the_allowed_root() {
        let root = temp_folder();
        let outside = temp_folder();

        // A .png outside the confinement root is refused even though it exists.
        let external = outside.join("external.png");
        fs::write(&external, [0x89]).unwrap();
        assert!(read_transcript_image_within(&external, &root).is_err());

        // A .png symlink inside the root pointing at a non-image secret outside
        // it is rejected: canonicalization resolves the link, so the location
        // and extension checks see the real target, not the .png link name.
        let secret = outside.join("secret.conf");
        fs::write(&secret, "token=hunter2").unwrap();
        let link = root.join("innocent.png");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(read_transcript_image_within(&link, &root).is_err());

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}

//! Wire types for qmux's control protocol: newline-delimited JSON requests and
//! responses exchanged between in-pane processes (the `qmux` CLI, agent hooks)
//! and the app's control listener. `browser.open_file` alone carries an exact,
//! size-declared byte body after its JSON header. Shared by the server
//! (`control_socket`) and the client (`qmux-cli`) so the two sides can never
//! drift. Transport-agnostic on purpose — today the frames travel over a local
//! Unix socket, but nothing here may assume that: a forwarded socket or a
//! network transport must be able to reuse these types unchanged.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single control request. `token` normally scopes the request to exactly one
/// pane: the server resolves the pane from the token and treats any pane id
/// inside `payload` as advisory only. The notification-only public entry point
/// also accepts an empty token from a same-user Unix-socket peer; no other
/// command may use that exception.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRequest {
    pub token: String,
    pub command: String,
    #[serde(default)]
    pub payload: Value,
}

/// The server's reply to one `ControlRequest`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlResponse {
    pub ok: bool,
    pub data: Value,
    pub error: Option<String>,
}

/// Largest single file `qmux open` will carry from a remote pane to the desktop.
/// The transport streams exactly this many bytes at most; neither endpoint should
/// allocate a buffer proportional to the file size.
pub const MAX_REMOTE_OPEN_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Metadata in the JSON header that precedes a streamed remote file body.
/// `name` must be a safe basename; the desktop chooses every parent directory
/// and never accepts a destination path from the remote caller.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserOpenFileHeader {
    pub name: String,
    pub size: u64,
}

/// Extensions the embedded browser handles as a top-level local preview. This
/// is shared by the remote CLI's early validation and the desktop's authoritative
/// MIME check so a file cannot pass one endpoint and fail only after transfer.
pub fn is_browser_preview_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "html"
            | "htm"
            | "css"
            | "js"
            | "mjs"
            | "json"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "ico"
            | "pdf"
            | "mp4"
            | "webm"
            | "mp3"
            | "wav"
            | "txt"
            | "log"
            | "md"
            | "markdown"
            | "csv"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
    )
}

/// Whether an untrusted remote basename is safe to materialize and maps to one
/// of the explicit embedded-browser preview types.
pub fn is_safe_browser_preview_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 240
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && !matches!(name, "." | "..")
        && name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| is_browser_preview_extension(extension))
}

pub const PUBLIC_API_VERSION: u32 = 1;

/// Public command invocation carried inside `cli.call`. Authentication and
/// caller identity stay in the outer control request and are never accepted
/// from these arguments.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicControlRequest {
    pub operation: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicControlError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicControlResponse {
    pub ok: bool,
    pub api_version: u32,
    #[serde(default)]
    pub result: Value,
    pub error: Option<PublicControlError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_requests_reject_unknown_envelope_fields() {
        let error = serde_json::from_value::<PublicControlRequest>(serde_json::json!({
            "operation": "pane.list",
            "argument": {}
        }))
        .err()
        .expect("unknown envelope fields must fail closed");
        assert!(error.to_string().contains("unknown field `argument`"));
    }

    #[test]
    fn browser_preview_extensions_are_explicit_and_case_insensitive() {
        assert!(is_browser_preview_extension("HTML"));
        assert!(is_browser_preview_extension("png"));
        assert!(is_browser_preview_extension("markdown"));
        assert!(!is_browser_preview_extension("zip"));
        assert!(!is_browser_preview_extension(""));
        assert!(is_safe_browser_preview_name("report.HTML"));
        assert!(!is_safe_browser_preview_name("../report.html"));
        assert!(!is_safe_browser_preview_name("archive.zip"));
    }
}

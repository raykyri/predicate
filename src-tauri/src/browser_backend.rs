//! Experimental discovery endpoint for Codex's in-app Browser plugin.
//!
//! The Browser plugin currently discovers JSON-RPC peers by scanning Unix
//! sockets in /tmp/codex-browser-use. This protocol is not a public OpenAI API,
//! so keep this adapter small and capability-conservative: it identifies qmux
//! to agent.browsers.list(), answers health checks, and advertises no automation
//! commands yet. The embedded preview remains an iframe, not a CDP endpoint.

use serde_json::{Value, json};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;

const DISCOVERY_DIR: &str = "/tmp/codex-browser-use";
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Managed by Tauri so a clean app shutdown removes the discoverable socket.
pub struct BrowserDiscoverySocket {
    socket_path: PathBuf,
}

impl BrowserDiscoverySocket {
    pub fn path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for BrowserDiscoverySocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub fn start_browser_discovery() -> Result<BrowserDiscoverySocket, String> {
    let directory = PathBuf::from(DISCOVERY_DIR);
    let created = !directory.exists();
    fs::create_dir_all(&directory)
        .map_err(|err| format!("failed to create {}: {err}", directory.display()))?;
    if created {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o1777))
            .map_err(|err| format!("failed to secure {}: {err}", directory.display()))?;
    }
    remove_stale_qmux_sockets(&directory);

    let socket_path = directory.join(format!("qmux-{}.sock", std::process::id()));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to reclaim {}: {err}",
                socket_path.display()
            ));
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .map_err(|err| format!("failed to bind {}: {err}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("failed to secure {}: {err}", socket_path.display()))?;

    thread::Builder::new()
        .name("qmux-browser-discovery".to_string())
        .spawn(move || {
            for connection in listener.incoming() {
                match connection {
                    Ok(stream) => {
                        let _ = thread::Builder::new()
                            .name("qmux-browser-discovery-client".to_string())
                            .spawn(move || {
                                if let Err(err) = serve_connection(stream) {
                                    eprintln!("qmux: browser discovery client failed: {err}");
                                }
                            });
                    }
                    Err(err) => {
                        eprintln!("qmux: browser discovery accept failed: {err}");
                        break;
                    }
                }
            }
        })
        .map_err(|err| format!("failed to start browser discovery listener: {err}"))?;

    Ok(BrowserDiscoverySocket { socket_path })
}

fn remove_stale_qmux_sockets(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_qmux_socket = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("qmux-") && name.ends_with(".sock"));
        if is_qmux_socket && UnixStream::connect(&path).is_err() {
            let _ = fs::remove_file(path);
        }
    }
}

fn serve_connection(mut stream: UnixStream) -> Result<(), String> {
    loop {
        let request = match read_frame(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(err) => return Err(err),
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = handle_request(&request, id);
        write_frame(&mut stream, &response)?;
    }
}

fn handle_request(request: &Value, id: Value) -> Value {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "getInfo" => {
            let session_id = request
                .pointer("/params/session_id")
                .or_else(|| request.pointer("/params/sessionId"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let build_flavor =
                std::env::var("QMUX_CODEX_APP_BUILD_FLAVOR").unwrap_or_else(|_| "prod".to_string());
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "id": "qmux",
                    "name": "qmux Browser",
                    "type": "iab",
                    "family": "qmux",
                    "capabilities": {
                        "browser": [],
                        "tab": []
                    },
                    "metadata": {
                        "codexSessionId": session_id,
                        "codexAppBuildFlavor": build_flavor,
                        "qmuxVersion": env!("CARGO_PKG_VERSION"),
                        "automation": "discovery-only"
                    }
                }
            })
        }
        "ping" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": "pong"
        }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("method '{method}' is not implemented by qmux")
            }
        }),
    }
}

fn read_frame(stream: &mut UnixStream) -> Result<Option<Value>, String> {
    let mut length = [0_u8; 4];
    match stream.read_exact(&mut length) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(format!("failed to read frame length: {err}")),
    }
    let length = u32::from_ne_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(format!("invalid browser discovery frame length {length}"));
    }
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .map_err(|err| format!("failed to read browser discovery frame: {err}"))?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|err| format!("invalid browser discovery JSON: {err}"))
}

fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    let payload = serde_json::to_vec(value)
        .map_err(|err| format!("failed to encode browser discovery response: {err}"))?;
    let length = u32::try_from(payload.len())
        .map_err(|_| "browser discovery response is too large".to_string())?;
    stream
        .write_all(&length.to_ne_bytes())
        .and_then(|_| stream.write_all(&payload))
        .map_err(|err| format!("failed to write browser discovery response: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_info_echoes_codex_session_metadata() {
        let response = handle_request(
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "getInfo",
                "params": { "session_id": "session-123" }
            }),
            json!(7),
        );
        assert_eq!(response["result"]["type"], "iab");
        assert_eq!(
            response["result"]["metadata"]["codexSessionId"],
            "session-123"
        );
        assert_eq!(response["result"]["capabilities"]["browser"], json!([]));
    }

    #[test]
    fn native_length_prefixed_json_round_trips() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let request = json!({"jsonrpc": "2.0", "id": "a", "method": "ping"});
        write_frame(&mut client, &request).unwrap();
        assert_eq!(read_frame(&mut server).unwrap(), Some(request));
    }

    #[test]
    fn unknown_methods_return_json_rpc_error() {
        let response = handle_request(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "createTab"}),
            json!(1),
        );
        assert_eq!(response["error"]["code"], -32601);
    }

    /// Manual compatibility probe for the private Browser-plugin discovery
    /// protocol. Run this ignored test, then call agent.browsers.list() from a
    /// Codex turn within its wait window.
    #[test]
    #[ignore = "manual Codex Browser-plugin compatibility probe"]
    fn expose_discovery_socket_for_manual_codex_probe() {
        let socket = start_browser_discovery().unwrap();
        eprintln!("probe socket: {}", socket.path().display());
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

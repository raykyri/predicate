//! Wire types for qmux's control protocol: the newline-delimited JSON requests
//! and responses exchanged between in-pane processes (the `qmux` CLI, agent
//! hooks) and the app's control listener. Shared by the server
//! (`control_socket`) and the client (`qmux-cli`) so the two sides can never
//! drift. Transport-agnostic on purpose — today the frames travel over a local
//! Unix socket, but nothing here may assume that: a forwarded socket or a
//! network transport must be able to reuse these types unchanged.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single control request. `token` scopes the request to exactly one pane:
/// the server resolves the pane from the token and treats any pane id inside
/// `payload` as advisory only.
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

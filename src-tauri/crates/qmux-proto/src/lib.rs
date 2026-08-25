//! Wire types for qmux's control protocol: the newline-delimited JSON requests
//! and responses exchanged between in-pane processes (the `qmux` CLI, agent
//! hooks) and the app's control listener. Shared by the server
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
}

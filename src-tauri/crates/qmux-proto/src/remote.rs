//! Wire types for qmux remote control (docs/remote-control-plan.md).
//!
//! Frames travel over QUIC streams between a paired device and the app. The
//! codec itself (length prefix + tag byte) lives with the transport in the
//! app's `remote::frames`; this module owns only the JSON shapes so a future
//! second client (the iOS app) shares them with the server and cannot drift.
//!
//! Stream layout:
//! - `control` — one bidirectional stream the client opens first. Client
//!   sends [`RemoteFrame::Hello`] then `Call`/`Subscribe`/`PaneStream`;
//!   server answers `Ready`, `CallResult`, and fatal `GoingAway`.
//! - `events` — one server-opened unidirectional stream (after the first
//!   `Subscribe` that asks for events) carrying `Event` and `Resync`.
//! - `pane:<id>` — one server-opened unidirectional stream per requested
//!   pane: a `PaneHeader` frame, then raw PTY bytes and reset markers as
//!   binary frames (tags [`FRAME_TAG_PANE_BYTES`] / [`FRAME_TAG_PANE_RESET`]).
//!
//! Unknown JSON *fields* are tolerated (forward compatibility inside one
//! protocol version); unknown frame *types* are a protocol error.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// ALPN for paired-device sessions. Only an already-paired endpoint id may
/// connect; the accept gate closes anything else before its first frame.
pub const REMOTE_ALPN: &[u8] = b"qmux/remote/1";
/// ALPN for the pairing ceremony. Any endpoint may connect, but only while a
/// pairing window is open, and the only thing it can do is present a
/// one-time secret.
pub const PAIR_ALPN: &[u8] = b"qmux/pair/1";

/// Version of the remote frame protocol; distinct from the public control
/// API version carried inside `CallResult` responses.
pub const REMOTE_PROTOCOL_VERSION: u32 = 1;

/// Tag byte for a JSON [`RemoteFrame`].
pub const FRAME_TAG_JSON: u8 = 0;
/// Tag byte for raw PTY bytes on a pane stream.
pub const FRAME_TAG_PANE_BYTES: u8 = 1;
/// Tag byte for a pane-stream reset marker: the ring overflowed, the client
/// should clear its screen; the next bytes frame is a fresh full replay.
pub const FRAME_TAG_PANE_RESET: u8 = 2;

/// Cap on one JSON frame. Control payloads are small; transcript turns in
/// `CallResult` are the largest legitimate payload.
pub const MAX_JSON_FRAME_BYTES: u32 = 4 * 1024 * 1024;
/// Cap on one pane byte frame; the fan-out slices larger runs.
pub const MAX_PANE_FRAME_BYTES: u32 = 256 * 1024;

/// Every JSON frame on every remote stream, both directions. Sides reject
/// frames they do not expect for their role and stream.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteFrame {
    /// Client's first frame on the control stream.
    #[serde(rename_all = "camelCase")]
    Hello {
        api_version: u32,
        /// Client identifier for logs, e.g. "qmux-ios/0.1".
        client: String,
        /// Human name shown in the sessions list, e.g. "Ray's iPhone".
        #[serde(default)]
        device_name: Option<String>,
    },
    /// Server's answer to `Hello`; the session is live once this arrives.
    #[serde(rename_all = "camelCase")]
    Ready {
        api_version: u32,
        /// App identifier, e.g. "qmux/0.3.1".
        app: String,
        /// Mac name shown in the client's UI.
        mac_name: String,
        /// Whether this device was paired read-only.
        read_only: bool,
    },
    /// One public control invocation; answered by `CallResult` with the same
    /// `seq`. Calls may be pipelined.
    #[serde(rename_all = "camelCase")]
    Call {
        seq: u64,
        operation: String,
        #[serde(default)]
        arguments: Value,
    },
    /// The `PublicControlResponse` for one `Call`, verbatim.
    #[serde(rename_all = "camelCase")]
    CallResult { seq: u64, response: Value },
    /// Replaces the session's subscription set. `events` turns the event
    /// stream on or off; `panes` names the panes whose byte streams should
    /// be open (missing ones are opened, absent ones are closed).
    #[serde(rename_all = "camelCase")]
    Subscribe {
        #[serde(default)]
        events: bool,
        #[serde(default)]
        panes: Vec<String>,
    },
    /// First frame on the events stream.
    EventStreamHeader {},
    /// One `QmuxEvent`, verbatim, on the events stream.
    #[serde(rename_all = "camelCase")]
    Event { event: Value },
    /// The event queue overflowed and state events were dropped. The client
    /// must refetch pane/agent/queue state; nothing is replayed.
    Resync {},
    /// First frame on a pane byte stream.
    #[serde(rename_all = "camelCase")]
    PaneHeader {
        id: String,
        /// Pane dimensions so the client can letterbox; never resizable
        /// remotely.
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
    },
    /// Pairing: the scanned one-time secret plus how the device introduces
    /// itself. Only valid on [`PAIR_ALPN`] while a window is open.
    #[serde(rename_all = "camelCase")]
    PairRequest {
        api_version: u32,
        secret: String,
        device_name: String,
    },
    /// Pairing outcome. `accepted` false covers deny, timeout, and a closed
    /// window; the message is display-ready.
    #[serde(rename_all = "camelCase")]
    PairResult {
        accepted: bool,
        #[serde(default)]
        mac_name: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Server is closing the session deliberately (toggle off, revocation).
    #[serde(rename_all = "camelCase")]
    GoingAway { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_tolerate_unknown_fields() {
        let frame = RemoteFrame::Call {
            seq: 7,
            operation: "pane.list".to_string(),
            arguments: Value::Null,
        };
        let encoded = serde_json::to_string(&frame).unwrap();
        assert!(encoded.contains("\"type\":\"call\""));
        let decoded: RemoteFrame = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded, RemoteFrame::Call { seq: 7, .. }));

        // Unknown fields inside a known frame must parse: a newer client may
        // send extras within the same protocol version.
        let with_extra: RemoteFrame = serde_json::from_str(
            r#"{"type":"hello","apiVersion":1,"client":"qmux-ios/0.1","futureField":true}"#,
        )
        .unwrap();
        assert!(matches!(
            with_extra,
            RemoteFrame::Hello { api_version: 1, .. }
        ));
    }

    #[test]
    fn unknown_frame_types_are_rejected() {
        let error = serde_json::from_str::<RemoteFrame>(r#"{"type":"launchMissiles"}"#)
            .err()
            .expect("unknown frame types must fail closed");
        assert!(error.to_string().contains("launchMissiles"));
    }
}

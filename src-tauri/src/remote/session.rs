//! Per-connection remote session: hello/ready, then call dispatch.
//!
//! [`RemoteSession`] is deliberately transport-free so `control.rs` can be
//! exercised against it with no endpoint bound (stage 1); the frame loop
//! here drives the same struct over a live connection (stage 2).
//!
//! Calls are pipelined: each `Call` runs on a blocking thread (the control
//! dispatcher locks `AppState`), bounded by a per-session semaphore, and its
//! `CallResult` is serialized back through one writer task. A long
//! `pane.waitOutput` therefore never blocks a concurrent `ping`.

use crate::remote::endpoint::{CLOSE_GOING_AWAY, CLOSE_PROTOCOL_ERROR, RemoteAccess};
use crate::remote::frames;
use crate::state::AppState;
use iroh::endpoint::Connection;
use qmux_proto::remote::{REMOTE_PROTOCOL_VERSION, RemoteFrame};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the client has to send `Hello` before the connection is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// In-flight calls per session. Covers a screen of pipelined reads plus a
/// couple of long waits; at the cap further calls queue behind the stream.
const MAX_CONCURRENT_CALLS: usize = 8;
/// Outbound frames buffered for the writer task before call handlers block.
const WRITER_QUEUE: usize = 64;

/// State one connected device carries across calls.
#[derive(Debug)]
pub struct RemoteSession {
    /// Human name shown in the sessions list, e.g. "Ray's iPhone".
    pub device_name: String,
    /// Devices paired read-only may look at everything and change nothing.
    pub read_only: bool,
    /// The pane this session's context resolves against when an operation
    /// needs "the current pane". Set by the `session.focus` operation; falls
    /// back to the app's active pane while unset or stale.
    focus_pane: Mutex<Option<String>>,
}

impl RemoteSession {
    pub fn new(device_name: impl Into<String>, read_only: bool) -> Self {
        Self {
            device_name: device_name.into(),
            read_only,
            focus_pane: Mutex::new(None),
        }
    }

    pub fn focus_pane(&self) -> Option<String> {
        self.focus_pane.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn set_focus_pane(&self, pane_id: String) {
        if let Ok(mut guard) = self.focus_pane.lock() {
            *guard = Some(pane_id);
        }
    }
}

/// Runs one paired device's session to completion. The access came from the
/// accept gate; the connection's ALPN was already checked.
pub(crate) async fn serve_remote_connection(
    state: AppState,
    access: RemoteAccess,
    connection: Connection,
) {
    let device = access.device_name.clone();
    match run_control_stream(state, access, &connection).await {
        Ok(()) => {}
        Err(reason) => {
            eprintln!("qmux: remote session for {device} ended: {reason}");
            connection.close(CLOSE_PROTOCOL_ERROR.into(), reason.as_bytes());
        }
    }
}

async fn run_control_stream(
    state: AppState,
    access: RemoteAccess,
    connection: &Connection,
) -> Result<(), String> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|err| format!("no control stream: {err}"))?;

    let hello = tokio::time::timeout(HELLO_TIMEOUT, frames::read_json(&mut recv))
        .await
        .map_err(|_| "timed out waiting for hello".to_string())??
        .ok_or_else(|| "connection closed before hello".to_string())?;
    let RemoteFrame::Hello { api_version, .. } = hello else {
        return Err("expected hello as the first frame".to_string());
    };
    if api_version != REMOTE_PROTOCOL_VERSION {
        let _ = frames::write_json(
            &mut send,
            &RemoteFrame::GoingAway {
                reason: format!(
                    "protocol version {api_version} is not supported (this qmux speaks {REMOTE_PROTOCOL_VERSION})"
                ),
            },
        )
        .await;
        // Closing immediately would race the frame off the wire: QUIC close
        // discards unacknowledged stream data. Finish the stream and give
        // the peer a moment to read the reason before forcing the close.
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(3), connection.closed()).await;
        connection.close(CLOSE_GOING_AWAY.into(), b"protocol version");
        return Ok(());
    }

    // One writer task serializes every outbound frame; call handlers finish
    // in whatever order the blocking pool produces.
    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<RemoteFrame>(WRITER_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(frame) = writer_rx.recv().await {
            if frames::write_json(&mut send, &frame).await.is_err() {
                break;
            }
        }
    });

    let session = Arc::new(RemoteSession::new(access.device_name, access.read_only));
    writer_tx
        .send(RemoteFrame::Ready {
            api_version: REMOTE_PROTOCOL_VERSION,
            app: format!("qmux/{}", env!("CARGO_PKG_VERSION")),
            mac_name: hostname(),
            read_only: session.read_only,
        })
        .await
        .map_err(|_| "writer task ended before ready".to_string())?;

    let limiter = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CALLS));
    let result = loop {
        match frames::read_json(&mut recv).await {
            Ok(None) => break Ok(()),
            Ok(Some(RemoteFrame::Call {
                seq,
                operation,
                arguments,
            })) => {
                let permit = limiter
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| "session limiter closed".to_string())?;
                let state = state.clone();
                let session = session.clone();
                let writer_tx = writer_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let response =
                        crate::control::handle_remote_call(&state, &session, &operation, arguments);
                    let _ = writer_tx.blocking_send(RemoteFrame::CallResult { seq, response });
                });
            }
            // Subscriptions and pane streams land in stage 3.
            Ok(Some(other)) => {
                break Err(format!(
                    "unexpected frame on the control stream: {}",
                    frame_name(&other)
                ));
            }
            Err(err) => break Err(err),
        }
    };

    drop(writer_tx);
    let _ = writer.await;
    result
}

fn frame_name(frame: &RemoteFrame) -> &'static str {
    match frame {
        RemoteFrame::Hello { .. } => "hello",
        RemoteFrame::Ready { .. } => "ready",
        RemoteFrame::Call { .. } => "call",
        RemoteFrame::CallResult { .. } => "callResult",
        RemoteFrame::Subscribe { .. } => "subscribe",
        RemoteFrame::EventStreamHeader {} => "eventStreamHeader",
        RemoteFrame::Event { .. } => "event",
        RemoteFrame::Resync {} => "resync",
        RemoteFrame::PaneHeader { .. } => "paneHeader",
        RemoteFrame::PairRequest { .. } => "pairRequest",
        RemoteFrame::PairResult { .. } => "pairResult",
        RemoteFrame::GoingAway { .. } => "goingAway",
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this Mac".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::endpoint::{
        DeviceGate, RemoteControlRuntime, RemoteReach, tests::loopback_addr,
    };
    use crate::state::test_support;
    use iroh::endpoint::presets;
    use iroh::{Endpoint, RelayMode, SecretKey};
    use qmux_proto::remote::REMOTE_ALPN;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    fn fixture_state(name: &str) -> AppState {
        let state = AppState::new(test_support::config(PathBuf::from(format!(
            "/tmp/qmux-remote-session-{name}"
        ))));
        state
            .insert_group_after(test_support::group("group-1"), None)
            .unwrap();
        state
            .insert_group_after(test_support::group("group-2"), Some("group-1"))
            .unwrap();
        state
            .insert_pane(test_support::pane_runtime("pane-1", "group-1"))
            .unwrap();
        state
            .insert_pane(test_support::pane_runtime("pane-2", "group-2"))
            .unwrap();
        state
    }

    fn gate_for(id: iroh::EndpointId, read_only: bool) -> DeviceGate {
        Arc::new(move |remote| {
            (*remote == id).then(|| RemoteAccess {
                device_name: "test iphone".to_string(),
                read_only,
            })
        })
    }

    async fn client_endpoint() -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("bind client endpoint")
    }

    struct Client {
        connection: Connection,
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        _endpoint: Endpoint,
    }

    async fn connect_and_hello(runtime: &RemoteControlRuntime) -> (Client, RemoteFrame) {
        let endpoint = client_endpoint().await;
        let connection = endpoint
            .connect(loopback_addr(runtime.endpoint()), REMOTE_ALPN)
            .await
            .expect("connect");
        let (mut send, mut recv) = connection.open_bi().await.expect("open control stream");
        frames::write_json(
            &mut send,
            &RemoteFrame::Hello {
                api_version: REMOTE_PROTOCOL_VERSION,
                client: "qmux-test/0".to_string(),
                device_name: None,
            },
        )
        .await
        .expect("send hello");
        let ready = frames::read_json(&mut recv)
            .await
            .expect("read ready")
            .expect("ready frame");
        (
            Client {
                connection,
                send,
                recv,
                _endpoint: endpoint,
            },
            ready,
        )
    }

    async fn call(client: &mut Client, seq: u64, operation: &str, arguments: Value) -> Value {
        frames::write_json(
            &mut client.send,
            &RemoteFrame::Call {
                seq,
                operation: operation.to_string(),
                arguments,
            },
        )
        .await
        .expect("send call");
        loop {
            let frame = frames::read_json(&mut client.recv)
                .await
                .expect("read result")
                .expect("result frame");
            match frame {
                RemoteFrame::CallResult {
                    seq: result_seq,
                    response,
                } if result_seq == seq => return response,
                RemoteFrame::CallResult { .. } => continue,
                other => panic!("unexpected frame while waiting for result: {other:?}"),
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    #[test]
    fn a_paired_device_lists_panes_and_keeps_its_own_focus() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("roundtrip");
        let secret = SecretKey::generate();
        let client_secret = SecretKey::generate();
        let server = RemoteControlRuntime::start(
            state,
            secret,
            RemoteReach::Local,
            false,
            gate_for(client_secret.public(), false),
        )
        .expect("start runtime");

        runtime().block_on(async {
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(client_secret.clone())
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
                .expect("bind client");
            let connection = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("connect");
            let (mut send, mut recv) = connection.open_bi().await.expect("open control stream");
            frames::write_json(
                &mut send,
                &RemoteFrame::Hello {
                    api_version: REMOTE_PROTOCOL_VERSION,
                    client: "qmux-test/0".to_string(),
                    device_name: None,
                },
            )
            .await
            .expect("send hello");
            let ready = frames::read_json(&mut recv)
                .await
                .expect("read ready")
                .expect("ready frame");
            let RemoteFrame::Ready { read_only, app, .. } = ready else {
                panic!("expected ready, got {ready:?}");
            };
            assert!(!read_only);
            assert!(app.starts_with("qmux/"), "unexpected app: {app}");

            let mut client = Client {
                connection,
                send,
                recv,
                _endpoint: endpoint,
            };
            let panes = call(&mut client, 1, "pane.list", Value::Null).await;
            assert_eq!(panes["ok"], true, "pane.list failed: {panes}");
            assert_eq!(panes["result"]["count"], 2);

            let focus = call(&mut client, 2, "session.focus", json!({ "id": "pane-2" })).await;
            assert_eq!(focus["ok"], true, "session.focus failed: {focus}");
            let current = call(&mut client, 3, "pane.current", Value::Null).await;
            assert_eq!(current["result"]["pane"]["id"], "pane-2");

            client.connection.close(0u32.into(), b"done");
        });
        server.shutdown();
    }

    #[test]
    fn an_unpaired_endpoint_is_closed_before_any_frame() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("unpaired");
        let allowed = SecretKey::generate();
        let server = RemoteControlRuntime::start(
            state,
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            gate_for(allowed.public(), false),
        )
        .expect("start runtime");

        runtime().block_on(async {
            // A different key than the gate allows.
            let endpoint = client_endpoint().await;
            let connection = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("quic connect succeeds; refusal is at the gate");
            let (mut send, mut recv) = connection.open_bi().await.expect("open stream");
            let _ = frames::write_json(
                &mut send,
                &RemoteFrame::Hello {
                    api_version: REMOTE_PROTOCOL_VERSION,
                    client: "qmux-test/0".to_string(),
                    device_name: None,
                },
            )
            .await;
            let outcome = frames::read_json(&mut recv).await;
            assert!(
                !matches!(outcome, Ok(Some(_))),
                "an unpaired endpoint must never receive a frame, got {outcome:?}"
            );
        });
        server.shutdown();
    }

    #[test]
    fn read_only_access_reaches_ready_but_not_writes() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("read-only");
        let client_secret = SecretKey::generate();
        let server = RemoteControlRuntime::start(
            state,
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            gate_for(client_secret.public(), true),
        )
        .expect("start runtime");

        runtime().block_on(async {
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(client_secret.clone())
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
                .expect("bind client");
            let connection = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("connect");
            let (mut send, mut recv) = connection.open_bi().await.expect("open control stream");
            frames::write_json(
                &mut send,
                &RemoteFrame::Hello {
                    api_version: REMOTE_PROTOCOL_VERSION,
                    client: "qmux-test/0".to_string(),
                    device_name: None,
                },
            )
            .await
            .expect("send hello");
            let ready = frames::read_json(&mut recv)
                .await
                .expect("read ready")
                .expect("ready frame");
            assert!(
                matches!(
                    ready,
                    RemoteFrame::Ready {
                        read_only: true,
                        ..
                    }
                ),
                "expected read-only ready, got {ready:?}"
            );

            let mut client = Client {
                connection,
                send,
                recv,
                _endpoint: endpoint,
            };
            let denied = call(
                &mut client,
                1,
                "pane.send",
                json!({ "id": "pane-1", "text": "echo hi" }),
            )
            .await;
            assert_eq!(denied["ok"], false);
            assert_eq!(denied["error"]["code"], "permission_denied");
        });
        server.shutdown();
    }

    #[test]
    fn a_wrong_protocol_version_gets_going_away() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("version");
        let client_secret = SecretKey::generate();
        let server = RemoteControlRuntime::start(
            state,
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            gate_for(client_secret.public(), false),
        )
        .expect("start runtime");

        runtime().block_on(async {
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(client_secret.clone())
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
                .expect("bind client");
            let connection = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("connect");
            let (mut send, mut recv) = connection.open_bi().await.expect("open control stream");
            frames::write_json(
                &mut send,
                &RemoteFrame::Hello {
                    api_version: 99,
                    client: "qmux-future/9".to_string(),
                    device_name: None,
                },
            )
            .await
            .expect("send hello");
            let frame = frames::read_json(&mut recv)
                .await
                .expect("read frame")
                .expect("frame");
            assert!(
                matches!(frame, RemoteFrame::GoingAway { .. }),
                "expected goingAway, got {frame:?}"
            );
        });
        server.shutdown();
    }
}

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

use crate::remote::endpoint::{
    CLOSE_GOING_AWAY, CLOSE_PROTOCOL_ERROR, RemoteAccess, RemoteRequestSequence,
};
use crate::remote::fanout::{PaneChannel, SessionChannels};
use crate::remote::frames;
use crate::state::AppState;
use iroh::endpoint::{Connection, SendStream};
use qmux_proto::remote::{
    FRAME_TAG_PANE_BYTES, FRAME_TAG_PANE_RESET, MAX_PANE_FRAME_BYTES, REMOTE_PROTOCOL_VERSION,
    RemoteFrame,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long the client has to send `Hello` before the connection is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// In-flight long waits per session. Ordinary calls are ordered; wait calls
/// detach after all earlier calls complete so they cannot block later control.
const MAX_CONCURRENT_WAITS: usize = 8;
/// Outbound frames buffered for the writer task before call handlers block.
const WRITER_QUEUE: usize = 64;

struct QueuedCall {
    seq: u64,
    operation: String,
    arguments: serde_json::Value,
}

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
    request_sequence: Arc<RemoteRequestSequence>,
) {
    let device = access.device_name.clone();
    match run_control_stream(state, access, &connection, request_sequence).await {
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
    request_sequence: Arc<RemoteRequestSequence>,
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

    // Register with the fan-out for the life of the session. Dormant until
    // the first Subscribe: no events queue and no pane has a ring.
    let (fanout_id, channels) = state.remote_fanout().register_session();
    let mut pumps = Pumps {
        state: state.clone(),
        connection: connection.clone(),
        channels: channels.clone(),
        events: None,
        panes: HashMap::new(),
    };

    let (call_tx, mut call_rx) = tokio::sync::mpsc::channel::<QueuedCall>(WRITER_QUEUE);
    let call_state = state.clone();
    let call_session = session.clone();
    let call_writer = writer_tx.clone();
    let wait_limiter = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_WAITS));
    let call_worker = tokio::spawn(async move {
        while let Some(call) = call_rx.recv().await {
            if is_long_wait(&call.operation) {
                let state = call_state.clone();
                let session = call_session.clone();
                let writer = call_writer.clone();
                let limiter = wait_limiter.clone();
                tokio::spawn(async move {
                    let Ok(_permit) = limiter.acquire_owned().await else {
                        return;
                    };
                    let response =
                        execute_call(state, session, call.operation, call.arguments).await;
                    let _ = writer
                        .send(RemoteFrame::CallResult {
                            seq: call.seq,
                            response,
                        })
                        .await;
                });
                continue;
            }

            let response =
                if is_write_operation(&call.operation) && !request_sequence.claim_write(call.seq) {
                    sequence_error("duplicate or stale mutating request sequence")
                } else {
                    execute_call(
                        call_state.clone(),
                        call_session.clone(),
                        call.operation,
                        call.arguments,
                    )
                    .await
                };
            if call_writer
                .send(RemoteFrame::CallResult {
                    seq: call.seq,
                    response,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut last_seq = None;
    let result = loop {
        match frames::read_json(&mut recv).await {
            Ok(None) => break Ok(()),
            Ok(Some(RemoteFrame::Call {
                seq,
                operation,
                arguments,
            })) => {
                if last_seq.is_some_and(|last| seq <= last) {
                    writer_tx
                        .send(RemoteFrame::CallResult {
                            seq,
                            response: sequence_error(
                                "request sequences must increase within a connection",
                            ),
                        })
                        .await
                        .map_err(|_| "writer task ended while rejecting sequence".to_string())?;
                    continue;
                }
                last_seq = Some(seq);
                call_tx
                    .send(QueuedCall {
                        seq,
                        operation,
                        arguments,
                    })
                    .await
                    .map_err(|_| "ordered call worker ended".to_string())?;
            }
            Ok(Some(RemoteFrame::Subscribe { events, panes })) => {
                pumps.apply(events, panes).await;
            }
            Ok(Some(other)) => {
                break Err(format!(
                    "unexpected frame on the control stream: {}",
                    frame_name(&other)
                ));
            }
            Err(err) => break Err(err),
        }
    };

    drop(call_tx);
    let _ = call_worker.await;
    state.remote_fanout().unregister_session(fanout_id);
    pumps.stop();
    drop(writer_tx);
    let _ = writer.await;
    result
}

async fn execute_call(
    state: AppState,
    session: Arc<RemoteSession>,
    operation: String,
    arguments: serde_json::Value,
) -> serde_json::Value {
    tokio::task::spawn_blocking(move || {
        crate::control::handle_remote_call(&state, &session, &operation, arguments)
    })
    .await
    .unwrap_or_else(|_| sequence_error("remote call worker panicked"))
}

fn is_long_wait(operation: &str) -> bool {
    matches!(operation, "pane.waitOutput" | "agent.wait")
}

fn is_write_operation(operation: &str) -> bool {
    matches!(
        operation,
        "session.focus"
            | "workspace.create"
            | "workspace.rename"
            | "pane.create"
            | "pane.send"
            | "pane.run"
            | "pane.rename"
            | "pane.focus"
            | "pane.close"
            | "agent.start"
            | "agent.fork"
            | "agent.prompt"
            | "agent.submit"
            | "agent.permission"
            | "agent.queue.remove"
            | "agent.queue.reorder"
            | "agent.queue.sendNext"
            | "agent.queue.pause"
            | "agent.queue.unpause"
            | "agent.focus"
            | "agent.release"
            | "artifact.open"
            | "split.join"
            | "split.leave"
            | "split.resize"
    )
}

fn sequence_error(message: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "apiVersion": qmux_proto::PUBLIC_API_VERSION,
        "result": null,
        "error": {
            "code": "stale_sequence",
            "message": message,
            "details": null,
        }
    })
}

/// The pump tasks one session owns: at most one event stream, one byte
/// stream per subscribed pane. `Subscribe` frames are declarative — the set
/// named in the latest frame is the set that runs.
struct Pumps {
    state: AppState,
    connection: Connection,
    channels: Arc<SessionChannels>,
    events: Option<JoinHandle<()>>,
    panes: HashMap<String, JoinHandle<()>>,
}

impl Pumps {
    async fn apply(&mut self, events: bool, panes: Vec<String>) {
        self.channels.set_events_on(events);
        if events && self.events.is_none() {
            self.events = Some(tokio::spawn(event_pump(
                self.connection.clone(),
                self.channels.clone(),
            )));
        } else if !events && let Some(task) = self.events.take() {
            task.abort();
        }

        let live_panes: std::collections::HashSet<String> = self
            .state
            .list_panes()
            .map(|list| list.into_iter().map(|pane| pane.id).collect())
            .unwrap_or_default();
        let desired: std::collections::HashSet<String> = panes
            .into_iter()
            // A pane that raced pane.removed is skipped, not an error: the
            // client learns from the event stream and re-subscribes.
            .filter(|id| live_panes.contains(id))
            .collect();
        let current: Vec<String> = self.panes.keys().cloned().collect();
        for pane_id in current {
            if !desired.contains(&pane_id) {
                self.channels.unregister_pane(&pane_id);
                if let Some(task) = self.panes.remove(&pane_id) {
                    task.abort();
                }
            }
        }
        for pane_id in desired {
            if self.panes.contains_key(&pane_id) {
                continue;
            }
            let channel = self.channels.register_pane(&pane_id);
            self.panes.insert(
                pane_id.clone(),
                tokio::spawn(pane_pump(
                    self.state.clone(),
                    self.connection.clone(),
                    pane_id,
                    channel,
                )),
            );
        }
    }

    fn stop(&mut self) {
        if let Some(task) = self.events.take() {
            task.abort();
        }
        for (_, task) in self.panes.drain() {
            task.abort();
        }
    }
}

async fn event_pump(connection: Connection, channels: Arc<SessionChannels>) {
    let Ok(mut send) = connection.open_uni().await else {
        return;
    };
    if frames::write_json(&mut send, &RemoteFrame::EventStreamHeader {})
        .await
        .is_err()
    {
        return;
    }
    loop {
        let (events, resync) = channels.drain_events();
        if resync
            && frames::write_json(&mut send, &RemoteFrame::Resync {})
                .await
                .is_err()
        {
            return;
        }
        for event in events {
            if frames::write_json(
                &mut send,
                &RemoteFrame::Event {
                    event: (*event).clone(),
                },
            )
            .await
            .is_err()
            {
                return;
            }
        }
        channels.events_notify.notified().await;
    }
}

async fn pane_pump(
    state: AppState,
    connection: Connection,
    pane_id: String,
    channel: Arc<PaneChannel>,
) {
    let Ok(mut send) = connection.open_uni().await else {
        return;
    };
    let (rows, cols) = state
        .list_panes()
        .ok()
        .and_then(|panes| panes.into_iter().find(|pane| pane.id == pane_id))
        .map(|pane| (Some(pane.rows), Some(pane.cols)))
        .unwrap_or((None, None));
    if frames::write_json(
        &mut send,
        &RemoteFrame::PaneHeader {
            id: pane_id.clone(),
            rows,
            cols,
        },
    )
    .await
    .is_err()
    {
        return;
    }
    // Prime with the full sanitized replay before any live bytes.
    if prime_from_journal(&state, &pane_id, &mut send, &channel)
        .await
        .is_err()
    {
        return;
    }
    loop {
        let (data, gapped) = channel.drain();
        if gapped {
            // The ring overflowed: the buffered fragment is incomplete, so
            // reset the client and re-prime from the durable journal instead
            // of forwarding it.
            if frames::write_frame(&mut send, FRAME_TAG_PANE_RESET, &[])
                .await
                .is_err()
                || prime_from_journal(&state, &pane_id, &mut send, &channel)
                    .await
                    .is_err()
            {
                return;
            }
            continue;
        }
        if !data.is_empty() && write_pane_bytes(&mut send, &data).await.is_err() {
            return;
        }
        channel.notify.notified().await;
    }
}

/// Reads and sanitizes the pane's journal off the async threads, sends it, then
/// drains live bytes accumulated across the snapshot seam. A chunk may appear
/// in both sources, which terminals tolerate; dropping the ring would instead
/// lose a chunk published just before its journal append. If the ring gapped
/// while replaying, reset and repeat until one complete seam is obtained.
async fn prime_from_journal(
    state: &AppState,
    pane_id: &str,
    send: &mut SendStream,
    channel: &Arc<PaneChannel>,
) -> Result<(), String> {
    loop {
        let root = state.config().workspace_root.clone();
        let pane = pane_id.to_string();
        let replay = tokio::task::spawn_blocking(move || {
            crate::scrollback::read_pane_scrollback(&root, &pane)
                .map(|raw| crate::scrollback::sanitize_scrollback_replay(&raw))
        })
        .await
        .map_err(|err| format!("journal read task failed: {err}"))?
        .unwrap_or_default();
        write_pane_bytes(send, &replay).await?;
        let (pending, gapped) = channel.drain();
        if gapped {
            frames::write_frame(send, FRAME_TAG_PANE_RESET, &[]).await?;
            continue;
        }
        return write_pane_bytes(send, &pending).await;
    }
}

async fn write_pane_bytes(send: &mut SendStream, data: &[u8]) -> Result<(), String> {
    for chunk in data.chunks(MAX_PANE_FRAME_BYTES as usize) {
        frames::write_frame(send, FRAME_TAG_PANE_BYTES, chunk).await?;
    }
    Ok(())
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

pub(crate) fn hostname() -> String {
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
        DeviceGate, RemoteControlRuntime, RemoteReach,
        tests::{connected_pair, loopback_addr},
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

    async fn send_call(client: &mut Client, seq: u64, operation: &str, arguments: Value) {
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
    }

    async fn read_call_result(client: &mut Client) -> (u64, Value) {
        loop {
            let frame = frames::read_json(&mut client.recv)
                .await
                .expect("read result")
                .expect("result frame");
            if let RemoteFrame::CallResult { seq, response } = frame {
                return (seq, response);
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
    fn pipelined_calls_preserve_order_and_duplicate_sequences_do_not_execute() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("ordered-calls");
        let server = RemoteControlRuntime::start(
            state,
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            Arc::new(|_| {
                Some(RemoteAccess {
                    device_name: "test iphone".to_string(),
                    read_only: false,
                })
            }),
        )
        .expect("start runtime");

        runtime().block_on(async {
            let (mut client, ready) = connect_and_hello(&server).await;
            assert!(matches!(ready, RemoteFrame::Ready { .. }));

            send_call(&mut client, 1, "session.focus", json!({ "id": "pane-2" })).await;
            send_call(&mut client, 2, "pane.current", Value::Null).await;
            let (first_seq, first) = read_call_result(&mut client).await;
            let (second_seq, second) = read_call_result(&mut client).await;
            assert_eq!(first_seq, 1);
            assert_eq!(first["ok"], true);
            assert_eq!(second_seq, 2);
            assert_eq!(second["result"]["pane"]["id"], "pane-2");

            send_call(&mut client, 3, "session.focus", json!({ "id": "pane-1" })).await;
            send_call(&mut client, 3, "session.focus", json!({ "id": "pane-2" })).await;
            let duplicate_results = [
                read_call_result(&mut client).await.1,
                read_call_result(&mut client).await.1,
            ];
            assert_eq!(
                duplicate_results
                    .iter()
                    .filter(|response| response["ok"] == true)
                    .count(),
                1
            );
            assert!(
                duplicate_results
                    .iter()
                    .any(|response| { response["error"]["code"] == "stale_sequence" })
            );
            let current = call(&mut client, 4, "pane.current", Value::Null).await;
            assert_eq!(current["result"]["pane"]["id"], "pane-1");
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
    fn authorization_is_rechecked_after_session_registration() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("authorization-race");
        let client_secret = SecretKey::generate();
        let client_id = client_secret.public();
        let gate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate: DeviceGate = {
            let gate_calls = gate_calls.clone();
            Arc::new(move |remote| {
                (*remote == client_id
                    && gate_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0)
                    .then(|| RemoteAccess {
                        device_name: "revoked iphone".to_string(),
                        read_only: false,
                    })
            })
        };
        let server = RemoteControlRuntime::start(
            state,
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            gate,
        )
        .expect("start runtime");

        runtime().block_on(async {
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(client_secret)
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
                .expect("bind client");
            let connection = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("connect");
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
                "authorization removed before registration must receive no frame: {outcome:?}"
            );
        });
        assert!(gate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
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
    fn subscribing_streams_events_and_primed_pane_bytes() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("streams");
        // Durable output that predates the subscription: the stream must be
        // primed with it before any live bytes.
        crate::scrollback::append_pane_scrollback(
            &state.config().workspace_root,
            "pane-1",
            b"seed-output\r\n",
        )
        .unwrap();
        let client_secret = SecretKey::generate();
        let server = RemoteControlRuntime::start(
            state.clone(),
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
                    api_version: REMOTE_PROTOCOL_VERSION,
                    client: "qmux-test/0".to_string(),
                    device_name: None,
                },
            )
            .await
            .expect("send hello");
            let _ready = frames::read_json(&mut recv)
                .await
                .expect("ready")
                .expect("frame");

            frames::write_json(
                &mut send,
                &RemoteFrame::Subscribe {
                    events: true,
                    panes: vec!["pane-1".to_string(), "pane-gone".to_string()],
                },
            )
            .await
            .expect("send subscribe");

            // Two server-opened uni streams arrive in either order; the
            // header frame identifies each.
            let mut event_stream = None;
            let mut pane_stream = None;
            for _ in 0..2 {
                let mut uni =
                    tokio::time::timeout(Duration::from_secs(10), connection.accept_uni())
                        .await
                        .expect("timed out waiting for a stream")
                        .expect("accept uni");
                let header = frames::read_json(&mut uni)
                    .await
                    .expect("read header")
                    .expect("header frame");
                match header {
                    RemoteFrame::EventStreamHeader {} => event_stream = Some(uni),
                    RemoteFrame::PaneHeader { id, rows, cols } => {
                        assert_eq!(id, "pane-1");
                        assert_eq!(rows, Some(24));
                        assert_eq!(cols, Some(80));
                        pane_stream = Some(uni);
                    }
                    other => panic!("unexpected header: {other:?}"),
                }
            }
            let mut event_stream = event_stream.expect("event stream");
            let mut pane_stream = pane_stream.expect("pane stream");

            // Prime: the journal's seed output arrives first.
            let mut primed = Vec::new();
            while !contains(&primed, b"seed-output") {
                let frame = tokio::time::timeout(
                    Duration::from_secs(10),
                    frames::read_frame(&mut pane_stream, MAX_PANE_FRAME_BYTES),
                )
                .await
                .expect("timed out waiting for prime")
                .expect("read prime")
                .expect("prime frame");
                assert_eq!(frame.tag, FRAME_TAG_PANE_BYTES);
                primed.extend(frame.payload);
            }

            // Live bytes flow after the prime.
            state
                .remote_fanout()
                .publish_pane_bytes("pane-1", b"live-bytes");
            let mut live = Vec::new();
            while !contains(&live, b"live-bytes") {
                let frame = tokio::time::timeout(
                    Duration::from_secs(10),
                    frames::read_frame(&mut pane_stream, MAX_PANE_FRAME_BYTES),
                )
                .await
                .expect("timed out waiting for live bytes")
                .expect("read live")
                .expect("live frame");
                assert_eq!(frame.tag, FRAME_TAG_PANE_BYTES);
                live.extend(frame.payload);
            }

            // Events mirror AppState::emit.
            state.emit(crate::events::QmuxEvent::new(
                "agent.status",
                Some("pane-1".to_string()),
                None,
                json!({ "status": "running" }),
            ));
            let frame = tokio::time::timeout(
                Duration::from_secs(10),
                frames::read_json(&mut event_stream),
            )
            .await
            .expect("timed out waiting for event")
            .expect("read event")
            .expect("event frame");
            let RemoteFrame::Event { event } = frame else {
                panic!("expected event, got {frame:?}");
            };
            assert_eq!(event["type"], "agent.status");

            // Unsubscribing tears the streams down.
            frames::write_json(
                &mut send,
                &RemoteFrame::Subscribe {
                    events: false,
                    panes: vec![],
                },
            )
            .await
            .expect("send unsubscribe");
            let ended = tokio::time::timeout(
                Duration::from_secs(10),
                frames::read_frame(&mut pane_stream, MAX_PANE_FRAME_BYTES),
            )
            .await
            .expect("timed out waiting for stream end");
            assert!(
                !matches!(ended, Ok(Some(_))),
                "the pane stream must end after unsubscribe, got {ended:?}"
            );

            connection.close(0u32.into(), b"done");
        });
        server.shutdown();
    }

    #[test]
    fn pane_prime_preserves_live_bytes_outside_the_journal_snapshot() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("prime-seam");
        crate::scrollback::append_pane_scrollback(
            &state.config().workspace_root,
            "pane-1",
            b"journal-seed",
        )
        .unwrap();
        let (fanout_id, channels) = state.remote_fanout().register_session();
        let channel = channels.register_pane("pane-1");
        state
            .remote_fanout()
            .publish_pane_bytes("pane-1", b"live-seam");

        runtime().block_on(async {
            let (client, server, _guard) = connected_pair().await;
            let mut send = server.open_uni().await.expect("open stream");
            let receive = client.accept_uni();
            prime_from_journal(&state, "pane-1", &mut send, &channel)
                .await
                .expect("prime");
            send.finish().expect("finish stream");
            let mut recv = receive.await.expect("accept stream");
            let mut bytes = Vec::new();
            while let Some(frame) = frames::read_frame(&mut recv, MAX_PANE_FRAME_BYTES)
                .await
                .expect("read frame")
            {
                assert_eq!(frame.tag, FRAME_TAG_PANE_BYTES);
                bytes.extend(frame.payload);
            }
            assert!(contains(&bytes, b"journal-seed"));
            assert!(contains(&bytes, b"live-seam"));
        });
        state.remote_fanout().unregister_session(fanout_id);
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
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

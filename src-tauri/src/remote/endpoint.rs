//! Building and tearing down the iroh endpoint.
//!
//! Off means absent: when remote control is off no endpoint exists — there
//! is no socket to scan, no discovery record, no relay connection. The two
//! reach modes map onto endpoint configuration:
//!
//! - **Local**: relays disabled, mDNS discovery only. Reachable on this
//!   network, invisible beyond it.
//! - **Anywhere**: n0's relay servers plus DNS/pkarr publishing. The
//!   endpoint id becomes linkable to this Mac's addresses, which is why the
//!   mode is a separately confirmed switch.

use crate::state::AppState;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};
use qmux_proto::remote::{PAIR_ALPN, REMOTE_ALPN};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// How far the endpoint reaches; `Local` is the default when the toggle
/// turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteReach {
    Local,
    Anywhere,
}

/// Binds the remote-control endpoint for the given reach. The caller owns
/// closing it; nothing here outlives the returned handle. `discovery: false`
/// binds with no lookup services at all — tests and the dev probe connect by
/// explicit address and must not touch mDNS or anything beyond loopback.
pub async fn bind_endpoint(
    secret: SecretKey,
    reach: RemoteReach,
    discovery: bool,
) -> Result<Endpoint, String> {
    let alpns = vec![REMOTE_ALPN.to_vec(), PAIR_ALPN.to_vec()];
    let endpoint = match (reach, discovery) {
        (RemoteReach::Local, true) => {
            // Minimal preset = crypto provider only: no relay servers, no
            // global discovery publishing. mDNS handles LAN peer lookup and
            // never leaves the local network.
            Endpoint::builder(presets::Minimal)
                .secret_key(secret)
                .alpns(alpns)
                .relay_mode(RelayMode::Disabled)
                .address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder())
                .bind()
                .await
        }
        (RemoteReach::Local, false) => {
            Endpoint::builder(presets::Minimal)
                .secret_key(secret)
                .alpns(alpns)
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
        }
        (RemoteReach::Anywhere, _) => {
            // N0 preset: n0's relays plus pkarr/DNS publishing and lookup.
            Endpoint::builder(presets::N0)
                .secret_key(secret)
                .alpns(alpns)
                .bind()
                .await
        }
    };
    endpoint.map_err(|err| format!("failed to bind remote endpoint: {err}"))
}

/// What the accept gate grants a paired endpoint id. `None` from the gate
/// means "not on the paired list": the connection is closed before its first
/// frame is read.
#[derive(Clone, Debug)]
pub struct RemoteAccess {
    pub device_name: String,
    pub read_only: bool,
}

/// Resolves an endpoint id to its access, if paired. Stage 4 backs this with
/// the persisted device list; tests seed it directly.
pub type DeviceGate = Arc<dyn Fn(&EndpointId) -> Option<RemoteAccess> + Send + Sync>;

/// What the pairing panel displays.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingInvite {
    /// Full QR payload: `qmux-pair:v1?node=…&psk=…&name=…&addr=…`.
    pub payload: String,
    /// The typeable short code (the psk).
    pub code: String,
    pub expires_in_ms: u128,
}

/// Minimal percent-encoding for QR payload values (name, addresses).
fn percent_encode(raw: &str) -> String {
    raw.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Application close codes, visible to the peer.
pub const CLOSE_NOT_PAIRED: u32 = 1;
pub const CLOSE_PROTOCOL_ERROR: u32 = 2;
pub const CLOSE_GOING_AWAY: u32 = 3;
pub const CLOSE_NO_PAIRING_WINDOW: u32 = 4;

/// Separate capacity pools prevent unauthenticated pairing peers from starving
/// already-paired control sessions. Handshakes have their own bounded pool;
/// protocol pools reject excess completed connections instead of queueing them.
const MAX_PENDING_HANDSHAKES: usize = 32;
const MAX_REMOTE_SESSIONS: usize = 16;
const MAX_PAIR_CONNECTIONS: usize = 4;

/// Owns the endpoint, its dedicated tokio runtime, and the accept loop.
///
/// The runtime is deliberately private to remote control rather than shared
/// with tauri's: its lifetime is exactly the toggle's, so `shutdown` can
/// prove "off means absent" by dropping the whole thing.
pub struct RemoteControlRuntime {
    endpoint: Endpoint,
    runtime: std::sync::Mutex<Option<tokio::runtime::Runtime>>,
    state: AppState,
    gate: DeviceGate,
    pairing: std::sync::Mutex<Option<crate::remote::pairing::PairingWindow>>,
    pending_pair: std::sync::Mutex<Option<crate::remote::pairing::PendingPair>>,
    sessions: std::sync::Mutex<HashMap<u64, RemoteSessionEntry>>,
    next_session_id: std::sync::atomic::AtomicU64,
    request_sequences: std::sync::Mutex<HashMap<EndpointId, Arc<RemoteRequestSequence>>>,
}

/// Device-scoped high-water mark for mutating request ids. It survives QUIC
/// reconnects for the life of the enabled endpoint, preventing a retry after a
/// lost response from applying the same write twice.
#[derive(Default)]
pub struct RemoteRequestSequence {
    last_write: std::sync::Mutex<Option<u64>>,
}

impl RemoteRequestSequence {
    pub fn claim_write(&self, seq: u64) -> bool {
        let Ok(mut last) = self.last_write.lock() else {
            return false;
        };
        if last.is_some_and(|last| seq <= last) {
            return false;
        }
        *last = Some(seq);
        true
    }
}

/// One live session, for the UI's list and for revocation.
#[derive(Clone)]
pub struct RemoteSessionEntry {
    pub endpoint_id: EndpointId,
    pub device_name: String,
    pub connected_at: u128,
    connection: iroh::endpoint::Connection,
}

impl RemoteControlRuntime {
    pub fn start(
        state: AppState,
        secret: SecretKey,
        reach: RemoteReach,
        discovery: bool,
        gate: DeviceGate,
    ) -> Result<Arc<Self>, String> {
        // Tauri commands run inside its Tokio runtime. Entering this private
        // runtime from there panics, so its blocking lifecycle lives on a
        // plain management thread.
        std::thread::spawn(move || {
            Self::start_on_management_thread(state, secret, reach, discovery, gate)
        })
        .join()
        .map_err(|_| "remote runtime startup thread panicked".to_string())?
    }

    fn start_on_management_thread(
        state: AppState,
        secret: SecretKey,
        reach: RemoteReach,
        discovery: bool,
        gate: DeviceGate,
    ) -> Result<Arc<Self>, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("qmux-remote")
            .enable_all()
            .build()
            .map_err(|err| format!("failed to start remote runtime: {err}"))?;
        let endpoint = runtime.block_on(bind_endpoint(secret, reach, discovery))?;
        let this = Arc::new(Self {
            endpoint,
            runtime: std::sync::Mutex::new(None),
            state,
            gate,
            pairing: std::sync::Mutex::new(None),
            pending_pair: std::sync::Mutex::new(None),
            sessions: std::sync::Mutex::new(HashMap::new()),
            next_session_id: std::sync::atomic::AtomicU64::new(1),
            request_sequences: std::sync::Mutex::new(HashMap::new()),
        });
        runtime.spawn(accept_loop(Arc::downgrade(&this), this.endpoint.clone()));
        *this
            .runtime
            .lock()
            .map_err(|_| "remote runtime lock poisoned".to_string())? = Some(runtime);
        Ok(this)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Opens (or replaces) the pairing window and returns what the UI shows:
    /// the QR payload, the typeable code, and how long they live.
    pub fn begin_pairing(&self) -> Result<PairingInvite, String> {
        let window = crate::remote::pairing::PairingWindow::open()?;
        let code = window.secret().to_string();
        let ttl = window
            .expires_at()
            .saturating_duration_since(std::time::Instant::now());
        let addrs: Vec<String> = self
            .endpoint
            .bound_sockets()
            .into_iter()
            .map(|sock| sock.to_string())
            .collect();
        let mac_name = crate::remote::session::hostname();
        let payload = format!(
            "qmux-pair:v1?node={}&psk={}&name={}{}",
            self.endpoint.id(),
            code,
            percent_encode(&mac_name),
            addrs
                .iter()
                .map(|addr| format!("&addr={}", percent_encode(addr)))
                .collect::<String>(),
        );
        *self
            .pairing
            .lock()
            .map_err(|_| "pairing lock poisoned".to_string())? = Some(window);
        Ok(PairingInvite {
            payload,
            code,
            expires_in_ms: ttl.as_millis(),
        })
    }

    pub fn cancel_pairing(&self) {
        if let Ok(mut window) = self.pairing.lock() {
            *window = None;
        }
    }

    /// Checks a presented secret against the open window, burning it on a
    /// match and closing the window on exhaustion or expiry.
    pub(crate) fn consume_pairing_secret(
        &self,
        presented: &str,
    ) -> crate::remote::pairing::ConsumeOutcome {
        use crate::remote::pairing::ConsumeOutcome;
        let Ok(mut slot) = self.pairing.lock() else {
            return ConsumeOutcome::Expired;
        };
        let Some(window) = slot.as_mut() else {
            return ConsumeOutcome::Expired;
        };
        let outcome = window.consume(presented);
        if !matches!(outcome, ConsumeOutcome::Wrong) {
            // Matched (single use), exhausted, and expired all end the window.
            *slot = None;
        }
        outcome
    }

    pub(crate) fn put_pending_pair(&self, pending: crate::remote::pairing::PendingPair) -> bool {
        match self.pending_pair.lock() {
            Ok(mut slot) if slot.is_none() => {
                *slot = Some(pending);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn clear_pending_pair(&self, request_id: &str) {
        if let Ok(mut slot) = self.pending_pair.lock()
            && slot
                .as_ref()
                .is_some_and(|pending| pending.info.request_id == request_id)
        {
            *slot = None;
        }
    }

    pub fn pending_pair(&self) -> Option<crate::remote::pairing::PendingPairInfo> {
        self.pending_pair
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|pending| pending.info.clone()))
    }

    /// Relays the person's answer to the waiting pairing connection.
    pub fn respond_pair(
        &self,
        request_id: &str,
        approved: bool,
        read_only: bool,
    ) -> Result<(), String> {
        let pending = self
            .pending_pair
            .lock()
            .map_err(|_| "pairing lock poisoned".to_string())?
            .take_if(|pending| pending.info.request_id == request_id)
            .ok_or_else(|| "that pairing request is no longer waiting".to_string())?;
        let _ = pending
            .responder
            .send(crate::remote::pairing::PairDecision {
                approved,
                read_only,
            });
        Ok(())
    }

    pub(crate) fn emit_pair_request(&self, info: &crate::remote::pairing::PendingPairInfo) {
        self.state.emit(crate::events::QmuxEvent::new(
            "remote.pair_request",
            None,
            None,
            serde_json::json!({
                "requestId": info.request_id,
                "deviceName": info.device_name,
                "endpointId": info.endpoint_id,
            }),
        ));
    }

    pub(crate) fn emit_pair_resolved(
        &self,
        info: &crate::remote::pairing::PendingPairInfo,
        approved: bool,
    ) {
        self.state.emit(crate::events::QmuxEvent::new(
            "remote.pair_resolved",
            None,
            None,
            serde_json::json!({
                "requestId": info.request_id,
                "deviceName": info.device_name,
                "endpointId": info.endpoint_id,
                "approved": approved,
            }),
        ));
    }

    pub fn sessions(&self) -> Vec<RemoteSessionEntry> {
        self.sessions
            .lock()
            .map(|sessions| sessions.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Closes every live session for one endpoint id. Pairs with
    /// `devices::revoke`, which removes the durable record; together they
    /// are complete revocation.
    pub fn disconnect_device(&self, endpoint_id: &str) {
        let Ok(sessions) = self.sessions.lock() else {
            return;
        };
        for entry in sessions.values() {
            if entry.endpoint_id.to_string() == endpoint_id {
                entry
                    .connection
                    .close(CLOSE_GOING_AWAY.into(), b"device revoked");
            }
        }
    }

    fn register_session(&self, entry: RemoteSessionEntry) -> u64 {
        let id = self
            .next_session_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(id, entry.clone());
        }
        self.state.emit(crate::events::QmuxEvent::new(
            "remote.session_connected",
            None,
            None,
            serde_json::json!({
                "deviceName": entry.device_name,
                "endpointId": entry.endpoint_id.to_string(),
            }),
        ));
        id
    }

    fn unregister_session(&self, id: u64) {
        let removed = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.remove(&id));
        if let Some(entry) = removed {
            self.state.emit(crate::events::QmuxEvent::new(
                "remote.session_disconnected",
                None,
                None,
                serde_json::json!({
                    "deviceName": entry.device_name,
                    "endpointId": entry.endpoint_id.to_string(),
                }),
            ));
        }
    }

    fn request_sequence(&self, endpoint_id: EndpointId) -> Arc<RemoteRequestSequence> {
        self.request_sequences
            .lock()
            .map(|mut sequences| {
                sequences
                    .entry(endpoint_id)
                    .or_insert_with(|| Arc::new(RemoteRequestSequence::default()))
                    .clone()
            })
            .unwrap_or_else(|_| Arc::new(RemoteRequestSequence::default()))
    }

    /// Closes every session and releases the port and the runtime. After
    /// this returns nothing remote-control is listening, running, or bound.
    pub fn shutdown(&self) {
        let Some(runtime) = self.runtime.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let endpoint = self.endpoint.clone();
        let _ = std::thread::spawn(move || {
            runtime.block_on(endpoint.close());
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        })
        .join();
    }
}

impl Drop for RemoteControlRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn accept_loop(this: Weak<RemoteControlRuntime>, endpoint: Endpoint) {
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES));
    let remote_sessions = Arc::new(tokio::sync::Semaphore::new(MAX_REMOTE_SESSIONS));
    let pair_connections = Arc::new(tokio::sync::Semaphore::new(MAX_PAIR_CONNECTIONS));
    while let Some(incoming) = endpoint.accept().await {
        let Some(this) = this.upgrade() else {
            break;
        };
        let Ok(handshake_permit) = handshakes.clone().acquire_owned().await else {
            break;
        };
        let this = this.clone();
        let remote_sessions = remote_sessions.clone();
        let pair_connections = pair_connections.clone();
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else {
                return;
            };
            drop(handshake_permit);
            let alpn = connection.alpn().to_vec();
            if alpn == REMOTE_ALPN {
                let Ok(_session_permit) = remote_sessions.try_acquire_owned() else {
                    connection.close(CLOSE_GOING_AWAY.into(), b"remote session limit");
                    return;
                };
                let remote = connection.remote_id();
                match (this.gate)(&remote) {
                    Some(access) => {
                        let session_id = this.register_session(RemoteSessionEntry {
                            endpoint_id: remote,
                            device_name: access.device_name.clone(),
                            connected_at: crate::remote::devices::now_millis(),
                            connection: connection.clone(),
                        });
                        // Re-read authorization only after the connection is
                        // visible to disconnect_device. A revoke/read-only
                        // update before registration is caught here; one after
                        // this check necessarily sees and closes the session.
                        if let Some(current_access) = (this.gate)(&remote) {
                            crate::remote::session::serve_remote_connection(
                                this.state.clone(),
                                current_access,
                                connection,
                                this.request_sequence(remote),
                            )
                            .await;
                        } else {
                            connection.close(CLOSE_NOT_PAIRED.into(), b"not paired");
                        }
                        this.unregister_session(session_id);
                    }
                    None => {
                        // Closed before any frame is read: an unpaired node
                        // learns nothing but "refused".
                        connection.close(CLOSE_NOT_PAIRED.into(), b"not paired");
                    }
                }
            } else if alpn == PAIR_ALPN {
                let Ok(_pair_permit) = pair_connections.try_acquire_owned() else {
                    connection.close(CLOSE_GOING_AWAY.into(), b"pairing busy");
                    return;
                };
                crate::remote::pairing::serve_pair_connection(this.clone(), connection).await;
            } else {
                connection.close(CLOSE_PROTOCOL_ERROR.into(), b"unknown protocol");
            }
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use iroh::EndpointAddr;
    use iroh::endpoint::Connection;

    /// Two endpoints in one process, relays and discovery off, connected
    /// over loopback by explicit address — the hermetic harness every
    /// remote test builds on.
    /// Callers must hold `state::test_support::net_serial_guard()` for the
    /// life of the endpoints (see its docs).
    pub(crate) async fn endpoint_pair() -> (Endpoint, Endpoint) {
        let server = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(vec![REMOTE_ALPN.to_vec(), PAIR_ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("bind server endpoint");
        let client = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("bind client endpoint");
        (client, server)
    }

    /// Loopback-only address for a bound endpoint.
    pub(crate) fn loopback_addr(endpoint: &Endpoint) -> EndpointAddr {
        EndpointAddr::new(endpoint.id()).with_addrs(endpoint.bound_sockets().into_iter().map(
            |mut sock| {
                if sock.ip().is_unspecified() {
                    sock.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
                }
                iroh::TransportAddr::Ip(sock)
            },
        ))
    }

    /// One connected REMOTE_ALPN client/server connection pair, plus the
    /// endpoints kept alive for the connection's lifetime.
    pub(crate) async fn connected_pair() -> (Connection, Connection, (Endpoint, Endpoint)) {
        let (client, server) = endpoint_pair().await;
        let addr = loopback_addr(&server);
        let accept = async {
            let incoming = server.accept().await.expect("server endpoint closed");
            incoming.await.expect("accept connection")
        };
        let (client_conn, server_conn) = tokio::join!(
            async { client.connect(addr, REMOTE_ALPN).await.expect("connect") },
            accept
        );
        (client_conn, server_conn, (client, server))
    }

    #[test]
    fn endpoint_binds_and_shuts_down() {
        let _serial = crate::state::test_support::net_serial_guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async {
            let endpoint = bind_endpoint(SecretKey::generate(), RemoteReach::Local, true)
                .await
                .expect("bind local endpoint");
            assert!(
                !endpoint.bound_sockets().is_empty(),
                "a bound endpoint must hold at least one socket"
            );
            assert!(!endpoint.is_closed());
            endpoint.close().await;
            assert!(
                endpoint.is_closed(),
                "close must be complete when it returns"
            );
        });
    }

    #[test]
    fn mutating_sequences_are_monotonic_across_sessions() {
        let sequence = RemoteRequestSequence::default();
        assert!(sequence.claim_write(7));
        assert!(!sequence.claim_write(7));
        assert!(!sequence.claim_write(6));
        assert!(sequence.claim_write(8));
    }
}

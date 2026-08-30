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
use std::sync::Arc;

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

/// Application close codes, visible to the peer.
pub const CLOSE_NOT_PAIRED: u32 = 1;
pub const CLOSE_PROTOCOL_ERROR: u32 = 2;
pub const CLOSE_GOING_AWAY: u32 = 3;
pub const CLOSE_NO_PAIRING_WINDOW: u32 = 4;

/// Ceiling on concurrent remote connections across all devices. Sessions are
/// long-lived, so this bounds devices plus in-flight handshakes, not request
/// rate; at the cap new connections wait in the accept queue.
const MAX_REMOTE_CONNECTIONS: usize = 16;

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
}

impl RemoteControlRuntime {
    pub fn start(
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
        });
        runtime.spawn(accept_loop(this.clone()));
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

    /// Closes every session and releases the port and the runtime. After
    /// this returns nothing remote-control is listening, running, or bound.
    pub fn shutdown(&self) {
        let Some(runtime) = self.runtime.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        runtime.block_on(self.endpoint.close());
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    }
}

impl Drop for RemoteControlRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn accept_loop(this: Arc<RemoteControlRuntime>) {
    let limiter = Arc::new(tokio::sync::Semaphore::new(MAX_REMOTE_CONNECTIONS));
    while let Some(incoming) = this.endpoint.accept().await {
        let Ok(permit) = limiter.clone().acquire_owned().await else {
            break;
        };
        let this = this.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(connection) = incoming.await else {
                return;
            };
            let alpn = connection.alpn().to_vec();
            if alpn == REMOTE_ALPN {
                let remote = connection.remote_id();
                match (this.gate)(&remote) {
                    Some(access) => {
                        crate::remote::session::serve_remote_connection(
                            this.state.clone(),
                            access,
                            connection,
                        )
                        .await;
                    }
                    None => {
                        // Closed before any frame is read: an unpaired node
                        // learns nothing but "refused".
                        connection.close(CLOSE_NOT_PAIRED.into(), b"not paired");
                    }
                }
            } else if alpn == PAIR_ALPN {
                // Stage 4 opens this surface while a pairing window exists.
                connection.close(CLOSE_NO_PAIRING_WINDOW.into(), b"no pairing window");
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
}

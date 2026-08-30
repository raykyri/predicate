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

use iroh::endpoint::presets;
use iroh::{Endpoint, RelayMode, SecretKey};
use qmux_proto::remote::{PAIR_ALPN, REMOTE_ALPN};

/// How far the endpoint reaches; `Local` is the default when the toggle
/// turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteReach {
    Local,
    Anywhere,
}

/// Binds the remote-control endpoint for the given reach. The caller owns
/// closing it; nothing here outlives the returned handle.
pub async fn bind_endpoint(secret: SecretKey, reach: RemoteReach) -> Result<Endpoint, String> {
    let alpns = vec![REMOTE_ALPN.to_vec(), PAIR_ALPN.to_vec()];
    let endpoint = match reach {
        RemoteReach::Local => {
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
        RemoteReach::Anywhere => {
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
            let endpoint = bind_endpoint(SecretKey::generate(), RemoteReach::Local)
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

//! The pairing ceremony: one-time secret, TTL, burn-on-use, approval gate.
//!
//! The QR (or typed code) carries this Mac's endpoint id and a one-time
//! secret. Scanning proves the phone was shown the screen; the secret
//! authenticates the first contact so nobody on the network can race the
//! pairing; and the Mac still asks the person before trusting the device,
//! because a photographed code must not be enough on its own.

use crate::remote::devices::{self, RemotePairedDevice};
use crate::remote::endpoint::{CLOSE_PROTOCOL_ERROR, RemoteControlRuntime};
use crate::remote::frames;
use iroh::EndpointId;
use iroh::endpoint::Connection;
use qmux_proto::remote::{REMOTE_PROTOCOL_VERSION, RemoteFrame};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a pairing code stays scannable.
pub const PAIRING_TTL: Duration = Duration::from_secs(180);
/// Wrong secrets tolerated before the window closes outright.
const MAX_ATTEMPTS: u32 = 3;
/// How long the approval prompt waits for the person at the Mac.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a connecting device has to present its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Unambiguous code alphabet (no 0/O, 1/I/L, U). 10 characters ≈ 49 bits:
/// single-use, three-minute TTL, three attempts.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
const CODE_LENGTH: usize = 10;

/// The open pairing window. One at a time; beginning a new one replaces it.
pub struct PairingWindow {
    secret: String,
    expires_at: Instant,
    attempts: u32,
}

impl PairingWindow {
    pub fn open() -> Result<Self, String> {
        Ok(Self {
            secret: generate_code()?,
            expires_at: Instant::now() + PAIRING_TTL,
            attempts: 0,
        })
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Checks a presented secret. `Ok(true)` burns the window (single use);
    /// `Ok(false)` counts an attempt and reports whether the window
    /// survives; expiry and exhaustion are terminal.
    pub fn consume(&mut self, presented: &str) -> ConsumeOutcome {
        if Instant::now() >= self.expires_at {
            return ConsumeOutcome::Expired;
        }
        if self.attempts >= MAX_ATTEMPTS {
            return ConsumeOutcome::Exhausted;
        }
        self.attempts += 1;
        if constant_time_eq(self.secret.as_bytes(), presented.trim().as_bytes()) {
            ConsumeOutcome::Matched
        } else if self.attempts >= MAX_ATTEMPTS {
            ConsumeOutcome::Exhausted
        } else {
            ConsumeOutcome::Wrong
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsumeOutcome {
    Matched,
    Wrong,
    Exhausted,
    Expired,
}

/// What the UI shows while a request waits for approval.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPairInfo {
    pub request_id: String,
    pub device_name: String,
    pub endpoint_id: String,
}

/// The person's answer, relayed to the waiting connection.
#[derive(Clone, Copy, Debug)]
pub struct PairDecision {
    pub approved: bool,
    pub read_only: bool,
}

pub(crate) struct PendingPair {
    pub info: PendingPairInfo,
    pub responder: tokio::sync::oneshot::Sender<PairDecision>,
}

fn generate_code() -> Result<String, String> {
    let mut bytes = [0_u8; CODE_LENGTH];
    getrandom::getrandom(&mut bytes).map_err(|err| format!("failed to draw randomness: {err}"))?;
    Ok(bytes
        .iter()
        .map(|byte| CODE_ALPHABET[(*byte as usize) % CODE_ALPHABET.len()] as char)
        .collect())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serves one connection on the pairing ALPN to completion.
pub(crate) async fn serve_pair_connection(
    runtime: Arc<RemoteControlRuntime>,
    connection: Connection,
) {
    let remote = connection.remote_id();
    if let Err(reason) = run_pairing(&runtime, &connection, remote).await {
        eprintln!("qmux: pairing attempt from {remote} failed: {reason}");
        connection.close(CLOSE_PROTOCOL_ERROR.into(), reason.as_bytes());
    }
}

async fn run_pairing(
    runtime: &Arc<RemoteControlRuntime>,
    connection: &Connection,
    remote: EndpointId,
) -> Result<(), String> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|err| format!("no pairing stream: {err}"))?;
    let request = tokio::time::timeout(REQUEST_TIMEOUT, frames::read_json(&mut recv))
        .await
        .map_err(|_| "timed out waiting for the pair request".to_string())??
        .ok_or_else(|| "connection closed before the pair request".to_string())?;
    let RemoteFrame::PairRequest {
        api_version,
        secret,
        device_name,
    } = request
    else {
        return Err("expected a pair request as the first frame".to_string());
    };

    let refusal = if api_version != REMOTE_PROTOCOL_VERSION {
        Some("this qmux speaks a different protocol version".to_string())
    } else {
        match runtime.consume_pairing_secret(&secret) {
            ConsumeOutcome::Matched => None,
            ConsumeOutcome::Wrong => Some("that code is not right".to_string()),
            ConsumeOutcome::Exhausted => {
                Some("too many wrong codes; pairing is closed".to_string())
            }
            ConsumeOutcome::Expired => Some("the code expired; get a fresh one".to_string()),
        }
    };
    if let Some(message) = refusal {
        return finish_with(connection, &mut send, false, Some(message)).await;
    }

    // The secret matched and is burned. Ask the person at the Mac.
    let device_name = presentable_name(&device_name);
    let (responder, decision_rx) = tokio::sync::oneshot::channel();
    let info = PendingPairInfo {
        request_id: format!("pair-{}", devices::now_millis()),
        device_name: device_name.clone(),
        endpoint_id: remote.to_string(),
    };
    if !runtime.put_pending_pair(PendingPair {
        info: info.clone(),
        responder,
    }) {
        return finish_with(
            connection,
            &mut send,
            false,
            Some("another device is pairing right now".to_string()),
        )
        .await;
    }
    runtime.emit_pair_request(&info);

    let decision = tokio::time::timeout(APPROVAL_TIMEOUT, decision_rx).await;
    runtime.clear_pending_pair(&info.request_id);
    let decision = match decision {
        Ok(Ok(decision)) => decision,
        // Timeout or a dropped responder both read as "not approved".
        _ => PairDecision {
            approved: false,
            read_only: false,
        },
    };
    if !decision.approved {
        runtime.emit_pair_resolved(&info, false);
        return finish_with(
            connection,
            &mut send,
            false,
            Some("the Mac did not approve this device".to_string()),
        )
        .await;
    }

    devices::add(
        runtime.state(),
        RemotePairedDevice {
            endpoint_id: remote.to_string(),
            name: device_name,
            paired_at: devices::now_millis(),
            last_seen: None,
            read_only: decision.read_only,
        },
    )?;
    runtime.emit_pair_resolved(&info, true);
    finish_with(connection, &mut send, true, None).await
}

/// Writes the result, then drains before closing so the frame is not
/// discarded by the QUIC close.
async fn finish_with(
    connection: &Connection,
    send: &mut iroh::endpoint::SendStream,
    accepted: bool,
    message: Option<String>,
) -> Result<(), String> {
    frames::write_json(
        send,
        &RemoteFrame::PairResult {
            accepted,
            mac_name: accepted.then(crate::remote::session::hostname),
            message,
        },
    )
    .await?;
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(3), connection.closed()).await;
    connection.close(0u32.into(), b"pairing done");
    Ok(())
}

/// Device names arrive from an untrusted peer: bound the length and strip
/// control characters before they reach the approval dialog.
fn presentable_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|character| !character.is_control())
        .take(64)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "Unnamed device".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::devices;
    use crate::remote::endpoint::{RemoteControlRuntime, RemoteReach, tests::loopback_addr};
    use crate::state::{AppState, test_support};
    use iroh::endpoint::presets;
    use iroh::{Endpoint, RelayMode, SecretKey};
    use qmux_proto::remote::{PAIR_ALPN, REMOTE_ALPN};
    use std::path::PathBuf;

    fn fixture_state(name: &str) -> AppState {
        let root = PathBuf::from(format!(
            "/tmp/qmux-remote-pairing-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::new(test_support::config(root));
        state
            .insert_group_after(test_support::group("group-1"), None)
            .unwrap();
        state
            .insert_pane(test_support::pane_runtime("pane-1", "group-1"))
            .unwrap();
        state
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    async fn pair_request(
        server: &RemoteControlRuntime,
        client_secret: &SecretKey,
        secret: &str,
        name: &str,
    ) -> (Endpoint, iroh::endpoint::Connection, RemoteFrame) {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(client_secret.clone())
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("bind client");
        let connection = endpoint
            .connect(loopback_addr(server.endpoint()), PAIR_ALPN)
            .await
            .expect("connect for pairing");
        let (mut send, mut recv) = connection.open_bi().await.expect("open pairing stream");
        frames::write_json(
            &mut send,
            &RemoteFrame::PairRequest {
                api_version: REMOTE_PROTOCOL_VERSION,
                secret: secret.to_string(),
                device_name: name.to_string(),
            },
        )
        .await
        .expect("send pair request");
        let result = frames::read_json(&mut recv)
            .await
            .expect("read pair result")
            .expect("pair result frame");
        (endpoint, connection, result)
    }

    async fn wait_for_pending(server: &RemoteControlRuntime) -> PendingPairInfo {
        for _ in 0..200 {
            if let Some(pending) = server.pending_pair() {
                return pending;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no pairing request surfaced for approval");
    }

    #[test]
    fn a_scanned_secret_pairs_after_mac_approval_and_admits_the_device() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("approve");
        let client_secret = SecretKey::generate();
        let server = RemoteControlRuntime::start(
            state.clone(),
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            devices::gate(state.clone()),
        )
        .expect("start runtime");

        let invite = server.begin_pairing().expect("open pairing window");
        assert!(invite.payload.starts_with("qmux-pair:v1?node="));
        assert!(invite.payload.contains(&format!("psk={}", invite.code)));

        runtime().block_on(async {
            let approver = {
                let server = server.clone();
                tokio::spawn(async move {
                    let pending = wait_for_pending(&server).await;
                    assert_eq!(pending.device_name, "Ray's iPhone");
                    server
                        .respond_pair(&pending.request_id, true, false)
                        .expect("respond");
                })
            };
            let (_endpoint, _connection, result) =
                pair_request(&server, &client_secret, &invite.code, "Ray's iPhone").await;
            approver.await.expect("approver task");
            assert!(
                matches!(result, RemoteFrame::PairResult { accepted: true, .. }),
                "expected acceptance, got {result:?}"
            );

            // The record is durable and the device now passes the gate.
            let listed = devices::list(&state);
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].endpoint_id, client_secret.public().to_string());

            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(client_secret.clone())
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
                .expect("bind client");
            let connection = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("connect as paired device");
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
            assert!(matches!(ready, RemoteFrame::Ready { .. }));

            // The burned secret is dead: a second device with the same code
            // is refused without reaching approval.
            let (_e2, _c2, replay) =
                pair_request(&server, &SecretKey::generate(), &invite.code, "impostor").await;
            match replay {
                RemoteFrame::PairResult {
                    accepted, message, ..
                } => {
                    assert!(!accepted);
                    assert!(message.unwrap_or_default().contains("expired"));
                }
                other => panic!("expected refusal, got {other:?}"),
            }
        });
        server.shutdown();
    }

    #[test]
    fn wrong_codes_and_denials_never_pair() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("deny");
        let server = RemoteControlRuntime::start(
            state.clone(),
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            devices::gate(state.clone()),
        )
        .expect("start runtime");
        let invite = server.begin_pairing().expect("open pairing window");

        runtime().block_on(async {
            // A wrong code is refused and does not burn the window.
            let (_e, _c, wrong) =
                pair_request(&server, &SecretKey::generate(), "WRONGWRONG", "guess").await;
            match wrong {
                RemoteFrame::PairResult {
                    accepted, message, ..
                } => {
                    assert!(!accepted);
                    assert!(message.unwrap_or_default().contains("not right"));
                }
                other => panic!("expected refusal, got {other:?}"),
            }

            // The right code reaches approval; the Mac says no; nothing is
            // persisted.
            let denier = {
                let server = server.clone();
                tokio::spawn(async move {
                    let pending = wait_for_pending(&server).await;
                    server
                        .respond_pair(&pending.request_id, false, false)
                        .expect("respond");
                })
            };
            let (_e2, _c2, denied) =
                pair_request(&server, &SecretKey::generate(), &invite.code, "iPhone").await;
            denier.await.expect("denier task");
            assert!(
                matches!(
                    denied,
                    RemoteFrame::PairResult {
                        accepted: false,
                        ..
                    }
                ),
                "expected denial, got {denied:?}"
            );
            assert!(devices::list(&state).is_empty());
        });
        server.shutdown();
    }

    #[test]
    fn revoking_a_device_disconnects_its_live_session() {
        let _serial = test_support::net_serial_guard();
        let state = fixture_state("revoke");
        let client_secret = SecretKey::generate();
        devices::add(
            &state,
            RemotePairedDevice {
                endpoint_id: client_secret.public().to_string(),
                name: "iPhone".to_string(),
                paired_at: 1,
                last_seen: None,
                read_only: false,
            },
        )
        .unwrap();
        let server = RemoteControlRuntime::start(
            state.clone(),
            SecretKey::generate(),
            RemoteReach::Local,
            false,
            devices::gate(state.clone()),
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
            let _ready = frames::read_json(&mut recv).await.expect("ready");
            for _ in 0..200 {
                if !server.sessions().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(server.sessions().len(), 1, "the session must be listed");

            // Revocation: record gone, live session closed, next connect
            // refused before any frame.
            let id = client_secret.public().to_string();
            assert!(devices::revoke(&state, &id).unwrap());
            server.disconnect_device(&id);
            let closed = tokio::time::timeout(Duration::from_secs(10), connection.closed())
                .await
                .expect("the live session must be closed by revocation");
            let closed = format!("{closed:?}");
            assert!(closed.contains("revoked"), "unexpected close: {closed}");

            let retry = endpoint
                .connect(loopback_addr(server.endpoint()), REMOTE_ALPN)
                .await
                .expect("quic connect still succeeds");
            let (mut send, mut recv) = retry.open_bi().await.expect("open stream");
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
                "a revoked device must never receive a frame, got {outcome:?}"
            );
        });
        server.shutdown();
    }

    #[test]
    fn a_window_burns_on_match_and_closes_after_three_misses() {
        let mut window = PairingWindow::open().unwrap();
        let secret = window.secret().to_string();
        assert_eq!(secret.len(), CODE_LENGTH);
        assert_eq!(window.consume("WRONGWRONG"), ConsumeOutcome::Wrong);
        assert_eq!(window.consume(&secret), ConsumeOutcome::Matched);

        let mut window = PairingWindow::open().unwrap();
        for outcome in [
            ConsumeOutcome::Wrong,
            ConsumeOutcome::Wrong,
            ConsumeOutcome::Exhausted,
        ] {
            assert_eq!(window.consume("NOPENOPENO"), outcome);
        }
        // Even the right code is dead once the window is exhausted.
        let secret = window.secret().to_string();
        assert_eq!(window.consume(&secret), ConsumeOutcome::Exhausted);
    }

    #[test]
    fn an_expired_window_never_matches() {
        let mut window = PairingWindow::open().unwrap();
        window.expires_at = Instant::now() - Duration::from_secs(1);
        let secret = window.secret().to_string();
        assert_eq!(window.consume(&secret), ConsumeOutcome::Expired);
    }

    #[test]
    fn presented_device_names_are_bounded_and_printable() {
        assert_eq!(presentable_name("  Ray's iPhone \u{7}\n"), "Ray's iPhone");
        assert_eq!(presentable_name("\u{0}\u{1}"), "Unnamed device");
        assert_eq!(presentable_name(&"x".repeat(200)).len(), 64);
    }
}

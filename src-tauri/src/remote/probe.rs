//! `qmux remote-probe` — the hidden dev client for the remote protocol.
//!
//! The first client of the remote transport, kept deliberately tiny: connect
//! by explicit endpoint id and address (no discovery, no relays), speak
//! hello/ready, run one public control call, print the JSON response. The
//! integration tests are the real verification; this exists so a person can
//! poke a running qmux from a second terminal.
//!
//! ```text
//! qmux remote-probe --id <endpoint-id> --addr 192.168.1.31:41822 \
//!     [--call pane.list] [--args '{"id":"pane-1"}'] [--secret <hex32>]
//! ```
//!
//! `--secret` reuses a stable client identity (64 hex chars) so the server
//! side can pair it once; omitted, a fresh key is generated per invocation.

use crate::remote::frames;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use qmux_proto::remote::{REMOTE_ALPN, REMOTE_PROTOCOL_VERSION, RemoteFrame};
use serde_json::Value;
use std::str::FromStr;

pub fn run(args: Vec<String>) -> Result<(), String> {
    let mut id: Option<EndpointId> = None;
    let mut addrs: Vec<std::net::SocketAddr> = Vec::new();
    let mut operation = "context".to_string();
    let mut arguments = Value::Null;
    let mut secret: Option<SecretKey> = None;

    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag.as_str() {
            "--id" => {
                id = Some(
                    EndpointId::from_str(&value("--id")?)
                        .map_err(|err| format!("invalid endpoint id: {err}"))?,
                );
            }
            "--addr" => {
                addrs.push(
                    value("--addr")?
                        .parse()
                        .map_err(|err| format!("invalid --addr: {err}"))?,
                );
            }
            "--call" => operation = value("--call")?,
            "--args" => {
                arguments = serde_json::from_str(&value("--args")?)
                    .map_err(|err| format!("--args must be JSON: {err}"))?;
            }
            "--secret" => {
                let hex = value("--secret")?;
                let bytes = decode_hex32(&hex)?;
                secret = Some(SecretKey::from_bytes(&bytes));
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let id = id.ok_or_else(|| "--id <endpoint-id> is required".to_string())?;
    if addrs.is_empty() {
        return Err(
            "at least one --addr ip:port is required (the probe uses no discovery)".to_string(),
        );
    }
    let secret = secret.unwrap_or_else(SecretKey::generate);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start runtime: {err}"))?;
    runtime.block_on(async move {
        eprintln!("probe   client id {}", secret.public());
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .map_err(|err| format!("failed to bind: {err}"))?;
        let mut addr = EndpointAddr::new(id);
        for sock in addrs {
            addr = addr.with_ip_addr(sock);
        }
        let connection = endpoint
            .connect(addr, REMOTE_ALPN)
            .await
            .map_err(|err| format!("connect failed: {err}"))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|err| format!("failed to open control stream: {err}"))?;
        frames::write_json(
            &mut send,
            &RemoteFrame::Hello {
                api_version: REMOTE_PROTOCOL_VERSION,
                client: format!("qmux-probe/{}", env!("CARGO_PKG_VERSION")),
                device_name: Some("remote-probe".to_string()),
            },
        )
        .await?;
        let ready = frames::read_json(&mut recv)
            .await?
            .ok_or_else(|| "connection closed before ready (is this device paired?)".to_string())?;
        match ready {
            RemoteFrame::Ready {
                app,
                read_only,
                mac_name,
                ..
            } => eprintln!("ready   {app} on {mac_name} · read_only={read_only}"),
            RemoteFrame::GoingAway { reason } => return Err(format!("refused: {reason}")),
            other => return Err(format!("unexpected frame before ready: {other:?}")),
        }
        frames::write_json(
            &mut send,
            &RemoteFrame::Call {
                seq: 1,
                operation: operation.clone(),
                arguments,
            },
        )
        .await?;
        let result = frames::read_json(&mut recv)
            .await?
            .ok_or_else(|| "connection closed before the result".to_string())?;
        let RemoteFrame::CallResult { response, .. } = result else {
            return Err(format!("unexpected frame: {result:?}"));
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string())
        );
        connection.close(0u32.into(), b"done");
        endpoint.close().await;
        Ok(())
    })
}

fn decode_hex32(hex: &str) -> Result<[u8; 32], String> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("--secret must be 64 hex characters".to_string());
    }
    let mut bytes = [0_u8; 32];
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let value = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16)
            .map_err(|err| format!("invalid hex: {err}"))?;
        bytes[index] = value;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_hex_round_trips_and_rejects_junk() {
        let hex = "aa".repeat(32);
        let bytes = decode_hex32(&hex).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0xaa));
        assert!(decode_hex32("deadbeef").is_err());
        assert!(decode_hex32(&"zz".repeat(32)).is_err());
    }
}

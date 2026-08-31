//! Tagged length-prefixed frames over QUIC streams.
//!
//! Every frame on every remote stream is `[u32 len BE][u8 tag][len-1 bytes]`
//! — the tag is counted in `len`, so `len == 0` is invalid and a frame is
//! never empty. Tag [`FRAME_TAG_JSON`] carries one serialized
//! [`RemoteFrame`]; pane streams additionally carry raw PTY bytes
//! ([`FRAME_TAG_PANE_BYTES`]) and reset markers ([`FRAME_TAG_PANE_RESET`]),
//! which skip JSON entirely because they are the highest-volume path.

use iroh::endpoint::{ReadExactError, RecvStream, SendStream};
use qmux_proto::remote::{FRAME_TAG_JSON, MAX_JSON_FRAME_BYTES, RemoteFrame};

/// One decoded frame.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub tag: u8,
    pub payload: Vec<u8>,
}

/// Reads one frame, enforcing `max_payload` on the payload length. Returns
/// `Ok(None)` on a cleanly finished stream (the peer closed between frames);
/// a stream that ends mid-frame is an error.
pub async fn read_frame(recv: &mut RecvStream, max_payload: u32) -> Result<Option<Frame>, String> {
    let mut header = [0_u8; 5];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(err) => return Err(format!("failed to read frame header: {err}")),
    }
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let tag = header[4];
    if len == 0 {
        return Err("invalid frame: zero length".to_string());
    }
    let payload_len = len - 1;
    if payload_len > max_payload {
        return Err(format!(
            "frame of {payload_len} bytes exceeds the {max_payload}-byte cap"
        ));
    }
    let mut payload = vec![0_u8; payload_len as usize];
    recv.read_exact(&mut payload)
        .await
        .map_err(|err| format!("failed to read frame payload: {err}"))?;
    Ok(Some(Frame { tag, payload }))
}

/// Writes one frame. `payload` must respect the stream's cap; this only
/// guards the arithmetic.
pub async fn write_frame(send: &mut SendStream, tag: u8, payload: &[u8]) -> Result<(), String> {
    let len = u32::try_from(payload.len())
        .ok()
        .and_then(|len| len.checked_add(1))
        .ok_or_else(|| "frame payload too large to encode".to_string())?;
    let mut header = [0_u8; 5];
    header[..4].copy_from_slice(&len.to_be_bytes());
    header[4] = tag;
    send.write_all(&header)
        .await
        .map_err(|err| format!("failed to write frame header: {err}"))?;
    send.write_all(payload)
        .await
        .map_err(|err| format!("failed to write frame payload: {err}"))?;
    Ok(())
}

/// Serializes and writes one JSON frame.
pub async fn write_json(send: &mut SendStream, frame: &RemoteFrame) -> Result<(), String> {
    let payload =
        serde_json::to_vec(frame).map_err(|err| format!("failed to encode remote frame: {err}"))?;
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| "JSON frame payload is too large to encode".to_string())?;
    if payload_len > MAX_JSON_FRAME_BYTES {
        return Err(format!(
            "JSON frame of {payload_len} bytes exceeds the {MAX_JSON_FRAME_BYTES}-byte cap"
        ));
    }
    write_frame(send, FRAME_TAG_JSON, &payload).await
}

/// Reads one frame and requires it to be JSON, parsing it. `Ok(None)` on a
/// cleanly finished stream.
pub async fn read_json(recv: &mut RecvStream) -> Result<Option<RemoteFrame>, String> {
    let Some(frame) = read_frame(recv, MAX_JSON_FRAME_BYTES).await? else {
        return Ok(None);
    };
    if frame.tag != FRAME_TAG_JSON {
        return Err(format!(
            "expected a JSON frame, got tag {} on a control stream",
            frame.tag
        ));
    }
    serde_json::from_slice::<RemoteFrame>(&frame.payload)
        .map(Some)
        .map_err(|err| format!("invalid remote frame: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::endpoint::tests::connected_pair;
    use qmux_proto::remote::MAX_PANE_FRAME_BYTES;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    #[test]
    fn frames_round_trip_across_a_real_stream() {
        let _serial = crate::state::test_support::net_serial_guard();
        runtime().block_on(async {
            let (client, server, _guard) = connected_pair().await;
            let (mut send, _recv) = client.open_bi().await.expect("open bi");
            let accept = server.accept_bi();

            write_json(
                &mut send,
                &RemoteFrame::Call {
                    seq: 1,
                    operation: "ping".to_string(),
                    arguments: serde_json::Value::Null,
                },
            )
            .await
            .expect("write json");
            write_frame(
                &mut send,
                qmux_proto::remote::FRAME_TAG_PANE_BYTES,
                b"\x1b[2J hi",
            )
            .await
            .expect("write bytes");
            send.finish().expect("finish");

            let (_send_back, mut recv) = accept.await.expect("accept bi");
            let first = read_json(&mut recv)
                .await
                .expect("read json")
                .expect("frame");
            assert!(matches!(first, RemoteFrame::Call { seq: 1, .. }));
            let second = read_frame(&mut recv, MAX_PANE_FRAME_BYTES)
                .await
                .expect("read frame")
                .expect("frame");
            assert_eq!(second.tag, qmux_proto::remote::FRAME_TAG_PANE_BYTES);
            assert_eq!(second.payload, b"\x1b[2J hi");
            // The peer finished the stream cleanly: end-of-stream, not error.
            assert_eq!(
                read_frame(&mut recv, MAX_PANE_FRAME_BYTES)
                    .await
                    .expect("eof"),
                None
            );
        });
    }

    #[test]
    fn oversize_and_truncated_frames_are_rejected() {
        let _serial = crate::state::test_support::net_serial_guard();
        runtime().block_on(async {
            let (client, server, _guard) = connected_pair().await;
            let (mut send, _recv) = client.open_bi().await.expect("open bi");
            let accept = server.accept_bi();

            // A header promising more than the cap must be refused before
            // any payload is read.
            let mut oversize = [0_u8; 5];
            oversize[..4].copy_from_slice(&(MAX_PANE_FRAME_BYTES + 2).to_be_bytes());
            oversize[4] = qmux_proto::remote::FRAME_TAG_PANE_BYTES;
            send.write_all(&oversize)
                .await
                .expect("write oversize header");

            let (_send_back, mut recv) = accept.await.expect("accept bi");
            let error = read_frame(&mut recv, MAX_PANE_FRAME_BYTES)
                .await
                .err()
                .expect("oversize frames must fail closed");
            assert!(error.contains("cap"), "unexpected error: {error}");

            // A stream that dies mid-frame is an error, not an EOF.
            let (mut send2, _recv2) = client.open_bi().await.expect("open bi");
            let accept2 = server.accept_bi();
            send2
                .write_all(&[0, 0, 0, 9, FRAME_TAG_JSON, b'{'])
                .await
                .expect("write partial frame");
            send2.finish().expect("finish");
            let (_s, mut recv2) = accept2.await.expect("accept bi");
            let error = read_frame(&mut recv2, MAX_PANE_FRAME_BYTES)
                .await
                .err()
                .expect("truncated frames must fail closed");
            assert!(error.contains("payload"), "unexpected error: {error}");
        });
    }

    #[test]
    fn outbound_json_uses_the_same_cap_as_inbound_json() {
        let oversized = RemoteFrame::GoingAway {
            reason: "x".repeat(MAX_JSON_FRAME_BYTES as usize + 1),
        };
        let _serial = crate::state::test_support::net_serial_guard();
        runtime().block_on(async {
            let (client, _server, _guard) = connected_pair().await;
            let (mut send, _recv) = client.open_bi().await.expect("open stream");
            let error = write_json(&mut send, &oversized)
                .await
                .expect_err("oversized outbound JSON must fail before writing");
            assert!(error.contains("cap"), "unexpected error: {error}");
        });
    }
}

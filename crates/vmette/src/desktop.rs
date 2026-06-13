//! Desktop computer-use protocol: the **framed request/response codec** that
//! carries the [`crate::Action`] vocabulary between the host [`crate::Session`]
//! (Agent workload) and the in-guest `vmette-desktop-agent`.
//!
//! The *types* on the wire ([`Action`], [`ResponseHeader`], [`ScrollDirection`])
//! are the host↔guest contract and live in `vmette-proto`; this module owns
//! only the framing — a blocking round-trip over any `Read`/`Write`, pure (no
//! VZ, no objc2) and unit-testable in isolation. The headless one-shot path
//! never touches it.
//!
//! ## Wire format
//!
//! ```text
//! [u32 LE req_id][u32 LE header_len][header JSON bytes][payload bytes]
//! ```
//!
//! Two little-endian `u32`s prefix the JSON header: a **request id** and the
//! header length. The host assigns a monotonically increasing `req_id` per
//! request; the guest echoes it verbatim in the matching response frame. That
//! lets the host demultiplex responses to the right waiting caller (C4), so a
//! slow screenshot no longer blocks an input action's *submission* and a
//! response whose caller has already timed out is drained and dropped by its
//! `req_id` instead of desyncing the stream. (The in-guest agent is
//! single-threaded, so it still *executes* one request at a time; `req_id`
//! decouples the host side — submission, per-request timeouts, fault isolation
//! — not guest-side throughput.)
//!
//! Screenshots and any other binary results travel as a raw payload *after* the
//! header; the header's `payload_len` says how many payload bytes follow (0 for
//! none). Requests carry no payload. The guest C agent reads
//! `req_id → header_len → header → payload` and writes the same shape back.

use std::io::{self, Read, Write};

// The action vocabulary + response header are the host↔guest wire *contract*,
// owned by `vmette-proto`. This module owns only the framing codec that moves
// them over the vsock; re-export the types so the library's public API (and
// `crate::Action` / `crate::ResponseHeader`) stay one import away.
pub use vmette_proto::{Action, ResponseHeader, ScrollDirection};

/// Maximum header length we will accept off the wire (1 MiB). Guards a
/// corrupt/hostile length prefix from triggering a huge allocation. The
/// JSON header is tiny in practice; payloads are bounded separately.
const MAX_HEADER_LEN: u32 = 1 << 20;

/// Maximum payload length we will accept off the wire (64 MiB). A 1280×800
/// 24-bit PNG is well under this; the cap bounds a corrupt `payload_len`.
const MAX_PAYLOAD_LEN: u32 = 64 << 20;

/// Write a framed message: `[u32 LE req_id][u32 LE header_len][header][payload]`.
/// The caller is responsible for having set `payload_len` inside the header to
/// match `payload.len()` when serializing a [`ResponseHeader`]; for request
/// frames the payload is empty.
pub fn write_frame<W: Write>(
    w: &mut W,
    req_id: u32,
    header: &[u8],
    payload: &[u8],
) -> io::Result<()> {
    let len = u32::try_from(header.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "header too large"))?;
    w.write_all(&req_id.to_le_bytes())?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(header)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()
}

/// Read a frame's `req_id` and header bytes: a `u32 LE` request id, a `u32 LE`
/// header length, then that many header bytes. Does not read the payload — the
/// caller parses the header to learn `payload_len`, then calls [`read_payload`].
pub fn read_header<R: Read>(r: &mut R) -> io::Result<(u32, Vec<u8>)> {
    let mut id_buf = [0u8; 4];
    r.read_exact(&mut id_buf)?;
    let req_id = u32::from_le_bytes(id_buf);
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("header length {len} exceeds cap {MAX_HEADER_LEN}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok((req_id, buf))
}

/// Read exactly `len` payload bytes (the binary tail of a response frame).
pub fn read_payload<R: Read>(r: &mut R, len: u32) -> io::Result<Vec<u8>> {
    if len > MAX_PAYLOAD_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload length {len} exceeds cap {MAX_PAYLOAD_LEN}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Serialize and send an [`Action`] as a request frame (no payload), tagged
/// with `req_id` so the guest's response can be matched back to this caller.
pub fn send_action<W: Write>(w: &mut W, req_id: u32, action: &Action) -> io::Result<()> {
    let header = serde_json::to_vec(action)?;
    write_frame(w, req_id, &header, &[])
}

/// Read a full response frame: the echoed `req_id`, the parsed
/// [`ResponseHeader`], then its declared `payload_len` bytes. A header that
/// fails to parse is a framing-fatal error (we can't know how many payload
/// bytes follow), so the demultiplexer treats it as a stream tear-down rather
/// than a single-request failure.
pub fn read_response<R: Read>(r: &mut R) -> io::Result<(u32, ResponseHeader, Vec<u8>)> {
    let (req_id, header_bytes) = read_header(r)?;
    let header: ResponseHeader = serde_json::from_slice(&header_bytes)?;
    let payload = if header.payload_len > 0 {
        read_payload(r, header.payload_len)?
    } else {
        Vec::new()
    };
    Ok((req_id, header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_header_only() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 7, b"hello", &[]).unwrap();
        // 4-byte LE req_id (7) + 4-byte LE length (5) + "hello"
        assert_eq!(&buf[..4], &7u32.to_le_bytes());
        assert_eq!(&buf[4..8], &5u32.to_le_bytes());
        assert_eq!(&buf[8..], b"hello");

        let mut cur = std::io::Cursor::new(buf);
        let (id, header) = read_header(&mut cur).unwrap();
        assert_eq!(id, 7);
        assert_eq!(header, b"hello");
    }

    #[test]
    fn frame_round_trip_with_payload() {
        let header = br#"{"ok":true,"payload_len":4}"#;
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut buf = Vec::new();
        write_frame(&mut buf, 42, header, &payload).unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let (id, h, p) = read_response(&mut cur).unwrap();
        assert_eq!(id, 42);
        assert!(h.ok);
        assert_eq!(h.payload_len, 4);
        assert_eq!(p, payload);
    }

    #[test]
    fn send_action_then_read_back_as_frame() {
        let mut buf = Vec::new();
        send_action(&mut buf, 3, &Action::LeftClick).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let (id, header) = read_header(&mut cur).unwrap();
        assert_eq!(id, 3);
        let a: Action = serde_json::from_slice(&header).unwrap();
        assert_eq!(a, Action::LeftClick);
    }

    #[test]
    fn oversized_header_length_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // req_id
        buf.extend_from_slice(&(MAX_HEADER_LEN + 1).to_le_bytes());
        let mut cur = std::io::Cursor::new(buf);
        let err = read_header(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Responses written out of request order still carry — and are read back
    /// with — their own `req_id`, so the host demultiplexer can route each to
    /// the correct waiter regardless of arrival order.
    #[test]
    fn responses_demultiplex_by_req_id_out_of_order() {
        // Two responses queued back to back, id 20 then id 10 (out of order).
        let mut buf = Vec::new();
        let h20 = br#"{"ok":true,"x":2,"y":0,"payload_len":0}"#;
        let h10 = br#"{"ok":true,"x":1,"y":0,"payload_len":3}"#;
        write_frame(&mut buf, 20, h20, &[]).unwrap();
        write_frame(&mut buf, 10, h10, &[0xAA, 0xBB, 0xCC]).unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let (id_a, ha, pa) = read_response(&mut cur).unwrap();
        assert_eq!(id_a, 20);
        assert_eq!(ha.x, Some(2));
        assert!(pa.is_empty());
        let (id_b, hb, pb) = read_response(&mut cur).unwrap();
        assert_eq!(id_b, 10);
        assert_eq!(hb.x, Some(1));
        assert_eq!(pb, [0xAA, 0xBB, 0xCC]);
    }
}

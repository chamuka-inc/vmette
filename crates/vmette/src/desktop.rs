//! Desktop computer-use protocol: the framed request/response wire format
//! and action vocabulary spoken between the host [`crate::Session`] (Agent
//! workload) and the in-guest `vmette-desktop-agent`.
//!
//! This module is the entire host-side surface of the desktop feature's
//! protocol — the headless one-shot path never touches it. Everything here
//! is pure (no VZ, no objc2): types + (de)serialization + a blocking
//! round-trip over any `Read`/`Write`, so it is unit-testable in isolation.
//!
//! ## Wire format
//!
//! ```text
//! [u32 LE header_len][header JSON bytes][payload bytes]
//! ```
//!
//! A single little-endian `u32` prefixes the JSON header. Screenshots and
//! any other binary results travel as a raw payload *after* the header; the
//! header's `payload_len` says how many payload bytes follow (0 for none).
//! Requests carry no payload. Keeping one length prefix (not two) matches
//! the guest C agent's simpler `read(u32) → read(header) → read(payload)`.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// A single computer-use action sent host → guest. Serialized as the JSON
/// header of a request frame (no payload). Variants mirror the Anthropic
/// computer-use tool so the MCP layer maps 1:1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Capture the framebuffer; response carries a PNG payload.
    Screenshot,
    /// Report the current pointer position in the response header (`x`,`y`).
    CursorPosition,
    /// Absolute pointer move to `(x, y)`.
    MouseMove { x: i32, y: i32 },
    /// Left button click at the current pointer position.
    LeftClick,
    /// Right button click at the current pointer position.
    RightClick,
    /// Middle button click at the current pointer position.
    MiddleClick,
    /// Double left click at the current pointer position.
    DoubleClick,
    /// Press-move-release: drag from the current position to `(x, y)`.
    LeftClickDrag { x: i32, y: i32 },
    /// Type a UTF-8 string via synthetic key events.
    Type { text: String },
    /// Press a key chord, e.g. `"ctrl+c"`, `"Return"`, `"alt+Tab"`.
    Key { keys: String },
    /// Scroll `amount` clicks in `direction` at `(x, y)`.
    Scroll {
        x: i32,
        y: i32,
        direction: ScrollDirection,
        amount: i32,
    },
    /// Sleep `ms` milliseconds guest-side (lets UI settle).
    Wait { ms: u64 },
    /// Launch a shell command in the desktop session (e.g. `"chromium &"`).
    Exec { command: String },
}

/// Scroll wheel direction for [`Action::Scroll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// JSON header of a response frame (guest → host). `ok` reports success;
/// on failure `error` carries a message and no payload follows. `x`/`y`
/// are populated by [`Action::CursorPosition`]. `payload_len` is the count
/// of binary bytes (e.g. PNG) following this header in the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseHeader {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default)]
    pub payload_len: u32,
}

impl ResponseHeader {
    /// A bare success header with no payload and no coordinates.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            x: None,
            y: None,
            payload_len: 0,
        }
    }

    /// A failure header carrying `msg`.
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            x: None,
            y: None,
            payload_len: 0,
        }
    }
}

/// Maximum header length we will accept off the wire (1 MiB). Guards a
/// corrupt/hostile length prefix from triggering a huge allocation. The
/// JSON header is tiny in practice; payloads are bounded separately.
const MAX_HEADER_LEN: u32 = 1 << 20;

/// Maximum payload length we will accept off the wire (64 MiB). A 1280×800
/// 24-bit PNG is well under this; the cap bounds a corrupt `payload_len`.
const MAX_PAYLOAD_LEN: u32 = 64 << 20;

/// Write a framed message: `[u32 LE header_len][header][payload]`. The
/// caller is responsible for having set `payload_len` inside the header to
/// match `payload.len()` when serializing a [`ResponseHeader`]; for request
/// frames the payload is empty.
pub fn write_frame<W: Write>(w: &mut W, header: &[u8], payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(header.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "header too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(header)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()
}

/// Read the framed header bytes: a `u32 LE` length followed by that many
/// bytes. Does not read the payload — the caller parses the header to learn
/// `payload_len`, then calls [`read_payload`].
pub fn read_header<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
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
    Ok(buf)
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

/// Serialize and send an [`Action`] as a request frame (no payload).
pub fn send_action<W: Write>(w: &mut W, action: &Action) -> io::Result<()> {
    let header = serde_json::to_vec(action)?;
    write_frame(w, &header, &[])
}

/// Read a full response frame: parse the [`ResponseHeader`], then read its
/// declared `payload_len` bytes.
pub fn read_response<R: Read>(r: &mut R) -> io::Result<(ResponseHeader, Vec<u8>)> {
    let header_bytes = read_header(r)?;
    let header: ResponseHeader = serde_json::from_slice(&header_bytes)?;
    let payload = if header.payload_len > 0 {
        read_payload(r, header.payload_len)?
    } else {
        Vec::new()
    };
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_screenshot_serializes_with_tag() {
        let j = serde_json::to_string(&Action::Screenshot).unwrap();
        assert_eq!(j, r#"{"action":"screenshot"}"#);
    }

    #[test]
    fn action_with_fields_round_trips() {
        let a = Action::MouseMove { x: 10, y: 20 };
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(j, r#"{"action":"mouse_move","x":10,"y":20}"#);
        let back: Action = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn scroll_direction_is_snake_case() {
        let a = Action::Scroll {
            x: 1,
            y: 2,
            direction: ScrollDirection::Down,
            amount: 3,
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains(r#""direction":"down""#));
        let back: Action = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn type_and_key_round_trip() {
        for a in [
            Action::Type {
                text: "hello world".into(),
            },
            Action::Key {
                keys: "ctrl+c".into(),
            },
            Action::Exec {
                command: "chromium &".into(),
            },
            Action::Wait { ms: 500 },
        ] {
            let j = serde_json::to_string(&a).unwrap();
            let back: Action = serde_json::from_str(&j).unwrap();
            assert_eq!(back, a);
        }
    }

    #[test]
    fn response_header_ok_omits_optional_fields() {
        let j = serde_json::to_string(&ResponseHeader::ok()).unwrap();
        assert_eq!(j, r#"{"ok":true,"payload_len":0}"#);
    }

    #[test]
    fn response_header_err_carries_message() {
        let h = ResponseHeader::err("boom");
        let j = serde_json::to_string(&h).unwrap();
        assert!(j.contains(r#""ok":false"#));
        assert!(j.contains(r#""error":"boom""#));
        let back: ResponseHeader = serde_json::from_str(&j).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn frame_round_trip_header_only() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello", &[]).unwrap();
        // 4-byte LE length (5) + "hello"
        assert_eq!(&buf[..4], &5u32.to_le_bytes());
        assert_eq!(&buf[4..], b"hello");

        let mut cur = std::io::Cursor::new(buf);
        let header = read_header(&mut cur).unwrap();
        assert_eq!(header, b"hello");
    }

    #[test]
    fn frame_round_trip_with_payload() {
        let header = br#"{"ok":true,"payload_len":4}"#;
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut buf = Vec::new();
        write_frame(&mut buf, header, &payload).unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let (h, p) = read_response(&mut cur).unwrap();
        assert!(h.ok);
        assert_eq!(h.payload_len, 4);
        assert_eq!(p, payload);
    }

    #[test]
    fn send_action_then_read_back_as_frame() {
        let mut buf = Vec::new();
        send_action(&mut buf, &Action::LeftClick).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let header = read_header(&mut cur).unwrap();
        let a: Action = serde_json::from_slice(&header).unwrap();
        assert_eq!(a, Action::LeftClick);
    }

    #[test]
    fn oversized_header_length_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_HEADER_LEN + 1).to_le_bytes());
        let mut cur = std::io::Cursor::new(buf);
        let err = read_header(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn cursor_position_response_carries_coords() {
        let h = ResponseHeader {
            ok: true,
            error: None,
            x: Some(640),
            y: Some(400),
            payload_len: 0,
        };
        let j = serde_json::to_string(&h).unwrap();
        let back: ResponseHeader = serde_json::from_str(&j).unwrap();
        assert_eq!(back.x, Some(640));
        assert_eq!(back.y, Some(400));
    }
}

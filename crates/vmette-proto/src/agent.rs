//! The host↔guest **computer-use vocabulary** spoken over vsock between the
//! host [`Session`](https://docs.rs/vmette) (Agent workload) and the in-guest
//! `vmette-desktop-agent`.
//!
//! These are pure types: a request is an [`Action`] serialized as the JSON
//! header of a frame (no payload), and a reply is a [`ResponseHeader`]
//! optionally followed by a binary payload (e.g. a screenshot PNG). The
//! framing codec that moves them over the wire lives in `vmette::desktop`.

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
    /// Replace the X clipboard (the `CLIPBOARD` and `PRIMARY` selections) with
    /// `text`, so a subsequent paste (Ctrl+V in GUI apps, Shift+Insert /
    /// middle-click in terminals) inserts it. Pairs with [`Action::Key`].
    SetClipboard { text: String },
    /// Read the X `CLIPBOARD` selection; the text is returned as the response
    /// **payload** (UTF-8), not in the header — so arbitrary content needs no
    /// JSON escaping. Empty when the clipboard is unset.
    GetClipboard,
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
    fn clipboard_actions_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&Action::GetClipboard).unwrap(),
            r#"{"action":"get_clipboard"}"#
        );
        let a = Action::SetClipboard { text: "hi".into() };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"action":"set_clipboard","text":"hi"}"#
        );
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
            Action::SetClipboard {
                text: "clip".into(),
            },
            Action::GetClipboard,
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

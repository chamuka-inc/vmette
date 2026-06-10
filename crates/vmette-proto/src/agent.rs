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
    /// Run `command` (via `/bin/sh -c`) to completion **synchronously**,
    /// returning its combined stdout/stderr as the response **payload** (UTF-8)
    /// and its exit status in the header's `exit_code` (`None` ⇒ it did not
    /// exit cleanly — killed by the `timeout_ms` guard or a signal). Stdin is
    /// `/dev/null`. The in-guest agent is single-threaded, so a long command
    /// blocks every other action until it returns — intended for short,
    /// terminating commands (read a file, run a probe), not GUI apps (use
    /// [`Action::Exec`]/[`Action::Navigate`] for those). `timeout_ms` defaults
    /// guest-side and is clamped below the host vsock read timeout.
    ExecCapture {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Open `url` in the desktop's browser. The guest hands the URL to a
    /// fixed launcher (`vmette-open`) **without a shell**, so the URL is never
    /// word-split or interpreted — a deterministic, injection-safe alternative
    /// to driving the address bar with synthetic keystrokes. Fire-and-forget:
    /// returns a bare ok once the launcher is spawned, not when the page loads
    /// (pair with a settle screenshot to wait for paint).
    Navigate { url: String },
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
/// are populated by [`Action::CursorPosition`]. `exit_code` is populated by
/// [`Action::ExecCapture`] (`None` ⇒ the command did not exit cleanly, e.g.
/// it timed out). `payload_len` is the count of binary bytes (e.g. PNG)
/// following this header in the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseHeader {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
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
            exit_code: None,
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
            exit_code: None,
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
            Action::Navigate {
                url: "https://example.com/a?b=c&d=e".into(),
            },
            Action::ExecCapture {
                command: "cat /etc/os-release".into(),
                timeout_ms: Some(5000),
            },
            Action::ExecCapture {
                command: "ls".into(),
                timeout_ms: None,
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
    fn exec_capture_serializes_timeout_when_set() {
        let a = Action::ExecCapture {
            command: "ls".into(),
            timeout_ms: None,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"action":"exec_capture","command":"ls"}"#
        );
        let a = Action::ExecCapture {
            command: "ls".into(),
            timeout_ms: Some(2000),
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"action":"exec_capture","command":"ls","timeout_ms":2000}"#
        );
    }

    #[test]
    fn response_header_carries_exit_code() {
        let h = ResponseHeader {
            ok: true,
            error: None,
            x: None,
            y: None,
            exit_code: Some(0),
            payload_len: 12,
        };
        let j = serde_json::to_string(&h).unwrap();
        assert!(j.contains(r#""exit_code":0"#));
        let back: ResponseHeader = serde_json::from_str(&j).unwrap();
        assert_eq!(back, h);
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
            exit_code: None,
            payload_len: 0,
        };
        let j = serde_json::to_string(&h).unwrap();
        let back: ResponseHeader = serde_json::from_str(&j).unwrap();
        assert_eq!(back.x, Some(640));
        assert_eq!(back.y, Some(400));
    }
}

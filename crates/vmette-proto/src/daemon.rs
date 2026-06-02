//! The **`vmetted` UNIX-socket protocol**: line-delimited JSON, one request
//! object in, one-or-more reply objects out. Two independent request/reply
//! pairs share the socket:
//!
//! * **Stateless run** — [`Request`] in, a stream of [`Frame`]s out. The daemon
//!   forks a `vmette` subprocess and relays its stdout/stderr/exit. This object
//!   carries no `kind` tag; the daemon routes to it as the default.
//! * **Stateful desktop** — a [`DesktopRequest`] in (internally tagged by
//!   `kind`), a single [`DesktopReply`] out. These drive live, persistent
//!   desktop sessions held in the daemon's session registry.
//!
//! The desktop reply payloads are standalone structs ([`ActionReply`],
//! [`SettleReply`], …) that double as the [`DesktopReply`] variants, so the
//! daemon builds them and a client reads them back as the *same* Rust types.
//!
//! Fields with a server-side default are modelled as [`Option`] and skipped on
//! the wire when absent: a client expresses "unspecified" as `None`, and the
//! daemon owns the one true default. The stateless [`Request`] follows the same
//! rule — its optional fields render to argv only when set (see
//! [`Request::to_cli_args`]), letting the `vmette` CLI own the lone default.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::Action;
use crate::geom::Rect;
use crate::mount::ShareMount;

// ---- stateless run path -------------------------------------------------

/// One stateless run request: boot a one-shot microVM, relay its output. The
/// daemon (and the MCP sandbox path) render this to `vmette` CLI flags via
/// [`Request::to_cli_args`]. Carries no `kind` tag.
///
/// Fields with a binary-side default are modelled as [`Option`] and omitted
/// from the rendered argv when `None`, so the `vmette` CLI applies the one true
/// default and no value is spelled twice. `kernel`, `initramfs`, `rootfs`, and
/// `exec` are always required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// Rootfs spec dispatched through the CLI's provider registry.
    /// See `vmette providers` for valid forms (path, image ref, tar+...).
    pub rootfs: String,
    #[serde(default)]
    pub rootfs_ro: bool,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub shares: Vec<ShareMount>,
    #[serde(default)]
    pub disks: Vec<PathBuf>,
    pub exec: String,
    #[serde(default)]
    pub net: bool,
    #[serde(default)]
    pub switch_root: bool,
    /// vsock port: -1 disable, 0 auto, >0 fixed. `None` → CLI default (auto).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vsock_port: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_vsock_port: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mib: Option<u64>,
}

impl Request {
    /// Render this request to the `vmette` CLI argv — the single owner of the
    /// `Request` → command-line mapping, shared by the daemon's subprocess
    /// dispatch and the MCP sandbox so the two cannot drift. Unset optional
    /// fields are omitted, letting the CLI apply its own default.
    pub fn to_cli_args(&self) -> Vec<OsString> {
        // The always-present flags; the optional ones are pushed below.
        let mut a: Vec<OsString> = vec![
            "--kernel".into(),
            self.kernel.clone().into_os_string(),
            "--initramfs".into(),
            self.initramfs.clone().into_os_string(),
            "--rootfs".into(),
            self.rootfs.clone().into(),
        ];
        if self.rootfs_ro {
            a.push("--rootfs-ro".into());
        }
        if self.offline {
            a.push("--offline".into());
        }
        for s in &self.shares {
            a.push("--share".into());
            a.push(format!("{}={}", s.tag, s.path.display()).into());
        }
        for d in &self.disks {
            a.push("--disk".into());
            a.push(d.clone().into_os_string());
        }
        a.push("--exec".into());
        a.push(self.exec.clone().into());
        if self.net {
            a.push("--net".into());
        }
        if self.switch_root {
            a.push("--switch-root".into());
        }
        if let Some(p) = self.vsock_port {
            a.push("--vsock-port".into());
            a.push(p.to_string().into());
        }
        if let Some(p) = self.guest_vsock_port {
            a.push("--guest-vsock-port".into());
            a.push(p.to_string().into());
        }
        if let Some(t) = self.timeout_seconds {
            a.push("--timeout".into());
            a.push(t.to_string().into());
        }
        if let Some(v) = self.vcpus {
            a.push("--vcpus".into());
            a.push(v.to_string().into());
        }
        if let Some(m) = self.mem_mib {
            a.push("--mem-mib".into());
            a.push(m.to_string().into());
        }
        a
    }
}

/// One streamed reply line from the stateless run path. The daemon emits many
/// `Stdout`/`Stderr` frames followed by a terminal `Exit` (or `Error`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Frame {
    Stdout { data: String },
    Stderr { data: String },
    Exit { code: i32 },
    Error { message: String },
}

// ---- stateful desktop path: requests ------------------------------------

/// A desktop request, internally tagged by `kind`. The daemon routes desktop
/// connections here; each variant's payload is a standalone struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesktopRequest {
    /// Boot a persistent desktop VM. `image` is resolved client-side; the
    /// remaining defaulted fields (`vcpus`, `mem_mib`, `size`) are filled by
    /// the daemon when absent.
    DesktopStart(DesktopStart),
    /// Run one computer-use action against a live session.
    DesktopAction(DesktopAction),
    /// Poll until the desktop stops changing, then return that frame plus the
    /// regions still moving.
    DesktopScreenshotSettled(DesktopScreenshotSettled),
    /// Capture one frame and report what moved since the previous capture.
    DesktopWhatChanged(DesktopWhatChanged),
    /// Tear a live session down.
    DesktopStop(DesktopStop),
}

/// Payload of [`DesktopRequest::DesktopStart`]. The kernel + initramfs are the
/// ordinary vmette assets; desktop-ness comes from `image` + the Agent
/// workload. `None` optional fields take the daemon's defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopStart {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// OCI/tar/path rootfs spec. Resolved client-side (explicit `--image` →
    /// `$VMETTE_DESKTOP_IMAGE` → local `vmette-desktop-rootfs.tar` → registry
    /// fallback) exactly like kernel/initramfs, so the daemon receives a
    /// concrete spec and owns no desktop-image default.
    pub image: String,
    /// "WIDTHxHEIGHT"; daemon defaults to 1280x800 when absent/unparseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default)]
    pub net: bool,
    #[serde(default)]
    pub offline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mib: Option<u64>,
}

/// Payload of [`DesktopRequest::DesktopAction`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopAction {
    pub session_id: String,
    pub action: Action,
}

/// Payload of [`DesktopRequest::DesktopScreenshotSettled`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopScreenshotSettled {
    pub session_id: String,
    /// Max time to wait for the screen to settle before returning the latest
    /// frame anyway (with `settled: false`). Daemon defaults to 10s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// How long the screen must stay continuously settled before the frame is
    /// returned. Bridges the quiescent gap a network-bound app shows between
    /// painting its chrome and its content: a transient settle (a blank page
    /// mid-load) is interrupted when content paints and so does not satisfy the
    /// hold, while a video/spinner is excluded as churn and never resets it.
    /// Daemon defaults to a small confirmation hold; `desktop_launch` passes a
    /// larger one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_hold_ms: Option<u64>,
}

/// Payload of [`DesktopRequest::DesktopWhatChanged`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopWhatChanged {
    pub session_id: String,
}

/// Payload of [`DesktopRequest::DesktopStop`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopStop {
    pub session_id: String,
}

// ---- stateful desktop path: replies -------------------------------------

/// A single desktop reply, internally tagged by `kind`. Each variant wraps a
/// standalone payload struct the daemon builds and the client reads back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesktopReply {
    Session(SessionReply),
    ActionResult(ActionReply),
    Settled(SettleReply),
    Changed(ChangedReply),
    Stopped,
    Error(ErrorReply),
}

/// Reply to `desktop_start`: the new session's id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReply {
    pub session_id: String,
}

/// Reply to `desktop_action`: the agent's response-header fields plus an
/// optional base64 PNG (present for `screenshot`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// Base64 PNG for `screenshot`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png_base64: Option<String>,
    /// Clipboard contents for `get_clipboard` (the response payload decoded as
    /// UTF-8); absent for every other action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Reply to `desktop_screenshot_settled`: the captured frame, whether it
/// actually settled (vs. timed out), and the regions still moving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleReply {
    pub settled: bool,
    pub moving: Vec<Rect>,
    pub png_base64: String,
}

/// Reply to `desktop_what_changed`: a fresh frame and the damage box (absent
/// when nothing changed since the previous capture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<Rect>,
    pub png_base64: String,
}

/// Reply carrying a daemon-side error message (any failed request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorReply {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_leaves_unset_optionals_none() {
        let req: Request =
            serde_json::from_str(r#"{"kernel":"/k","initramfs":"/i","rootfs":"/r","exec":"echo"}"#)
                .unwrap();
        assert_eq!(req.vsock_port, None);
        assert_eq!(req.guest_vsock_port, None);
        assert_eq!(req.vcpus, None);
        assert_eq!(req.mem_mib, None);
        assert!(!req.net);
    }

    #[test]
    fn to_cli_args_omits_unset_optionals() {
        let req: Request =
            serde_json::from_str(r#"{"kernel":"/k","initramfs":"/i","rootfs":"/r","exec":"echo"}"#)
                .unwrap();
        let args: Vec<String> = req
            .to_cli_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--kernel",
                "/k",
                "--initramfs",
                "/i",
                "--rootfs",
                "/r",
                "--exec",
                "echo"
            ]
        );
        // No defaulted scalar flags appear — the CLI owns those defaults.
        assert!(!args
            .iter()
            .any(|a| a == "--vcpus" || a == "--mem-mib" || a == "--vsock-port"));
    }

    #[test]
    fn to_cli_args_renders_set_fields() {
        let req = Request {
            kernel: "/k".into(),
            initramfs: "/i".into(),
            rootfs: "/r".into(),
            rootfs_ro: true,
            offline: true,
            shares: vec![ShareMount {
                tag: "work".into(),
                path: "/tmp/x".into(),
            }],
            disks: vec!["/d.img".into()],
            exec: "echo hi".into(),
            net: true,
            switch_root: true,
            vsock_port: Some(0),
            guest_vsock_port: Some(1025),
            timeout_seconds: Some(30),
            vcpus: Some(2),
            mem_mib: Some(1024),
        };
        let args: Vec<String> = req
            .to_cli_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--share" && w[1] == "work=/tmp/x"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--disk" && w[1] == "/d.img"));
        assert!(args.contains(&"--rootfs-ro".to_string()));
        assert!(args.contains(&"--net".to_string()));
        assert!(args.contains(&"--switch-root".to_string()));
        assert!(args.windows(2).any(|w| w[0] == "--vcpus" && w[1] == "2"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--mem-mib" && w[1] == "1024"));
    }

    #[test]
    fn frame_tags_are_lowercase() {
        let j = serde_json::to_string(&Frame::Exit { code: 0 }).unwrap();
        assert_eq!(j, r#"{"kind":"exit","code":0}"#);
    }

    #[test]
    fn desktop_request_deserializes_by_kind() {
        let r: DesktopRequest =
            serde_json::from_str(r#"{"kind":"desktop_stop","session_id":"abc"}"#).unwrap();
        match r {
            DesktopRequest::DesktopStop(s) => assert_eq!(s.session_id, "abc"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn desktop_action_carries_typed_action() {
        let r: DesktopRequest = serde_json::from_str(
            r#"{"kind":"desktop_action","session_id":"s","action":{"action":"left_click"}}"#,
        )
        .unwrap();
        match r {
            DesktopRequest::DesktopAction(a) => {
                assert_eq!(a.session_id, "s");
                assert_eq!(a.action, Action::LeftClick);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn desktop_start_omits_unset_optionals() {
        let j = serde_json::to_string(&DesktopRequest::DesktopStart(DesktopStart {
            kernel: "/k".into(),
            initramfs: "/i".into(),
            image: "alpine:3.20".into(),
            size: None,
            net: true,
            offline: false,
            vcpus: None,
            mem_mib: None,
        }))
        .unwrap();
        // kind + the always-present fields (image is required, resolved
        // client-side); size/vcpus/mem_mib stay omitted when None.
        assert_eq!(
            j,
            r#"{"kind":"desktop_start","kernel":"/k","initramfs":"/i","image":"alpine:3.20","net":true,"offline":false}"#
        );
    }

    #[test]
    fn reply_session_flattens_under_kind() {
        let j = serde_json::to_string(&DesktopReply::Session(SessionReply {
            session_id: "deadbeef".into(),
        }))
        .unwrap();
        assert_eq!(j, r#"{"kind":"session","session_id":"deadbeef"}"#);
    }

    #[test]
    fn reply_action_omits_none_fields() {
        let j = serde_json::to_string(&DesktopReply::ActionResult(ActionReply {
            ok: true,
            error: None,
            x: None,
            y: None,
            png_base64: None,
            text: None,
        }))
        .unwrap();
        assert_eq!(j, r#"{"kind":"action_result","ok":true}"#);
    }

    #[test]
    fn reply_settled_carries_moving_rects() {
        let j = serde_json::to_string(&DesktopReply::Settled(SettleReply {
            settled: true,
            moving: vec![Rect {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            }],
            png_base64: "AA".into(),
        }))
        .unwrap();
        assert_eq!(
            j,
            r#"{"kind":"settled","settled":true,"moving":[{"x":1,"y":2,"w":3,"h":4}],"png_base64":"AA"}"#
        );
    }

    #[test]
    fn reply_changed_omits_absent_damage() {
        let j = serde_json::to_string(&DesktopReply::Changed(ChangedReply {
            changed: None,
            png_base64: "AA".into(),
        }))
        .unwrap();
        assert_eq!(j, r#"{"kind":"changed","png_base64":"AA"}"#);
    }

    #[test]
    fn reply_error_round_trips() {
        let j = serde_json::to_string(&DesktopReply::Error(ErrorReply {
            message: "boom".into(),
        }))
        .unwrap();
        assert_eq!(j, r#"{"kind":"error","message":"boom"}"#);
        let back: DesktopReply = serde_json::from_str(&j).unwrap();
        match back {
            DesktopReply::Error(e) => assert_eq!(e.message, "boom"),
            other => panic!("wrong variant: {other:?}"),
        }
    }
}

//! `vmette desktop …` — a thin client for the desktop session registry in
//! `vmetted`. This exists for manual end-to-end testing without an MCP host:
//! start a persistent desktop VM, screenshot it, drive mouse/keyboard, run
//! apps, then stop it.
//!
//! Each subcommand is one request/one reply over the daemon's UNIX socket
//! (line-delimited JSON) via the shared synchronous [`vmette_daemon_client`]
//! transport — no async runtime in the CLI. Requests and replies are the shared
//! [`vmette_proto`] wire types, so a field renamed in the protocol is a compile
//! error here, not a silent runtime break.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine as _;
use vmette_proto::agent::{Action, ScrollDirection};
use vmette_proto::daemon::{
    ActionReply, DesktopAction, DesktopReply, DesktopRequest, DesktopScreenshotSettled,
    DesktopStart, DesktopStop, DesktopView,
};
use vmette_proto::ShareMount;

/// Ensure `vmetted` is up before a desktop command. `autostart` (the default
/// socket, not a caller-managed `--socket`) lazily spawns a detached daemon so
/// `vmette desktop` works without a manual `vmetted &`. Delegates to the shared
/// [`vmette_daemon_client`] transport — the single owner of connect/auto-spawn.
fn ensure_daemon(socket: &Path, autostart: bool) -> Result<(), String> {
    vmette_daemon_client::DaemonClient::new(socket, autostart).ensure()
}

fn desktop_usage() -> ! {
    eprintln!(
        "vmette desktop <command> [options]   (talks to vmetted; start it first)\n\
         \n\
         commands:\n\
           start [--image REF] [--size WxH] [--net] [--offline] [--ca-certs DIR]\n\
                 [--kernel PATH] [--initramfs PATH]   boot a desktop; prints SESSION_ID\n\
           screenshot SESSION_ID --out FILE [--settle]   capture the framebuffer to a PNG\n\
                       [--timeout-ms N] [--stable-hold-ms N]   (--settle waits for the screen to quiesce)\n\
           cursor      SESSION_ID                     print the pointer position\n\
           move        SESSION_ID X Y                 move the pointer\n\
           click       SESSION_ID X Y                 left-click at X Y\n\
           double-click SESSION_ID X Y                double left-click at X Y\n\
           right-click SESSION_ID X Y                 right-click at X Y\n\
           drag        SESSION_ID FX FY TX TY         press at (FX,FY), drag to (TX,TY), release\n\
           type        SESSION_ID TEXT                type a string\n\
           key         SESSION_ID CHORD               press a chord, e.g. 'ctrl+c'\n\
           set-clipboard SESSION_ID TEXT              put TEXT on the clipboard\n\
           get-clipboard SESSION_ID                   print the clipboard contents\n\
           paste       SESSION_ID TEXT                set clipboard then Ctrl+V\n\
           scroll      SESSION_ID X Y DIR AMOUNT      scroll (DIR: up|down|left|right)\n\
           exec        SESSION_ID COMMAND             launch a shell command in the guest\n\
           exec-capture SESSION_ID COMMAND [--timeout-ms N]   run a command and print its output\n\
           navigate    SESSION_ID URL                 open URL in the desktop browser (no shell)\n\
           view        SESSION_ID                     open a live VNC view; prints vnc://HOST:PORT\n\
           stop        SESSION_ID                     tear the session down\n\
         \n\
         global:\n\
           --socket PATH   daemon socket (default ~/Library/Caches/vmette/vmette.sock)\n"
    );
    std::process::exit(2);
}

/// Send one request and read the single reply (via the shared transport).
/// `autostart` is off here — `ensure_daemon` already brought the daemon up.
fn call(socket: &Path, req: &DesktopRequest) -> Result<DesktopReply, String> {
    vmette_daemon_client::DaemonClient::new(socket, false).request(req)
}

/// Send a `desktop_action` carrying `action` and return its action reply.
fn action(socket: &Path, session: &str, action: Action) -> Result<ActionReply, String> {
    let reply = call(
        socket,
        &DesktopRequest::DesktopAction(DesktopAction {
            session_id: session.to_string(),
            action,
        }),
    )?;
    match reply {
        DesktopReply::ActionResult(r) => Ok(r),
        other => Err(format!("unexpected reply to action: {other:?}")),
    }
}

/// Pull a positional arg or exit with usage.
fn pos(args: &[String], i: usize, what: &str) -> String {
    args.get(i).cloned().unwrap_or_else(|| {
        eprintln!("error: missing {what}");
        desktop_usage();
    })
}

fn parse_i32(s: &str, what: &str) -> i32 {
    s.parse().unwrap_or_else(|_| {
        eprintln!("error: {what} must be an integer, got '{s}'");
        desktop_usage();
    })
}

/// Entry point: `args` is argv after the `desktop` token.
pub fn run(mut args: Vec<String>) -> ExitCode {
    // Extract the global --socket flag from anywhere in the args.
    let mut socket = vmette_assets::default_socket();
    let mut socket_overridden = false;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--socket" {
            if i + 1 >= args.len() {
                eprintln!("error: --socket needs PATH");
                desktop_usage();
            }
            socket = PathBuf::from(args.remove(i + 1));
            socket_overridden = true;
            args.remove(i);
        } else {
            i += 1;
        }
    }

    let cmd = if args.is_empty() {
        desktop_usage();
    } else {
        args.remove(0)
    };

    // Every desktop command needs vmetted up. On the default socket we
    // auto-start it (mirroring the MCP server) so `vmette desktop start` works
    // out of the box; a custom --socket means the caller runs their own daemon,
    // so we don't spawn one behind their back. Help never connects.
    if !matches!(cmd.as_str(), "-h" | "--help") {
        if let Err(e) = ensure_daemon(&socket, !socket_overridden) {
            eprintln!("[vmette desktop] error: {e}");
            return ExitCode::from(1);
        }
    }

    let result = match cmd.as_str() {
        "start" => cmd_start(&socket, &args),
        "screenshot" => cmd_screenshot(&socket, &args),
        "cursor" => cmd_cursor(&socket, &args),
        "move" => {
            let s = pos(&args, 0, "SESSION_ID");
            let x = parse_i32(&pos(&args, 1, "X"), "X");
            let y = parse_i32(&pos(&args, 2, "Y"), "Y");
            action(&socket, &s, Action::MouseMove { x, y }).map(|r| Some(landed(x, y, &r)))
        }
        "click" => cmd_click(&socket, &args, Action::LeftClick),
        "double-click" => cmd_click(&socket, &args, Action::DoubleClick),
        "right-click" => cmd_click(&socket, &args, Action::RightClick),
        "drag" => cmd_drag(&socket, &args),
        "type" => {
            let s = pos(&args, 0, "SESSION_ID");
            let text = pos(&args, 1, "TEXT");
            action(&socket, &s, Action::Type { text }).map(|_| None)
        }
        "key" => {
            let s = pos(&args, 0, "SESSION_ID");
            let keys = pos(&args, 1, "CHORD");
            action(&socket, &s, Action::Key { keys }).map(|_| None)
        }
        "set-clipboard" => {
            let s = pos(&args, 0, "SESSION_ID");
            let text = pos(&args, 1, "TEXT");
            action(&socket, &s, Action::SetClipboard { text }).map(|_| None)
        }
        "get-clipboard" => {
            let s = pos(&args, 0, "SESSION_ID");
            action(&socket, &s, Action::GetClipboard).map(|r| Some(r.text.unwrap_or_default()))
        }
        // Convenience: set the clipboard, then paste with Ctrl+V (GUI apps).
        "paste" => {
            let s = pos(&args, 0, "SESSION_ID");
            let text = pos(&args, 1, "TEXT");
            action(&socket, &s, Action::SetClipboard { text })
                .and_then(|_| {
                    action(
                        &socket,
                        &s,
                        Action::Key {
                            keys: "ctrl+v".into(),
                        },
                    )
                })
                .map(|_| None)
        }
        "scroll" => cmd_scroll(&socket, &args),
        "exec" => {
            let s = pos(&args, 0, "SESSION_ID");
            let command = pos(&args, 1, "COMMAND");
            action(&socket, &s, Action::Exec { command }).map(|_| None)
        }
        "exec-capture" => cmd_exec_capture(&socket, &args),
        "navigate" => {
            let s = pos(&args, 0, "SESSION_ID");
            let url = pos(&args, 1, "URL");
            action(&socket, &s, Action::Navigate { url }).map(|_| None)
        }
        "view" => cmd_view(&socket, &args),
        "stop" => {
            let s = pos(&args, 0, "SESSION_ID");
            call(
                &socket,
                &DesktopRequest::DesktopStop(DesktopStop {
                    session_id: s.clone(),
                }),
            )
            .map(|_| Some(format!("stopped {s}")))
        }
        "-h" | "--help" => desktop_usage(),
        other => {
            eprintln!("error: unknown desktop command '{other}'");
            desktop_usage();
        }
    };

    match result {
        Ok(Some(msg)) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[vmette desktop] error: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_start(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let mut image: Option<String> = None;
    let mut size: Option<String> = None;
    let mut net = false;
    let mut offline = false;
    let mut kernel: Option<PathBuf> = None;
    let mut initramfs: Option<PathBuf> = None;
    let mut shares: Vec<ShareMount> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--image" => image = it.next().cloned(),
            "--size" => size = it.next().cloned(),
            "--net" => net = true,
            "--offline" => offline = true,
            "--kernel" => kernel = it.next().map(PathBuf::from),
            "--initramfs" => initramfs = it.next().map(PathBuf::from),
            "--ca-certs" => {
                let path = it.next().map(PathBuf::from).ok_or("--ca-certs needs DIR")?;
                if let Some(share) = vmette_assets::resolve_ca_share(Some(path)) {
                    shares.push(share);
                }
            }
            other => return Err(format!("unknown start option '{other}'")),
        }
    }

    // No explicit `--ca-certs`? Fall back to the machine-wide source
    // (`$VMETTE_CA_CERTS` / `~/.config/vmette/certs`) so a desktop trusts a
    // configured proxy CA without a per-call flag — same resolution every
    // other vmette root uses.
    crate::ensure_ca_share(&mut shares);

    let kernel = vmette_assets::require_asset(kernel, "vmlinuz-virt")?;
    let initramfs = vmette_assets::require_asset(initramfs, "initramfs-vmette")?;
    // Resolve the desktop rootfs spec like the kernel/initramfs: explicit
    // `--image` → `$VMETTE_DESKTOP_IMAGE` → local `vmette-desktop-rootfs.tar` →
    // registry fallback. The daemon receives a concrete spec.
    let image = vmette_assets::resolve_desktop_image(image);

    // `vcpus`/`mem_mib` left unset → the daemon applies its desktop defaults.
    let reply = call(
        socket,
        &DesktopRequest::DesktopStart(DesktopStart {
            kernel,
            initramfs,
            image,
            size,
            net,
            offline,
            shares,
            vcpus: None,
            mem_mib: None,
        }),
    )?;
    match reply {
        DesktopReply::Session(s) => Ok(Some(s.session_id)),
        other => Err(format!("daemon did not return a session_id: {other:?}")),
    }
}

fn cmd_screenshot(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let mut out: Option<PathBuf> = None;
    let mut settle = false;
    let mut timeout_ms: Option<u64> = None;
    let mut stable_hold_ms: Option<u64> = None;
    let mut it = args[1.min(args.len())..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = it.next().map(PathBuf::from),
            "--settle" => settle = true,
            "--timeout-ms" => {
                let v = it.next().ok_or("--timeout-ms needs N")?;
                timeout_ms = Some(v.parse().map_err(|_| "--timeout-ms must be an integer")?);
            }
            "--stable-hold-ms" => {
                let v = it.next().ok_or("--stable-hold-ms needs N")?;
                stable_hold_ms = Some(
                    v.parse()
                        .map_err(|_| "--stable-hold-ms must be an integer")?,
                );
            }
            other => return Err(format!("unknown screenshot option '{other}'")),
        }
    }
    let out = out.ok_or("screenshot needs --out FILE")?;

    // Either tuning flag implies --settle; without any of them we take a plain
    // immediate frame (the original behaviour).
    let settle = settle || timeout_ms.is_some() || stable_hold_ms.is_some();

    let (bytes, status) = if settle {
        let reply = call(
            socket,
            &DesktopRequest::DesktopScreenshotSettled(DesktopScreenshotSettled {
                session_id: session,
                timeout_ms,
                stable_hold_ms,
            }),
        )?;
        let s = match reply {
            DesktopReply::Settled(s) => s,
            other => return Err(format!("unexpected reply to screenshot: {other:?}")),
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(s.png_base64)
            .map_err(|e| format!("decode png: {e}"))?;
        let status = if s.settled {
            " (settled)".to_string()
        } else {
            format!(" (timed out; {} region(s) still moving)", s.moving.len())
        };
        (bytes, status)
    } else {
        let reply = action(socket, &session, Action::Screenshot)?;
        let b64 = reply.png_base64.ok_or("reply had no png_base64")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("decode png: {e}"))?;
        (bytes, String::new())
    };

    std::fs::write(&out, &bytes).map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(Some(format!(
        "wrote {} ({} bytes){}",
        out.display(),
        bytes.len(),
        status
    )))
}

/// Run a command synchronously in the guest and print its combined
/// stdout/stderr. Exits non-zero if the command did not exit cleanly (exit
/// status != 0, or no clean exit — e.g. it timed out), so it composes in
/// shell pipelines.
fn cmd_exec_capture(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let command = pos(args, 1, "COMMAND");
    let mut timeout_ms: Option<u64> = None;
    let mut it = args[2.min(args.len())..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--timeout-ms" => {
                let v = it.next().ok_or("--timeout-ms needs N")?;
                timeout_ms = Some(v.parse().map_err(|_| "--timeout-ms must be an integer")?);
            }
            other => return Err(format!("unknown exec-capture option '{other}'")),
        }
    }
    let reply = action(
        socket,
        &session,
        Action::ExecCapture {
            command,
            timeout_ms,
        },
    )?;
    // Print the captured output verbatim (no trailing newline added).
    if let Some(text) = reply.text {
        print!("{text}");
    }
    match reply.exit_code {
        Some(0) => Ok(None),
        Some(code) => Err(format!("command exited {code}")),
        None => Err("command did not exit cleanly (timed out or killed)".into()),
    }
}

/// Open (or look up) the session's live VNC view and print the `vnc://` URL a
/// viewer connects to. Idempotent — a second call returns the same address.
fn cmd_view(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let reply = call(
        socket,
        &DesktopRequest::DesktopView(DesktopView {
            session_id: session,
        }),
    )?;
    match reply {
        DesktopReply::View(v) => Ok(Some(format!("vnc://{}", v.addr))),
        other => Err(format!("unexpected reply to view: {other:?}")),
    }
}

fn cmd_cursor(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let reply = action(socket, &session, Action::CursorPosition)?;
    let x = reply.x.unwrap_or(0);
    let y = reply.y.unwrap_or(0);
    Ok(Some(format!("{x} {y}")))
}

/// Move to X Y then click. Click actions fire at the current pointer position,
/// so we position first for ergonomic `click X Y`. Prints where the pointer
/// actually landed (the agent echoes its resulting position), flagging a
/// window-manager constraint when it differs from the request.
fn cmd_click(socket: &Path, args: &[String], click: Action) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let x = parse_i32(&pos(args, 1, "X"), "X");
    let y = parse_i32(&pos(args, 2, "Y"), "Y");
    action(socket, &session, Action::MouseMove { x, y })?;
    let reply = action(socket, &session, click)?;
    Ok(Some(landed(x, y, &reply)))
}

/// Press at (FX,FY), drag to (TX,TY), release — a complete one-shot drag for
/// drag-and-drop UIs (reordering, pivot-table layout, sliders). Positions at the
/// start first, since `LeftClickDrag` begins at the current pointer. The agent
/// emits interpolated motion so the gesture crosses the target's drag threshold.
fn cmd_drag(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let fx = parse_i32(&pos(args, 1, "FROM_X"), "FROM_X");
    let fy = parse_i32(&pos(args, 2, "FROM_Y"), "FROM_Y");
    let tx = parse_i32(&pos(args, 3, "TO_X"), "TO_X");
    let ty = parse_i32(&pos(args, 4, "TO_Y"), "TO_Y");
    action(socket, &session, Action::MouseMove { x: fx, y: fy })?;
    let reply = action(socket, &session, Action::LeftClickDrag { x: tx, y: ty })?;
    Ok(Some(landed(tx, ty, &reply)))
}

/// Format a pointer action's landed position from the echoed `x`/`y`, matching
/// the `cursor` command's `"X Y"` shape so output stays machine-parseable.
fn landed(req_x: i32, req_y: i32, reply: &ActionReply) -> String {
    match (reply.x, reply.y) {
        (Some(ax), Some(ay)) if (ax, ay) != (req_x, req_y) => {
            format!("{ax} {ay} (constrained; requested {req_x} {req_y})")
        }
        (Some(ax), Some(ay)) => format!("{ax} {ay}"),
        _ => format!("{req_x} {req_y}"),
    }
}

fn cmd_scroll(socket: &Path, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let x = parse_i32(&pos(args, 1, "X"), "X");
    let y = parse_i32(&pos(args, 2, "Y"), "Y");
    let dir = pos(args, 3, "DIR");
    let amount = parse_i32(&pos(args, 4, "AMOUNT"), "AMOUNT");
    let direction = match dir.as_str() {
        "up" => ScrollDirection::Up,
        "down" => ScrollDirection::Down,
        "left" => ScrollDirection::Left,
        "right" => ScrollDirection::Right,
        other => return Err(format!("DIR must be up|down|left|right, got '{other}'")),
    };
    action(
        socket,
        &session,
        Action::Scroll {
            x,
            y,
            direction,
            amount,
        },
    )?;
    Ok(None)
}

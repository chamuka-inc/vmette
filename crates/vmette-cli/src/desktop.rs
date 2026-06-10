//! `vmette desktop …` — a thin client for the desktop session registry in
//! `vmetted`. This exists for manual end-to-end testing without an MCP host:
//! start a persistent desktop VM, screenshot it, drive mouse/keyboard, run
//! apps, then stop it.
//!
//! Each subcommand is one request/one reply over the daemon's UNIX socket
//! (line-delimited JSON), so this is plain blocking `std::os::unix::net`
//! rather than tokio — no async runtime in the CLI. Requests and replies are
//! the shared [`vmette_proto`] wire types, so a field renamed in the protocol
//! is a compile error here, not a silent runtime break.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use base64::Engine as _;
use vmette_proto::agent::{Action, ScrollDirection};
use vmette_proto::daemon::{
    ActionReply, DesktopAction, DesktopReply, DesktopRequest, DesktopScreenshotSettled,
    DesktopStart, DesktopStop, DesktopView,
};
use vmette_proto::ShareMount;

/// Make sure a `vmetted` is listening on `socket` before we send a request.
/// If `autostart` (the default socket, not a caller-managed `--socket`) and
/// nothing is up, spawn a detached `vmetted` and wait for it to bind — the same
/// lazy-start the MCP server does, so `vmette desktop` works without a manual
/// `vmetted &`. A `NotFound`/`ConnectionRefused` both mean "no daemon"; any
/// other connect error is surfaced as-is.
fn ensure_daemon(socket: &PathBuf, autostart: bool) -> Result<(), String> {
    use std::io::ErrorKind::{ConnectionRefused, NotFound};
    match UnixStream::connect(socket) {
        Ok(_) => return Ok(()),
        Err(e) if matches!(e.kind(), NotFound | ConnectionRefused) => {}
        Err(e) => return Err(format!("connect {} failed: {e}", socket.display())),
    }
    if !autostart {
        // Caller pointed --socket at their own daemon; if it's down, that's
        // theirs to fix. Let the request connect surface the error.
        return Ok(());
    }
    let bin = vmette_assets::locate_vmetted().ok_or_else(|| {
        "vmetted not found next to vmette or on $PATH (needed for desktop sessions) — \
         reinstall vmette, or start it manually with `vmetted &`"
            .to_string()
    })?;
    let mut cmd = Command::new(&bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe and is the only call made in the
    // forked child before exec. Detaching into a new session lets the daemon
    // outlive this short-lived CLI and survive terminal signals, matching the
    // MCP server's auto-spawn and vmetted's shared-daemon model.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| format!("spawning {}: {e}", bin.display()))?;
    // vmetted clears any stale socket and binds during startup; poll until it
    // accepts a connection, or give up after ~5s.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
    }
    Err(format!(
        "vmetted did not start listening on {} within 5s",
        socket.display()
    ))
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

/// Send one request and read the single reply back, mapping a daemon
/// [`DesktopReply::Error`] to an `Err`.
fn call(socket: &PathBuf, req: &DesktopRequest) -> Result<DesktopReply, String> {
    let stream = UnixStream::connect(socket).map_err(|e| {
        format!(
            "connect {} failed: {e} (is vmetted running?)",
            socket.display()
        )
    })?;
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    w.write_all(&line).map_err(|e| e.to_string())?;
    let _ = w.flush();

    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    let reply = reply.trim();
    if reply.is_empty() {
        return Err(
            "daemon closed the connection without replying — vmetted likely crashed or is \
             running a stale build. Check it's alive (`pgrep vmetted`) and restart it; if you \
             just reinstalled, kill the old PID first. See docs/DAEMON.md."
                .into(),
        );
    }
    let reply: DesktopReply =
        serde_json::from_str(reply).map_err(|e| format!("bad reply: {e}: {reply}"))?;
    match reply {
        DesktopReply::Error(e) => Err(e.message),
        other => Ok(other),
    }
}

/// Send a `desktop_action` carrying `action` and return its action reply.
fn action(socket: &PathBuf, session: &str, action: Action) -> Result<ActionReply, String> {
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
            action(&socket, &s, Action::MouseMove { x, y }).map(|_| None)
        }
        "click" => cmd_click(&socket, &args, Action::LeftClick),
        "double-click" => cmd_click(&socket, &args, Action::DoubleClick),
        "right-click" => cmd_click(&socket, &args, Action::RightClick),
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

fn cmd_start(socket: &PathBuf, args: &[String]) -> Result<Option<String>, String> {
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
                shares.push(ShareMount {
                    tag: "certs".to_string(),
                    path,
                });
            }
            other => return Err(format!("unknown start option '{other}'")),
        }
    }

    let kernel = vmette_assets::require_asset(kernel, "vmlinuz-virt")?;
    let initramfs = vmette_assets::require_asset(initramfs, "initramfs-vmette")?;
    // Resolve the desktop rootfs spec like the kernel/initramfs: explicit
    // `--image` → `$VMETTE_DESKTOP_IMAGE` → local `vmette-desktop-rootfs.tar` →
    // registry fallback. The daemon receives a concrete spec.
    let image = vmette_assets::default_desktop_image(image);

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

fn cmd_screenshot(socket: &PathBuf, args: &[String]) -> Result<Option<String>, String> {
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
fn cmd_exec_capture(socket: &PathBuf, args: &[String]) -> Result<Option<String>, String> {
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
fn cmd_view(socket: &PathBuf, args: &[String]) -> Result<Option<String>, String> {
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

fn cmd_cursor(socket: &PathBuf, args: &[String]) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let reply = action(socket, &session, Action::CursorPosition)?;
    let x = reply.x.unwrap_or(0);
    let y = reply.y.unwrap_or(0);
    Ok(Some(format!("{x} {y}")))
}

/// Move to X Y then click. Click actions fire at the current pointer position,
/// so we position first for ergonomic `click X Y`.
fn cmd_click(socket: &PathBuf, args: &[String], click: Action) -> Result<Option<String>, String> {
    let session = pos(args, 0, "SESSION_ID");
    let x = parse_i32(&pos(args, 1, "X"), "X");
    let y = parse_i32(&pos(args, 2, "Y"), "Y");
    action(socket, &session, Action::MouseMove { x, y })?;
    action(socket, &session, click)?;
    Ok(None)
}

fn cmd_scroll(socket: &PathBuf, args: &[String]) -> Result<Option<String>, String> {
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

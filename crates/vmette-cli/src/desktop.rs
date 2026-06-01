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
use std::path::PathBuf;
use std::process::ExitCode;

use base64::Engine as _;
use vmette_proto::agent::{Action, ScrollDirection};
use vmette_proto::daemon::{
    ActionReply, DesktopAction, DesktopReply, DesktopRequest, DesktopStart, DesktopStop,
};

fn default_socket() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Caches/vmette/vmette.sock")
}

fn desktop_usage() -> ! {
    eprintln!(
        "vmette desktop <command> [options]   (talks to vmetted; start it first)\n\
         \n\
         commands:\n\
           start [--image REF] [--size WxH] [--net] [--offline]\n\
                 [--kernel PATH] [--initramfs PATH]   boot a desktop; prints SESSION_ID\n\
           screenshot SESSION_ID --out FILE           capture the framebuffer to a PNG\n\
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
        return Err("daemon closed the connection without replying".into());
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
    let mut socket = default_socket();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--socket" {
            if i + 1 >= args.len() {
                eprintln!("error: --socket needs PATH");
                desktop_usage();
            }
            socket = PathBuf::from(args.remove(i + 1));
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

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--image" => image = it.next().cloned(),
            "--size" => size = it.next().cloned(),
            "--net" => net = true,
            "--offline" => offline = true,
            "--kernel" => kernel = it.next().map(PathBuf::from),
            "--initramfs" => initramfs = it.next().map(PathBuf::from),
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
    let mut it = args[1.min(args.len())..].iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            out = it.next().map(PathBuf::from);
        }
    }
    let out = out.ok_or("screenshot needs --out FILE")?;
    let reply = action(socket, &session, Action::Screenshot)?;
    let b64 = reply.png_base64.ok_or("reply had no png_base64")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("decode png: {e}"))?;
    std::fs::write(&out, &bytes).map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(Some(format!(
        "wrote {} ({} bytes)",
        out.display(),
        bytes.len()
    )))
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

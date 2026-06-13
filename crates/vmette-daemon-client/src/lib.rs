//! Synchronous client transport for the `vmetted` desktop-session socket
//! protocol — the **single owner** of connect / lazy-auto-spawn / line framing.
//!
//! `vmetted` speaks line-delimited JSON: one [`DesktopRequest`] in, one
//! [`DesktopReply`] out (see [`vmette_proto::daemon`]). Two consumers need to
//! talk to it: the `vmette` CLI (`vmette desktop …`, synchronous) and the
//! `vmette-mcp` server's `desktop_*` tools (async). Both previously hand-rolled
//! the same connect/auto-spawn/read-reply dance; this crate is that dance, once.
//! The CLI uses [`DaemonClient`] directly; the async MCP wraps a `request` call
//! in `spawn_blocking` (it already hops threads for the GUI round-trip).
//!
//! Auto-spawn: when `autostart` and nothing is listening (`NotFound` — never
//! started — or `ConnectionRefused` — present but dead), a detached `vmetted` is
//! spawned (`setsid`, so it outlives a short-lived CLI) and polled until it
//! binds (~5 s). The spawn is serialized by a lock so concurrent callers don't
//! each fork a daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use vmette_proto::daemon::{DesktopReply, DesktopRequest};

/// Shared message for "the daemon hung up without replying" — almost always a
/// crashed or stale `vmetted`.
const NO_REPLY: &str =
    "daemon closed the connection without replying — vmetted likely crashed or is running a \
     stale build. Check it's alive (`pgrep vmetted`) and restart it; if you just reinstalled, \
     kill the old PID first. See docs/DAEMON.md.";

/// A synchronous client for the `vmetted` desktop socket. Cheap to construct;
/// hold one and call [`DaemonClient::request`] per round-trip.
#[derive(Debug)]
pub struct DaemonClient {
    socket: PathBuf,
    /// Lazily start a detached `vmetted` if nothing is listening. The CLI sets
    /// this for the *default* socket (so `vmette desktop` just works) but not for
    /// a caller-managed `--socket` (a down daemon there is theirs to fix). The
    /// MCP server always sets it.
    autostart: bool,
    /// Serializes auto-spawn so concurrent callers don't each fork a daemon.
    spawn_lock: Mutex<()>,
}

impl DaemonClient {
    /// Construct a client for `socket`. `autostart` lazily spawns `vmetted` when
    /// nothing is listening.
    pub fn new(socket: impl Into<PathBuf>, autostart: bool) -> Self {
        Self {
            socket: socket.into(),
            autostart,
            spawn_lock: Mutex::new(()),
        }
    }

    /// The socket path this client talks to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Ensure a `vmetted` is up (connecting, lazily auto-spawning when
    /// `autostart`), without sending a request. Useful as an upfront check
    /// before a series of `request`s. A connect that surfaces an error here
    /// would also surface on the first `request`.
    pub fn ensure(&self) -> Result<(), String> {
        self.connect().map(drop)
    }

    /// Send one request and read the single reply, mapping a daemon
    /// [`DesktopReply::Error`] to an `Err`.
    pub fn request(&self, req: &DesktopRequest) -> Result<DesktopReply, String> {
        let stream = self.connect()?;
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
            return Err(NO_REPLY.to_string());
        }
        let reply: DesktopReply =
            serde_json::from_str(reply).map_err(|e| format!("bad reply: {e}: {reply}"))?;
        match reply {
            DesktopReply::Error(e) => Err(e.message),
            other => Ok(other),
        }
    }

    /// Connect to the socket, lazily starting `vmetted` on `NotFound` /
    /// `ConnectionRefused` when `autostart` (both mean "no daemon up").
    fn connect(&self) -> Result<UnixStream, String> {
        use std::io::ErrorKind::{ConnectionRefused, NotFound};
        match UnixStream::connect(&self.socket) {
            Ok(s) => return Ok(s),
            Err(e) if matches!(e.kind(), NotFound | ConnectionRefused) => {}
            Err(e) => return Err(format!("connect {} failed: {e}", self.socket.display())),
        }
        if !self.autostart {
            // Caller manages their own daemon; surface the connect error.
            return UnixStream::connect(&self.socket).map_err(|e| {
                format!(
                    "connect {} failed: {e} (is vmetted running?)",
                    self.socket.display()
                )
            });
        }
        self.start_and_connect()
    }

    /// Spawn a detached `vmetted`, wait for it to bind, and return the live
    /// connection. The spawn lock means only one caller forks the daemon;
    /// concurrent callers block, then find it already up.
    fn start_and_connect(&self) -> Result<UnixStream, String> {
        let _guard = self.spawn_lock.lock().unwrap();
        // Another caller may have started it while we waited for the lock.
        if let Ok(s) = UnixStream::connect(&self.socket) {
            return Ok(s);
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
        // outlive a short-lived CLI / the MCP server and survive terminal signals.
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
            if let Ok(s) = UnixStream::connect(&self.socket) {
                return Ok(s);
            }
        }
        Err(format!(
            "vmetted did not start listening on {} within 5s",
            self.socket.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use vmette_proto::daemon::{DesktopRequest, DesktopStop};

    /// Spin up a one-shot mock `vmetted` on a temp socket that reads one request
    /// line and writes `reply_json` back. Returns the socket path.
    fn mock_daemon(reply_json: &'static str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vmette-dc-test-{}-{}",
            std::process::id(),
            // a per-call counter avoids collisions without needing a clock
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line); // consume the request
                let mut w = stream;
                let _ = w.write_all(reply_json.as_bytes());
                let _ = w.write_all(b"\n");
            }
        });
        socket
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn stop_req() -> DesktopRequest {
        DesktopRequest::DesktopStop(DesktopStop {
            session_id: "s".into(),
        })
    }

    #[test]
    fn request_parses_a_reply() {
        let socket = mock_daemon(r#"{"kind":"stopped"}"#);
        let client = DaemonClient::new(socket, false);
        assert!(matches!(
            client.request(&stop_req()),
            Ok(DesktopReply::Stopped)
        ));
    }

    #[test]
    fn error_reply_maps_to_err() {
        let socket = mock_daemon(r#"{"kind":"error","message":"boom"}"#);
        let client = DaemonClient::new(socket, false);
        match client.request(&stop_req()) {
            Err(m) => assert_eq!(m, "boom"),
            Ok(other) => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn empty_reply_is_a_clear_error() {
        let socket = mock_daemon("");
        let client = DaemonClient::new(socket, false);
        let err = client.request(&stop_req()).unwrap_err();
        assert!(err.contains("without replying"), "got: {err}");
    }

    #[test]
    fn no_daemon_without_autostart_errors() {
        let client = DaemonClient::new("/nonexistent/vmette-dc.sock", false);
        assert!(client.request(&stop_req()).is_err());
    }
}

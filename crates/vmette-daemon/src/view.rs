//! Live **VNC view** of a desktop session — the bridge between an RFB client
//! and a live [`vmette::Session`].
//!
//! A [`ViewServer`] binds a **loopback TCP listener on an OS-assigned ephemeral
//! port** (never a fixed port) and is owned by exactly one session, so a host
//! running several desktops at once gets several independent views on distinct
//! ports — there is no shared listener and no global state. Each accepted VNC
//! client is served by a reader+writer thread pair that speaks RFB (via the
//! pure [`rfb`](vmette_daemon::rfb) codec) and reuses the session's existing
//! capabilities: it captures the screen with `Action::Screenshot` and forwards
//! the human viewer's pointer/keyboard back as the same computer-use
//! [`Action`]s the agent uses. No guest changes, no second vsock port — the
//! view is a translation layer over the machinery the desktop already has.
//!
//! ## Threading
//!
//! RFB is a client-pull protocol that also pipelines input, so each connection
//! splits into two blocking threads sharing the socket:
//!
//! * **reader** — performs the handshake, then reads client messages: pointer
//!   and key events are mapped to [`Action`]s and dispatched on the session
//!   client immediately; a `FramebufferUpdateRequest` just flags the writer.
//! * **writer** — owns the framebuffer loop: when an update is requested it
//!   captures, diffs against the last frame it sent, and writes only the
//!   changed rectangles (a non-incremental request sends the whole frame).
//!
//! Both threads issue requests on the same `SessionClient`, whose internal lock
//! serializes them — so a viewer's screenshot never interleaves with the
//! agent's synthetic input, which is exactly the ordering you want. The session
//! is the single writer of its display, so a human driving the view and the
//! agent driving the session simply take turns through that one lock.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::RngCore;
use tracing::{debug, warn};
use vmette::SessionClient;
use vmette_proto::agent::Action;

use vmette_daemon::rfb::{
    self, client_msg, key_action, pointer_actions, KeyState, PixelFormat, PointerState,
};

/// How often the writer re-captures while a viewer is waiting on an incremental
/// update and nothing has changed yet. ~5 fps: brisk enough to feel live, slow
/// enough that an idle connected viewer doesn't hammer the session's capture
/// path (every capture serializes with the agent through the session lock).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Upper bound on a `ClientCutText` (clipboard) payload we'll accept from a
/// viewer, so a client-supplied length can't drive an unbounded allocation.
const MAX_CLIENT_CUT_TEXT: usize = 1 << 20; // 1 MiB

/// Per-connection coordination between the reader (which receives client
/// requests) and the writer (which produces framebuffer updates).
struct Conn {
    /// Cleared when either thread sees the socket or session die, so the other
    /// stops promptly.
    alive: AtomicBool,
    state: Mutex<ConnState>,
    /// Signals the writer that `state` changed (a request arrived, or death).
    wake: Condvar,
}

struct ConnState {
    /// Monotonic count of `FramebufferUpdateRequest`s received. The writer
    /// records the value it last satisfied; whenever `req_seq` is ahead, a
    /// request is outstanding. A counter (not a bool) is what makes serving
    /// race-free: a request that arrives *while the writer is mid-send* simply
    /// advances `req_seq` past the writer's served mark, so the next loop serves
    /// it — there is no flag to clobber and no wakeup to lose.
    req_seq: u64,
    /// Whether the outstanding request(s) can be satisfied incrementally
    /// (`false` ⇒ a non-incremental request needs the full frame). Reset to
    /// `true` once served, unless a fresh request arrived during the send.
    incremental: bool,
    /// The client's negotiated pixel format (updated by `SetPixelFormat`).
    pixel_format: PixelFormat,
}

impl Conn {
    fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
            state: Mutex::new(ConnState {
                req_seq: 0,
                incremental: true,
                pixel_format: PixelFormat::server_default(),
            }),
            wake: Condvar::new(),
        }
    }

    /// Mark the connection dead and wake the writer so it can exit. The flag is
    /// stored **while holding `state`** — the same lock the writer parks under —
    /// so a kill landing in the window between the writer's `is_alive()` check
    /// and its `wake.wait()` cannot be lost: kill blocks until the writer has
    /// either parked (then the `notify_all` reaches it) or not yet checked the
    /// flag (then it observes `alive == false` and never parks).
    fn kill(&self) {
        {
            let _state = self.state.lock().unwrap();
            self.alive.store(false, Ordering::Release);
        }
        self.wake.notify_all();
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

/// The set of live viewer sockets, keyed by a per-connection id so each entry
/// can be removed when its connection ends (rather than accumulating dead
/// clones for the session's lifetime). Teardown `shutdown()`s whatever remains.
type Conns = Arc<Mutex<HashMap<u64, TcpStream>>>;

/// A live VNC view bound to one session. Owns the loopback listener and stops
/// it (and all viewer connections) on [`ViewServer::shutdown`].
pub struct ViewServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    /// A clone of each live client socket, so teardown can `shutdown()` them to
    /// unblock their reader threads. Pruned per-connection on disconnect.
    conns: Conns,
    /// Number of viewer connections currently being served. The view's capture
    /// loop bypasses the registry's `last_used` idle timer, so this is how the
    /// idle sweep learns a session is actively being watched and must not be
    /// reaped out from under a connected viewer.
    active: Arc<AtomicUsize>,
    accept_thread: Option<JoinHandle<()>>,
}

/// Per-connection cleanup, run when a viewer thread ends (even on an early
/// return or spawn failure): decrement the live count so it can't leak and
/// wedge a session as permanently "in use", and drop the connection's socket
/// clone from `conns` so closed sockets don't accumulate.
struct ConnGuard {
    active: Arc<AtomicUsize>,
    conns: Conns,
    id: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
        self.conns.lock().unwrap().remove(&self.id);
    }
}

impl ViewServer {
    /// Bind a loopback listener on an ephemeral port and start accepting VNC
    /// clients for `client`'s session. `display_size` is the Xvfb framebuffer
    /// the session captures at; it is advertised to clients and used to size
    /// the frame diff.
    pub fn start(client: SessionClient, display_size: (u32, u32)) -> Result<ViewServer> {
        // Port 0 ⇒ the OS picks a free ephemeral port; each session's view is
        // independent, so concurrent desktops never collide on a port.
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("binding view listener")?;
        let addr = listener.local_addr().context("view listener addr")?;
        listener
            .set_nonblocking(true)
            .context("view listener nonblocking")?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let accept_thread = {
            let shutdown = shutdown.clone();
            let conns = conns.clone();
            let active = active.clone();
            std::thread::Builder::new()
                .name("vmette-view-accept".into())
                .spawn(move || accept_loop(listener, client, display_size, shutdown, conns, active))
                .context("spawning view accept thread")?
        };
        Ok(ViewServer {
            addr,
            shutdown,
            conns,
            active,
            accept_thread: Some(accept_thread),
        })
    }

    /// The loopback `host:port` a VNC client connects to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many viewer connections are currently being served.
    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Stop accepting and tear every viewer connection down. Called when the
    /// session ends.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Unblock every reader stuck in a blocking read.
        for (_, s) in self.conns.lock().unwrap().drain() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ViewServer {
    fn drop(&mut self) {
        // Defensive: a registry path that drops without calling shutdown still
        // stops the accept thread rather than leaking it.
        if self.accept_thread.is_some() {
            self.shutdown();
        }
    }
}

/// Accept VNC clients until shutdown, spawning a handler per connection. The
/// listener is non-blocking so the loop can poll the shutdown flag rather than
/// block forever in `accept()`.
fn accept_loop(
    listener: TcpListener,
    client: SessionClient,
    display_size: (u32, u32),
    shutdown: Arc<AtomicBool>,
    conns: Conns,
    active: Arc<AtomicUsize>,
) {
    let mut next_id: u64 = 0;
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // The accepted socket inherits non-blocking on some platforms;
                // a viewer connection wants blocking I/O.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let id = next_id;
                next_id += 1;
                if let Ok(clone) = stream.try_clone() {
                    conns.lock().unwrap().insert(id, clone);
                }
                let client = client.clone();
                active.fetch_add(1, Ordering::Release);
                // The guard decrements `active` and prunes this connection's
                // socket from `conns` when the thread ends — including the
                // spawn-failure path below, where the guard moved into the
                // (never-run) closure is dropped by `spawn`.
                let guard = ConnGuard {
                    active: active.clone(),
                    conns: conns.clone(),
                    id,
                };
                if let Err(e) = std::thread::Builder::new()
                    .name("vmette-view-conn".into())
                    .spawn(move || {
                        let _guard = guard;
                        if let Err(e) = handle_connection(stream, client, display_size) {
                            warn!(%peer, error = %e, "view connection ended");
                        }
                    })
                {
                    warn!(error = %e, "spawning view connection thread");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                debug!(error = %e, "view accept failed; stopping");
                break;
            }
        }
    }
}

/// Run one VNC connection: RFB handshake, then split into reader (this thread,
/// dispatching input) and writer (framebuffer updates).
fn handle_connection(
    mut stream: TcpStream,
    client: SessionClient,
    display_size: (u32, u32),
) -> Result<()> {
    let (w, h) = display_size;
    handshake(&mut stream, w as u16, h as u16)?;

    let conn = Arc::new(Conn::new());
    let writer_stream = stream.try_clone().context("cloning view socket")?;
    let writer = {
        let conn = conn.clone();
        let client = client.clone();
        std::thread::Builder::new()
            .name("vmette-view-writer".into())
            .spawn(move || writer_loop(writer_stream, client, display_size, conn))
            .context("spawning view writer")?
    };

    let res = reader_loop(stream, &client, &conn);
    // Either side dying ends both: make sure the writer wakes and stops, then
    // reap it so the connection's threads don't outlive the socket.
    conn.kill();
    let _ = writer.join();
    res
}

/// The RFB handshake: offer 3.8, negotiate the security *type* for whatever
/// version the client pins the connection to (macOS Screen Sharing selects RFB
/// 3.3 — see [`rfb::type_negotiation`]), run VNC Authentication, then
/// `ClientInit`/`ServerInit`.
///
/// We offer VNC Authentication rather than `None` because macOS Screen Sharing
/// refuses to connect over `None`. The view is loopback-only and ephemeral, so
/// the challenge response is **not verified** — the loopback bind is the real
/// access boundary, and any password the viewer types is accepted. (Performing
/// the standard challenge/response is still required for the client to proceed.)
fn handshake(stream: &mut TcpStream, w: u16, h: u16) -> Result<()> {
    stream.write_all(rfb::PROTOCOL_VERSION)?;
    let mut client_version = [0u8; 12];
    stream.read_exact(&mut client_version)?;
    if &client_version[..4] != b"RFB " {
        anyhow::bail!("not an RFB client (bad version banner)");
    }

    // Negotiate the security type (always VNC Authentication).
    match rfb::type_negotiation(&client_version) {
        rfb::TypeNegotiation::Dictate => {
            // RFB 3.3: the server dictates the single security type as a U32.
            stream.write_all(&(rfb::SECURITY_VNC_AUTH as u32).to_be_bytes())?;
        }
        rfb::TypeNegotiation::List => {
            // RFB 3.7/3.8: offer a one-entry list; the client echoes its pick.
            stream.write_all(&[1, rfb::SECURITY_VNC_AUTH])?;
            let mut chosen = [0u8; 1];
            stream.read_exact(&mut chosen)?;
        }
    }

    // VNC Authentication: send a random challenge, read the (DES-encrypted)
    // response, and accept it unconditionally — see the fn doc for why.
    let mut challenge = [0u8; rfb::VNC_AUTH_CHALLENGE_LEN];
    rand::thread_rng().fill_bytes(&mut challenge);
    stream.write_all(&challenge)?;
    let mut response = [0u8; rfb::VNC_AUTH_CHALLENGE_LEN];
    stream.read_exact(&mut response)?;
    stream.write_all(&0u32.to_be_bytes())?; // SecurityResult: OK

    // ClientInit (shared-flag) — we always allow shared access, so ignore it.
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared)?;

    stream.write_all(&rfb::server_init(
        w,
        h,
        &PixelFormat::server_default(),
        "vmette desktop",
    ))?;
    Ok(())
}

/// Read client messages until the socket closes, dispatching pointer/key input
/// as session actions and flagging the writer on update requests.
fn reader_loop(mut stream: TcpStream, client: &SessionClient, conn: &Conn) -> Result<()> {
    let mut pointer = PointerState::default();
    let mut keys = KeyState::default();
    let mut msg_type = [0u8; 1];
    while conn.is_alive() {
        if stream.read_exact(&mut msg_type).is_err() {
            break; // client closed (or socket shut down on teardown).
        }
        debug!(msg = msg_type[0], "rfb client message");
        match msg_type[0] {
            client_msg::SET_PIXEL_FORMAT => {
                let mut rest = [0u8; 3 + 16];
                stream.read_exact(&mut rest)?;
                let pf = PixelFormat::parse(rest[3..].try_into().unwrap());
                // Only adopt formats the encoder can faithfully serve; a
                // malformed/hostile SetPixelFormat (e.g. bits_per_pixel > 32 or
                // a shift >= bpp) would otherwise reach encode_pixel and panic
                // the writer thread. Ignoring it keeps the previous (valid)
                // format and the view alive.
                if pf.supported() {
                    conn.state.lock().unwrap().pixel_format = pf;
                } else {
                    debug!(?pf, "ignoring unsupported client pixel format");
                }
            }
            client_msg::SET_ENCODINGS => {
                let mut head = [0u8; 3]; // padding + u16 count
                stream.read_exact(&mut head)?;
                let count = u16::from_be_bytes([head[1], head[2]]) as usize;
                // We only emit Raw; consume and ignore the client's list.
                let mut skip = vec![0u8; count * 4];
                stream.read_exact(&mut skip)?;
            }
            client_msg::FRAMEBUFFER_UPDATE_REQUEST => {
                let mut body = [0u8; 9]; // incremental + x,y,w,h
                stream.read_exact(&mut body)?;
                let incremental = body[0] != 0;
                let mut st = conn.state.lock().unwrap();
                st.req_seq += 1;
                // A non-incremental request forces a full send until served.
                st.incremental = st.incremental && incremental;
                conn.wake.notify_all();
            }
            client_msg::POINTER_EVENT => {
                let mut body = [0u8; 5]; // button-mask + x,y (u16)
                stream.read_exact(&mut body)?;
                let mask = body[0];
                let x = u16::from_be_bytes([body[1], body[2]]) as i32;
                let y = u16::from_be_bytes([body[3], body[4]]) as i32;
                for action in pointer_actions(&mut pointer, mask, x, y) {
                    dispatch(client, conn, action);
                }
            }
            client_msg::KEY_EVENT => {
                let mut body = [0u8; 7]; // down-flag + padding(2) + keysym(u32)
                stream.read_exact(&mut body)?;
                let down = body[0] != 0;
                let keysym = u32::from_be_bytes([body[3], body[4], body[5], body[6]]);
                if let Some(action) = key_action(&mut keys, down, keysym) {
                    dispatch(client, conn, action);
                }
            }
            client_msg::CLIENT_CUT_TEXT => {
                let mut head = [0u8; 7]; // padding(3) + u32 length
                stream.read_exact(&mut head)?;
                let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
                // The length is client-supplied; cap it so a hostile/buggy
                // client can't drive a multi-GiB allocation (which on alloc
                // failure would abort the whole daemon). A clipboard paste over
                // this bound is not a real workload.
                if len > MAX_CLIENT_CUT_TEXT {
                    anyhow::bail!("client cut text too large: {len} bytes");
                }
                let mut text = vec![0u8; len];
                stream.read_exact(&mut text)?;
                // Push the viewer's clipboard into the guest so paste works.
                if let Ok(s) = String::from_utf8(text) {
                    dispatch(client, conn, Action::SetClipboard { text: s });
                }
            }
            other => {
                // An unknown message has an unknown length; we can't resync the
                // stream, so end the connection rather than misread it.
                anyhow::bail!("unknown RFB client message type {other}");
            }
        }
    }
    Ok(())
}

/// Send one action to the session; a transport error means the session is gone,
/// so kill the connection. A non-ok action header (e.g. a momentarily busy
/// agent) is non-fatal — the human can retry.
fn dispatch(client: &SessionClient, conn: &Conn, action: Action) {
    if let Err(e) = client.request(&action) {
        debug!(error = %e, "view input dispatch failed; ending connection");
        conn.kill();
    }
}

/// Produce framebuffer updates on demand: wait for a request, capture, diff,
/// and write the changed rectangles (or the whole frame when non-incremental).
fn writer_loop(
    mut stream: TcpStream,
    client: SessionClient,
    display_size: (u32, u32),
    conn: Arc<Conn>,
) {
    let (w, h) = display_size;
    // The last frame's decoded pixels (for incremental diffs) and its raw PNG
    // bytes (for the idle fast path below).
    let mut last: Option<Vec<u8>> = None;
    let mut last_png: Option<Vec<u8>> = None;
    // The highest `req_seq` this writer has satisfied.
    let mut served_seq: u64 = 0;

    while conn.is_alive() {
        // Block until the client has requested an update we haven't served (or
        // the connection dies). We note `req_seq` now but do NOT advance
        // `served_seq` until a frame actually goes out — so a request arriving
        // during the capture/send below is never lost.
        let (req_seq, incremental) = {
            let mut st = conn.state.lock().unwrap();
            while st.req_seq == served_seq && conn.is_alive() {
                st = conn.wake.wait(st).unwrap();
            }
            if !conn.is_alive() {
                return;
            }
            (st.req_seq, st.incremental)
        };

        // Capture the current screen (raw PNG). A transport error means the
        // session is gone; a non-ok header is transient (agent momentarily busy).
        let payload = match client.request(&Action::Screenshot) {
            Ok((header, payload)) if header.ok => payload,
            Ok(_) => {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(_) => {
                conn.kill();
                return;
            }
        };

        // Idle fast path: the guest's PNG encode is deterministic, so an
        // unchanged screen produces byte-identical bytes. Skip the decode +
        // full-frame diff entirely. Only valid for an incremental request — a
        // non-incremental one must always send a frame.
        if incremental && last_png.as_deref() == Some(payload.as_slice()) {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        let frame = match decode_frame(&payload, w, h) {
            Some(f) => f,
            None => {
                // Wrong-sized or undecodable capture: retry shortly.
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        last_png = Some(payload);

        let rects = match (&last, incremental) {
            (Some(prev), true) => rfb::changed_rects(prev, &frame.pixels, w, h, frame.channels, 32),
            _ => vec![vmette_proto::Rect { x: 0, y: 0, w, h }],
        };

        if rects.is_empty() {
            // Incremental request but nothing changed: leave `served_seq`
            // behind so the request stays outstanding, and re-poll until the
            // screen moves (standard RFB — the server replies only when there
            // is something to show).
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        let pf = conn.state.lock().unwrap().pixel_format;
        let msg = rfb::framebuffer_update(&rects, &pf, &frame.pixels, w, frame.channels);
        if stream.write_all(&msg).is_err() {
            conn.kill();
            return;
        }
        last = Some(frame.pixels);

        // This frame satisfies every request up to `req_seq`. Reset the
        // incremental accumulator only if no newer request slipped in during
        // the send (which would have advanced `req_seq` past it).
        let mut st = conn.state.lock().unwrap();
        served_seq = req_seq;
        if st.req_seq == req_seq {
            st.incremental = true;
        }
    }
}

/// A decoded capture matching the expected framebuffer size.
struct Capture {
    pixels: Vec<u8>,
    channels: u8,
}

/// Decode a screenshot PNG into pixels, or `None` if it failed to decode or its
/// size doesn't match the advertised framebuffer (which is fixed, so a
/// mismatched capture can't be diffed or placed — skip it rather than misrender).
fn decode_frame(payload: &[u8], w: u32, h: u32) -> Option<Capture> {
    let frame = vmette_daemon::decode_png(payload).ok()?;
    if frame.width != w || frame.height != h {
        return None;
    }
    Some(Capture {
        pixels: frame.pixels,
        channels: frame.channels,
    })
}

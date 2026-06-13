//! [`Session`] — the host-side primitive that owns a booted VM, its VZ
//! dispatch queue, and teardown, decoupled from the process. Unlike the old
//! `run()` (whose lifecycle delegate called `std::process::exit`), a
//! `Session` records how it ended into a shared [`EndSlot`] and signals a
//! condvar, so the booting/waiting logic is reusable in-process (the daemon
//! hosts many of these). [`crate::run`] is a thin wrapper that starts a
//! one-shot session, waits, and exits with the guest's code.
//!
//! Threading model: each `Session` operates its VM on its **own private
//! serial dispatch queue** (`initWithConfiguration:queue:`), never the main
//! queue. libdispatch services that queue on its worker-thread pool, so the
//! delegate callbacks and async completion handlers fire there automatically
//! — no run loop needs pumping. [`Session::wait`] simply blocks on a condvar
//! until the terminal event is recorded. This is what lets the daemon host
//! many concurrent VMs (each with its own queue) without fighting over the
//! single main run loop.
//!
//! `Session` itself is `!Send` (it holds objc2 `Retained` handles). To drive
//! a session from other threads — as the multi-threaded daemon does — extract
//! the `Send` [`SessionClient`] (issues desktop requests) and [`StopHandle`]
//! (issues a graceful stop) before handing the `Session` off to the thread
//! that owns its lifetime via [`Session::wait`].

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::AllocAnyThread;
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZVirtioSocketDevice, VZVirtioSocketListener, VZVirtualMachine, VZVirtualMachineDelegate,
};

use crate::desktop::{self, Action, ResponseHeader};
use crate::error::Error;
use crate::vz::config::{build as build_vz_config, resolve_vsock_port, SerialSink};
use crate::vz::delegate::{DelegateState, VmetteDelegate};
use crate::vz::vsock::{ListenerMode, ListenerState, VsockLogger};
use crate::{cmdline, Config, ShareMount, WorkloadStrategy};

/// How long [`SessionClient::request`] waits for the in-guest agent to make
/// its outbound vsock connection before giving up. The desktop image boots
/// Xvfb + WM + agent, which can take several seconds on first run.
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-request response timeout. Bounds how long a single [`Demux::request`]
/// blocks on its reply channel before giving up, so a caller can't hang forever
/// on a wedged guest. Enforced at the channel (not as a socket `SO_RCVTIMEO`),
/// so the shared reader thread keeps doing pure blocking reads and only this one
/// caller fails; its late response is later drained and dropped by `req_id`.
/// Generous: a software-rendered screenshot frame can be slow.
const AGENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Send-wrapper for an objc2 `Retained`. The wrapped VM is only ever touched
/// from inside closures dispatched onto its own queue, so although `Retained`
/// is `!Send` we can safely move the wrapper across threads to enqueue that
/// work. Used to satisfy the `F: Send` bound on `DispatchQueue::after` /
/// `exec_async` when a closure captures the VM handle.
struct QueueBound<T>(Retained<T>);
unsafe impl<T> Send for QueueBound<T> {}
unsafe impl<T> Sync for QueueBound<T> {}
impl<T> std::ops::Deref for QueueBound<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// How a session ended. The first writer into the [`EndSlot`] wins, so a
/// timeout that races a natural poweroff reports `TimedOut`.
#[derive(Debug, Clone)]
pub enum SessionEnd {
    /// Guest powered off; carries the exit code propagated via `.vmette-exit`.
    Exited(i32),
    /// The configured timeout fired and the VM was force-stopped.
    TimedOut,
    /// The caller requested a stop ([`Session::stop`] / [`StopHandle::stop`]).
    Stopped,
    /// VZ reported the guest stopped with an error (start failure or
    /// `virtualMachine:didStopWithError:`).
    Error(String),
}

/// Shared, write-once terminal slot. The lifecycle delegate, the timeout
/// completion, and the start-failure completion all write here; the writer
/// also wakes any thread blocked in [`Session::wait`] via the condvar. These
/// writers run on the VM's private dispatch queue (a libdispatch worker
/// thread), so no run loop is involved.
pub(crate) struct EndSlot {
    end: Mutex<Option<SessionEnd>>,
    cv: Condvar,
}

impl EndSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            end: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    /// Record the terminal state (first writer wins) and wake any waiter.
    pub(crate) fn set(&self, e: SessionEnd) {
        let mut g = self.end.lock().unwrap();
        if g.is_none() {
            *g = Some(e);
        }
        self.cv.notify_all();
    }

    /// Block until a terminal state is recorded, then return a clone of it.
    /// Non-destructive so multiple observers (e.g. the lifetime thread and a
    /// late `stop`) all see the same outcome.
    fn wait_end(&self) -> SessionEnd {
        let mut g = self.end.lock().unwrap();
        while g.is_none() {
            g = self.cv.wait(g).unwrap();
        }
        g.clone().unwrap()
    }

    /// Non-destructive check used by the stop path to avoid issuing a stop on
    /// an already-ended session.
    fn is_set(&self) -> bool {
        self.end.lock().unwrap().is_some()
    }
}

/// One framed reply delivered from the demux reader thread to a waiting
/// caller: either the parsed response, or a message describing why the stream
/// died (read error, EOF, framing corruption).
type Reply = Result<(ResponseHeader, Vec<u8>), String>;

/// Host-side response demultiplexer over the single agent vsock fd (C4).
///
/// A dedicated reader thread owns all reads on the fd; callers register a
/// one-shot channel keyed by a monotonic `req_id`, write their request frame
/// under the short [`Demux::write`] lock, then block on their own channel. The
/// reader routes each response to the matching `req_id`. This means:
///
/// - a slow screenshot no longer serializes another caller's *submission* —
///   only the brief frame write is mutually excluded, not the read;
/// - a per-request timeout is enforced at the caller's channel, so a wedged
///   request fails alone (its late response is later drained and dropped by
///   `req_id`) without desyncing the stream;
/// - a stream-fatal error (read failure, EOF, or a header that won't parse —
///   we then can't know the payload length) poisons the whole demux once and
///   wakes every waiter, since the framing can no longer be trusted.
///
/// The guest agent is single-threaded and still executes one request at a
/// time; the demux decouples the host side, it does not parallelize the guest.
struct Demux {
    /// Serializes request-frame writes onto the fd. Held only for the write,
    /// never across the (slow) read — that is the whole point.
    write: Mutex<()>,
    /// Monotonic request id; wraps after `u32::MAX` requests (harmless — an id
    /// is only live between submit and reply).
    next_id: AtomicU32,
    /// In-flight callers, keyed by `req_id`. The reader removes-and-sends on
    /// arrival; a timed-out caller removes its own entry so the reader drops
    /// the orphaned late response.
    waiters: Arc<Mutex<HashMap<u32, SyncSender<Reply>>>>,
    /// Set once when the stream becomes unusable; subsequent requests fail
    /// fast with this message instead of registering a doomed waiter.
    poison: Arc<Mutex<Option<String>>>,
    /// The accepted agent vsock fd. Borrowed (not owned) — [`AgentConn`] caches
    /// and closes it; closing it is what unblocks the reader's `read` at
    /// teardown.
    fd: RawFd,
}

impl Demux {
    /// Spawn the reader thread and return the demux. `fd` must already be
    /// connected (the reader does pure blocking reads with no socket timeout —
    /// the per-request timeout lives at the caller's channel).
    fn start(fd: RawFd) -> Demux {
        let waiters: Arc<Mutex<HashMap<u32, SyncSender<Reply>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let poison: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let waiters_r = waiters.clone();
        let poison_r = poison.clone();
        std::thread::spawn(move || demux_reader(fd, waiters_r, poison_r));
        Demux {
            write: Mutex::new(()),
            next_id: AtomicU32::new(0),
            waiters,
            poison,
            fd,
        }
    }

    /// First poison message, if the stream has died.
    fn poisoned(&self) -> Option<String> {
        self.poison.lock().unwrap().clone()
    }

    /// Record the first poison message (later writers don't clobber it).
    fn poison_with(&self, msg: String) {
        let mut p = self.poison.lock().unwrap();
        if p.is_none() {
            *p = Some(msg);
        }
    }

    /// Submit one [`Action`] and block (up to [`AGENT_READ_TIMEOUT`]) for the
    /// reader to route back its response.
    fn request(&self, action: &Action) -> Result<(ResponseHeader, Vec<u8>), Error> {
        if let Some(msg) = self.poisoned() {
            return Err(Error::Vsock(msg));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel::<Reply>(1);
        self.waiters.lock().unwrap().insert(id, tx);

        {
            // Hold `write` only for the frame write so concurrent callers can't
            // interleave bytes on the fd; release before the (slow) wait.
            let _w = self.write.lock().unwrap();
            let mut stream = FdStream(self.fd);
            if let Err(e) = desktop::send_action(&mut stream, id, action) {
                // A partial frame may have hit the wire — the stream can no
                // longer be framed. Poison so every caller fails cleanly.
                self.waiters.lock().unwrap().remove(&id);
                let msg = format!("agent request write failed: {e}");
                self.poison_with(msg.clone());
                return Err(Error::Vsock(msg));
            }
        }

        match rx.recv_timeout(AGENT_READ_TIMEOUT) {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(msg)) => Err(Error::Vsock(msg)),
            Err(RecvTimeoutError::Timeout) => {
                // Give up on this id; the reader will drain and drop the late
                // response by req_id, leaving the stream synced for others.
                self.waiters.lock().unwrap().remove(&id);
                Err(Error::Vsock(
                    "timed out waiting for the guest agent response".into(),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The reader dropped our sender → the stream poisoned.
                Err(Error::Vsock(self.poisoned().unwrap_or_else(|| {
                    "agent connection closed unexpectedly".into()
                })))
            }
        }
    }
}

/// Reader thread: own all reads on the agent fd, routing each framed response
/// to the waiter registered under its `req_id`. Exits (poisoning all waiters)
/// on the first unrecoverable framing error — including EOF when the guest
/// powers off, which is the normal teardown path.
fn demux_reader(
    fd: RawFd,
    waiters: Arc<Mutex<HashMap<u32, SyncSender<Reply>>>>,
    poison: Arc<Mutex<Option<String>>>,
) {
    let mut stream = FdStream(fd);
    loop {
        match desktop::read_response(&mut stream) {
            Ok((id, header, payload)) => {
                // Remove-and-send: a present waiter gets its reply; an absent
                // one (caller timed out) means we just drain+drop this frame,
                // which keeps the stream synced for every other id.
                if let Some(tx) = waiters.lock().unwrap().remove(&id) {
                    let _ = tx.send(Ok((header, payload)));
                }
            }
            Err(e) => {
                let msg = format!("agent stream closed: {e}");
                {
                    let mut p = poison.lock().unwrap();
                    if p.is_none() {
                        *p = Some(msg.clone());
                    }
                }
                // Wake everyone still blocked so they fail now, not at timeout.
                let mut w = waiters.lock().unwrap();
                for (_, tx) in w.drain() {
                    let _ = tx.send(Err(msg.clone()));
                }
                return;
            }
        }
    }
}

/// The accepted agent vsock connection plus the channel the listener uses to
/// deliver it. Shared (behind `Arc`) by the [`Session`] and any
/// [`SessionClient`]; whichever drops last closes the fd. `None` rx for
/// non-Agent workloads.
pub(crate) struct AgentConn {
    workload: WorkloadStrategy,
    // The listener delivers the accepted connection fd here exactly once;
    // `fd()` drains it on first use and caches the fd in `fd`.
    rx: Mutex<Option<Receiver<RawFd>>>,
    fd: Mutex<Option<RawFd>>,
    // The response demultiplexer (C4), built lazily on the first request once
    // the fd has been resolved. `init` guards the one-time fallible build so a
    // burst of first requests doesn't race two reader threads onto one fd.
    demux: OnceLock<Demux>,
    init: Mutex<()>,
}

impl AgentConn {
    /// Send a desktop [`Action`] and block for the framed response. Only
    /// valid for Agent-workload connections.
    fn request(&self, action: &Action) -> Result<(ResponseHeader, Vec<u8>), Error> {
        if self.workload != WorkloadStrategy::Agent {
            return Err(Error::Vsock(
                "request() is only valid for Agent-workload sessions".into(),
            ));
        }
        self.demux()?.request(action)
    }

    /// Return the response demux, building it (and spawning its reader thread)
    /// on first use once the agent fd is resolved. The fd resolution blocks up
    /// to [`AGENT_CONNECT_TIMEOUT`] for the guest's outbound connection.
    fn demux(&self) -> Result<&Demux, Error> {
        if let Some(d) = self.demux.get() {
            return Ok(d);
        }
        let _g = self.init.lock().unwrap();
        if let Some(d) = self.demux.get() {
            return Ok(d); // lost the init race
        }
        let fd = self.fd()?;
        let _ = self.demux.set(Demux::start(fd));
        Ok(self.demux.get().unwrap())
    }

    /// Return the cached agent connection fd, blocking on first use until the
    /// listener delivers it (or the connect timeout elapses).
    fn fd(&self) -> Result<RawFd, Error> {
        let mut cached = self.fd.lock().unwrap();
        if let Some(fd) = *cached {
            return Ok(fd);
        }
        let rx_guard = self.rx.lock().unwrap();
        let rx = rx_guard
            .as_ref()
            .ok_or_else(|| Error::Vsock("no agent vsock channel (vsock disabled?)".into()))?;
        let fd = rx
            .recv_timeout(AGENT_CONNECT_TIMEOUT)
            .map_err(|_| Error::Vsock("timed out waiting for the guest agent to connect".into()))?;
        *cached = Some(fd);
        Ok(fd)
    }
}

impl Drop for AgentConn {
    fn drop(&mut self) {
        // Close the agent connection fd we dup'd off the accepted vsock
        // connection. The Echo path closes its own fd at EOF; the Agent
        // path's fd is owned here and closed when the last Arc drops.
        if let Some(fd) = *self.fd.lock().unwrap() {
            unsafe { libc::close(fd) };
        }
        // If the listener delivered a connection we never consumed (an
        // orphaned session where `fd()` was never called), the fd is still
        // buffered in the channel — a bare `RawFd` with no Drop of its own.
        // Drain and close it so the descriptor doesn't leak.
        if let Some(rx) = self.rx.lock().unwrap().take() {
            while let Ok(fd) = rx.try_recv() {
                unsafe { libc::close(fd) };
            }
        }
    }
}

/// Issue a graceful force-stop on the VM's own queue, recording
/// [`SessionEnd::Stopped`]. Shared by [`Session::stop`] and
/// [`StopHandle::stop`]. No-op if the session has already ended.
///
/// The `RcBlock` is constructed *inside* the dispatched closure so the
/// non-`Send` block never crosses the thread boundary — only the `Send`
/// `QueueBound<VM>` and `Arc<EndSlot>` do.
fn issue_stop(queue: &DispatchQueue, vm: &Retained<VZVirtualMachine>, end: &Arc<EndSlot>) {
    if end.is_set() {
        return;
    }
    let vm_for_stop = QueueBound(vm.clone());
    let end_for_stop = end.clone();
    queue.exec_async(move || {
        let stop_cb = RcBlock::new(move |_err: *mut NSError| {
            end_for_stop.set(SessionEnd::Stopped);
        });
        unsafe { vm_for_stop.stopWithCompletionHandler(&stop_cb) };
    });
}

/// Owns a per-session host temp dir used as the writable `ctl` virtio-fs
/// share for block-rootfs sessions. Removed on drop so the side-channel
/// dir doesn't outlive the VM.
struct ControlDirGuard(PathBuf);

impl Drop for ControlDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A unique path under the host temp dir, named `<prefix>-<pid>-<seq>-<nanos>`.
/// PID + a process-local counter + wall-clock nanos keep concurrent sessions in
/// one process (PID collides) and serial sessions under a coarse clock (nanos
/// collides) from colliding. Shared by the per-session temp artifacts below.
fn unique_temp_path(prefix: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{}-{}-{}-{}", prefix, std::process::id(), n, nanos))
}

/// Create a unique host temp dir for the block-rootfs control share.
fn make_control_dir() -> Result<PathBuf, Error> {
    let dir = unique_temp_path("vmette-ctl");
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    Ok(dir)
}

/// Owns the per-session ephemeral scratch disk image (`--scratch`). Removed on
/// drop so the writable-overlay backing store never outlives its VM — the
/// sandbox stays ephemeral, same as the tmpfs path it replaces.
struct ScratchFileGuard(PathBuf);

impl Drop for ScratchFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Create a unique, sparse raw disk image of `mib` MiB to back the guest's
/// writable overlay upper. `set_len` punches a hole (sparse on APFS), so the
/// image costs almost nothing on the host until the guest actually writes into
/// it.
fn make_scratch_image(mib: u64) -> Result<PathBuf, Error> {
    let path = unique_temp_path("vmette-scratch").with_extension("img");
    let f = std::fs::File::create(&path).map_err(Error::Io)?;
    f.set_len(mib.saturating_mul(1024 * 1024))
        .map_err(Error::Io)?;
    Ok(path)
}

/// Cap on captured guest output (matches the prior subprocess/MCP cap). Past
/// this the buffer stops growing (but the pipe keeps draining so the guest never
/// blocks) and a truncation marker is appended once.
const CAPTURE_CAP_BYTES: usize = 1024 * 1024;

/// Owns the host end of the capture pipe (write side; the read side is owned by
/// the reader thread, which closes it on exit). Closed on session drop so the
/// daemon doesn't leak a descriptor per run.
struct CapturePipe {
    write_fd: RawFd,
}

impl Drop for CapturePipe {
    fn drop(&mut self) {
        unsafe { libc::close(self.write_fd) };
    }
}

/// Drain the capture pipe (`read_fd`, owned + closed here), sending output
/// chunks on `tx` until the session ends and a short grace elapses with no
/// further bytes. The channel lets a consumer either **stream** chunks live
/// (the daemon, via [`Session::capture_rx`]) or **buffer** them at the end (the
/// MCP server / [`Session::wait_captured`]). Non-blocking so it can observe `end`
/// rather than block forever on a guest that never closes the console. Bounded
/// by [`CAPTURE_CAP_BYTES`]; keeps reading past the cap so a chatty guest never
/// blocks on a full pipe, but stops *sending* past it (after a one-time marker).
fn drain_capture(read_fd: RawFd, end: Arc<EndSlot>, tx: std::sync::mpsc::Sender<Vec<u8>>) {
    unsafe {
        let fl = libc::fcntl(read_fd, libc::F_GETFL);
        libc::fcntl(read_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut tmp = [0u8; 8192];
    let mut sent: usize = 0;
    let mut truncated = false;
    let mut grace: u32 = 0;
    loop {
        let n = unsafe { libc::read(read_fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n > 0 {
            grace = 0;
            if sent < CAPTURE_CAP_BYTES {
                let take = (n as usize).min(CAPTURE_CAP_BYTES - sent);
                // A send error means the receiver was dropped — keep draining the
                // pipe (so the guest never blocks) but stop sending.
                let _ = tx.send(tmp[..take].to_vec());
                sent += take;
                if take < n as usize && !truncated {
                    truncated = true;
                    let _ = tx.send(b"\n[output truncated at 1048576 bytes]\n".to_vec());
                }
            }
            continue;
        }
        // EOF (0) → done. EAGAIN/error (<0) → stop once the VM ended and a brief
        // grace (~0.5s) has passed with no trailing bytes; else poll on.
        if n == 0 || (end.is_set() && grace >= 25) {
            break;
        }
        if end.is_set() {
            grace += 1;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { libc::close(read_fd) };
    // `tx` drops here → the channel closes, ending any consumer's iteration.
}

/// A booted VM and everything that must outlive its dispatch queue.
pub struct Session {
    vm: Retained<VZVirtualMachine>,
    queue: DispatchRetained<DispatchQueue>,
    end: Arc<EndSlot>,
    vsock_port: Option<u32>,
    cmdline: String,
    agent: Arc<AgentConn>,
    // VZ holds the delegate and socket listener weakly; if either is
    // dropped the callbacks silently stop. We own them for the VM's life.
    _delegate: Retained<VmetteDelegate>,
    _vsock_keepalive: Option<(Retained<VsockLogger>, Retained<VZVirtioSocketListener>)>,
    // Block-rootfs control-share temp dir; removed when the session drops.
    _control_dir: Option<ControlDirGuard>,
    // Ephemeral `--scratch` disk image; removed when the session drops.
    _scratch_file: Option<ScratchFileGuard>,
    // C2 capture: the host pipe write end (closed on drop) + the receiver of
    // output chunks the reader thread sends. Consumed once, by either
    // `wait_captured` (buffer) or `capture_rx` (stream).
    _capture: Option<CapturePipe>,
    capture_rx: Mutex<Option<std::sync::mpsc::Receiver<Vec<u8>>>>,
}

impl Session {
    /// Build the VZ config, create the VM on its own serial queue, install
    /// the delegate / vsock listener / timeout, and call `start`. Returns
    /// once the VM has been asked to start; the boot proceeds on the VM's
    /// queue (a libdispatch worker thread) and the terminal event is observed
    /// via [`Session::wait`].
    ///
    /// Snapshot configs are rejected here — those still go through
    /// [`crate::vz::snapshot`] from [`crate::run`].
    pub fn start(config: &Config) -> Result<Session, Error> {
        let vsock_port = resolve_vsock_port(config.vsock_port);

        // The `ctl` virtio-fs share is ALWAYS attached: it carries the typed
        // boot envelope (`boot.env`, the host→guest config the guest's `/init`
        // sources) and, when the root is writable, the guest's exit code
        // (`.vmette-exit`). It is backed by a per-session host temp dir. Clone
        // the config so this injected share never leaks into the caller's
        // `Config`.
        let mut working = config.clone();
        // "ctl" is reserved. A caller share with the same tag would produce two
        // virtio-fs devices tagged "ctl" and the guest would mount one
        // nondeterministically — silently breaking boot-env/exit-code delivery.
        if config.shares.iter().any(|s| s.tag == "ctl") {
            return Err(Error::InvalidConfig(
                "share tag \"ctl\" is reserved for the boot/exit channel".into(),
            ));
        }
        let ctl_dir = make_control_dir()?;
        working.shares.push(ShareMount {
            tag: "ctl".into(),
            path: ctl_dir.clone(),
        });
        let control_dir = Some(ControlDirGuard(ctl_dir.clone()));
        // The guest writes its exit code into `ctl` only when the root is
        // writable (a block rootfs or an overlaid virtio-fs share). A truly
        // read-only directory rootfs (`--rootfs-ro`) can't and the guest won't —
        // but it still reads `boot.env` from the same share.
        let writable_root = match &config.rootfs {
            Some(crate::Rootfs::Block(_)) => true,
            Some(crate::Rootfs::Share(rs)) => !rs.read_only,
            None => false,
        };
        let exit_code_file = if writable_root {
            let p = ctl_dir.join(".vmette-exit");
            let _ = std::fs::remove_file(&p);
            Some(p)
        } else {
            None
        };

        // Ephemeral scratch disk (`--scratch`): only meaningful when the guest
        // builds a writable overlay (exactly the `writable_root` condition). A
        // read-only rootfs has no overlay upper to back, so the disk would go
        // unused; skip it. The guard deletes the image when the session drops,
        // keeping the sandbox ephemeral.
        let scratch_file = match (writable_root, config.scratch_mib) {
            (true, Some(mib)) => Some(ScratchFileGuard(make_scratch_image(mib)?)),
            _ => None,
        };
        let scratch_path = scratch_file.as_ref().map(|g| g.0.clone());
        // Name the device the same way the guest will see it (attach order in
        // build_vz_config below); `boot.env` carries it so /init knows which
        // block device to format ext4.
        let scratch_dev = scratch_path
            .as_ref()
            .map(|_| cmdline::scratch_device_name(&working));

        let mut cmdline = cmdline::build(&working, vsock_port);

        // C1: write the typed boot envelope the guest's `/init` sources from the
        // `ctl` share. Built from the caller's original `config` (not `working`),
        // so the implicit `ctl` share is excluded from `shares`. `capture_output`
        // rides through `from_config` so the guest redirects the exec to `hvc0`.
        if let Some(guard) = &control_dir {
            let params = crate::boot::from_config(config, scratch_dev.as_deref());
            std::fs::write(guard.0.join("boot.env"), crate::boot::to_env(&params))
                .map_err(Error::Io)?;
        }

        // C2 capture: when the caller wants the guest output captured in-process
        // (daemon/MCP), wire `hvc0` to a host pipe and push the kernel console +
        // `/init` chatter to a discarded `hvc1` (`console=hvc1`). A reader thread
        // drains the pipe into a bounded buffer for [`Session::wait_captured`].
        // Otherwise the single console inherits the host terminal.
        // `capture` carries (read_fd for the reader thread, CapturePipe owning
        // the write end). `None` when not capturing.
        let capture: Option<(RawFd, CapturePipe)> = if config.capture_output {
            let mut fds: [libc::c_int; 2] = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            // Move the kernel console off hvc0 so it stays clean for the exec.
            cmdline = cmdline.replace("console=hvc0", "console=hvc1");
            Some((fds[0], CapturePipe { write_fd: fds[1] }))
        } else {
            None
        };
        let sink = match &capture {
            Some((_, p)) => SerialSink::Capture {
                write_fd: p.write_fd,
            },
            None => SerialSink::Inherit,
        };

        let cfg = build_vz_config(
            &working,
            &cmdline,
            vsock_port,
            scratch_path.as_deref(),
            sink,
        )?;

        // Private serial queue for this VM. libdispatch services it on its
        // worker pool, so all VZ callbacks fire there without a run loop, and
        // many sessions can run concurrently (each on its own queue).
        let queue = DispatchQueue::new("com.chamuka.vmette.session", None);
        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &cfg, &queue)
        };

        let end = EndSlot::new();
        let timed_out = Arc::new(AtomicBool::new(false));

        // Start draining the capture pipe now (before the VM runs) so the guest
        // never blocks on a full pipe. The reader owns `read_fd`, sends output
        // chunks on a channel, and stops once `end` is recorded; `Session` keeps
        // the `write_fd` (CapturePipe) alive for the VM's lifetime.
        let capture_rx = match &capture {
            Some((read_fd, _)) => {
                let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
                let end_for_reader = end.clone();
                let read_fd = *read_fd;
                std::thread::spawn(move || drain_capture(read_fd, end_for_reader, tx));
                Some(rx)
            }
            None => None,
        };

        let delegate = VmetteDelegate::new(DelegateState {
            exit_code_file,
            timed_out: timed_out.clone(),
            end: end.clone(),
        });

        // Vsock listener (guest-initiated connections). Kept alive on the
        // Session so it outlives the queue. The mode depends on the workload:
        // Agent hands the accepted fd to this Session for the framed desktop
        // protocol; everything else logs + echoes.
        let mut agent_rx = None;
        let vsock_keepalive = if let Some(port) = vsock_port {
            let mode = match config.workload {
                WorkloadStrategy::Agent => {
                    let (tx, rx) = sync_channel::<RawFd>(1);
                    agent_rx = Some(rx);
                    ListenerMode::Agent {
                        fd_tx: Mutex::new(Some(tx)),
                    }
                }
                WorkloadStrategy::OneShot => ListenerMode::Echo {
                    ready_handler: Arc::new(Mutex::new(None)),
                },
            };
            let logger = VsockLogger::new(ListenerState { port, mode });
            let listener = unsafe { VZVirtioSocketListener::new() };
            unsafe {
                listener.setDelegate(Some(ProtocolObject::from_ref(&*logger)));
            }
            Some((logger, listener))
        } else {
            None
        };

        let agent = Arc::new(AgentConn {
            workload: config.workload,
            rx: Mutex::new(agent_rx),
            fd: Mutex::new(None),
            demux: OnceLock::new(),
            init: Mutex::new(()),
        });

        // All VM mutation (setDelegate, setSocketListener, start, the timeout
        // stop, …) must happen on the VM's queue. Do the synchronous setup on
        // it via exec_sync so it is ordered before start.
        let setup_vm = QueueBound(vm.clone());
        let setup_delegate = QueueBound(delegate.clone());
        let setup_listener = vsock_keepalive
            .as_ref()
            .map(|(_, l)| (QueueBound(l.clone()), vsock_port.unwrap_or(0)));
        queue.exec_sync(move || unsafe {
            let proto: &ProtocolObject<dyn VZVirtualMachineDelegate> =
                ProtocolObject::from_ref(&*setup_delegate.0);
            setup_vm.setDelegate(Some(proto));
            if let Some((listener, port)) = &setup_listener {
                let sock_dev = setup_vm.socketDevices();
                if let Some(dev) = sock_dev.firstObject() {
                    let dev: Retained<VZVirtioSocketDevice> = Retained::cast_unchecked(dev);
                    dev.setSocketListener_forPort(listener, *port);
                }
            }
        });

        // Timeout: force-stop the VM and record TimedOut.
        if let Some(secs) = config.timeout_seconds {
            let vm_for_timer = QueueBound(vm.clone());
            let timed_out_setter = timed_out.clone();
            let end_for_timer = end.clone();
            let when = DispatchTime::try_from(Duration::from_secs(secs as u64))
                .unwrap_or(DispatchTime::NOW);
            let _ = queue.after(when, move || {
                timed_out_setter.store(true, Ordering::SeqCst);
                let end_for_stop = end_for_timer.clone();
                let stop_cb = RcBlock::new(move |_err: *mut NSError| {
                    end_for_stop.set(SessionEnd::TimedOut);
                });
                unsafe { vm_for_timer.stopWithCompletionHandler(&stop_cb) };
            });
        }

        // Start on the VM's queue. A start failure is reported through the
        // same EndSlot so wait() returns Error rather than blocking forever.
        let vm_for_start = QueueBound(vm.clone());
        let end_for_start = end.clone();
        queue.exec_async(move || {
            let start_cb = RcBlock::new(move |err: *mut NSError| {
                if !err.is_null() {
                    let err = unsafe { &*err };
                    end_for_start.set(SessionEnd::Error(format!(
                        "vm.start failed: {}",
                        err.localizedDescription()
                    )));
                }
            });
            unsafe { vm_for_start.startWithCompletionHandler(&start_cb) };
        });

        Ok(Session {
            vm,
            queue,
            end,
            vsock_port,
            cmdline,
            agent,
            _delegate: delegate,
            _vsock_keepalive: vsock_keepalive,
            _control_dir: control_dir,
            _scratch_file: scratch_file,
            _capture: capture.map(|(_, p)| p),
            capture_rx: Mutex::new(capture_rx),
        })
    }

    /// The resolved vsock host port (`None` if vsock is disabled). Stable
    /// for the session's lifetime — read it for banners/logging instead of
    /// re-resolving, which would re-randomize an `Auto` port.
    pub fn vsock_port(&self) -> Option<u32> {
        self.vsock_port
    }

    /// The assembled kernel command line.
    pub fn cmdline(&self) -> &str {
        &self.cmdline
    }

    /// Block until the session ends, then return how it ended. Safe to call
    /// after the end was already recorded (returns immediately). Does not
    /// pump any run loop — the VM runs on its own dispatch queue.
    pub fn wait(&self) -> SessionEnd {
        self.end.wait_end()
    }

    /// Block until the session ends, then return the exit code plus the captured
    /// guest output (combined stdout+stderr) as a [`RunOutput`]. Only meaningful
    /// for a session started with [`Config::capture_output`](crate::Config::capture_output);
    /// otherwise `output` is empty. Drains the capture channel to completion
    /// (which closes once the reader has flushed all trailing output around
    /// poweroff), so every byte is included. Buffered — for live streaming use
    /// [`Session::capture_rx`] instead.
    pub fn wait_captured(&self) -> crate::RunOutput {
        let end = self.end.wait_end();
        let mut out = Vec::new();
        if let Some(rx) = self.capture_rx.lock().unwrap().take() {
            for chunk in rx {
                out.extend_from_slice(&chunk);
            }
        }
        let exit_code = match end {
            SessionEnd::Exited(code) => code,
            SessionEnd::TimedOut => 124,
            SessionEnd::Stopped => 0,
            SessionEnd::Error(_) => 1,
        };
        // The guest console is a tty (ONLCR), so every `\n` arrives as `\r\n`.
        // Normalize on the fully-buffered string (safe — no chunk boundaries) so
        // the captured output is clean LF, matching the prior subprocess path's
        // console handling. Lone `\r` (e.g. progress redraws) is left intact.
        crate::RunOutput {
            exit_code,
            output: String::from_utf8_lossy(&out).replace("\r\n", "\n"),
        }
    }

    /// Take the capture channel for **streaming** the guest's output chunks live
    /// as the VM runs (the daemon forwards them as `Frame::Stdout`). Each item is
    /// a chunk of combined stdout+stderr bytes; iteration ends when the session
    /// has fully ended and all trailing output is flushed. Returns `None` if the
    /// session was not started with `capture_output`, or if the channel was
    /// already taken (by an earlier `capture_rx`/`wait_captured`). The caller
    /// reads the exit code via [`Session::wait`] after the stream ends.
    pub fn capture_rx(&self) -> Option<std::sync::mpsc::Receiver<Vec<u8>>> {
        self.capture_rx.lock().unwrap().take()
    }

    /// Request a graceful force-stop of the guest. The stop completes on the
    /// VM's queue and records [`SessionEnd::Stopped`], unblocking a concurrent
    /// [`Session::wait`]. No-op if the session has already ended.
    pub fn stop(&self) {
        issue_stop(&self.queue, &self.vm, &self.end);
    }

    /// Send a desktop [`Action`] to the in-guest agent and block for its
    /// response (a [`ResponseHeader`] plus an optional binary payload, e.g.
    /// a PNG for [`Action::Screenshot`]). See [`SessionClient::request`].
    pub fn request(&self, action: &Action) -> Result<(ResponseHeader, Vec<u8>), Error> {
        self.agent.request(action)
    }

    /// Extract a `Send` client for issuing desktop requests from another
    /// thread (the daemon hands these to its async request handlers). Shares
    /// the agent connection with the `Session`; the fd is closed once the
    /// last holder drops.
    pub fn client(&self) -> SessionClient {
        SessionClient {
            agent: self.agent.clone(),
        }
    }

    /// Extract a `Send` handle for stopping the session from another thread.
    /// Stopping unblocks the thread that owns the `Session` in
    /// [`Session::wait`], which then tears the VM down by dropping it.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            vm: QueueBound(self.vm.clone()),
            queue: self.queue.clone(),
            end: self.end.clone(),
        }
    }
}

/// `Send` handle for issuing desktop [`Action`]s against a live session from
/// a thread other than the one that owns the [`Session`]. The data path is
/// plain blocking `read`/`write` on the accepted vsock fd, independent of the
/// VM's dispatch queue.
#[derive(Clone)]
pub struct SessionClient {
    agent: Arc<AgentConn>,
}

impl SessionClient {
    /// Send an [`Action`] and block for the framed response. The first call
    /// blocks up to [`AGENT_CONNECT_TIMEOUT`] for the agent's outbound vsock
    /// connection; subsequent calls reuse the cached fd. Only valid for
    /// [`WorkloadStrategy::Agent`] sessions.
    pub fn request(&self, action: &Action) -> Result<(ResponseHeader, Vec<u8>), Error> {
        self.agent.request(action)
    }
}

/// `Send` handle for stopping a live session from another thread. Holds a
/// reference to the VM and its queue; all fields are `Send`, so the handle is
/// `Send` without an explicit unsafe impl.
pub struct StopHandle {
    vm: QueueBound<VZVirtualMachine>,
    queue: DispatchRetained<DispatchQueue>,
    end: Arc<EndSlot>,
}

impl StopHandle {
    /// Request a graceful force-stop. No-op if the session has already ended.
    pub fn stop(&self) {
        issue_stop(&self.queue, &self.vm.0, &self.end);
    }
}

/// Borrowed-fd `Read`/`Write` adapter for the framed desktop protocol. Does
/// **not** own or close the fd — the [`AgentConn`] owns the agent connection
/// fd and closes it on drop.
struct FdStream(RawFd);

impl std::io::Read for FdStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl std::io::Write for FdStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::write(self.0, buf.as_ptr() as *const _, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

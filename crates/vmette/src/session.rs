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

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};
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
use crate::vz::config::{build as build_vz_config, resolve_vsock_port};
use crate::vz::delegate::{DelegateState, VmetteDelegate};
use crate::vz::vsock::{ListenerMode, ListenerState, VsockLogger};
use crate::{cmdline, Config, ShareMount, WorkloadStrategy};

/// How long [`SessionClient::request`] waits for the in-guest agent to make
/// its outbound vsock connection before giving up. The desktop image boots
/// Xvfb + WM + agent, which can take several seconds on first run.
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-read timeout on the agent vsock fd. Bounds how long a single framed
/// round-trip can stall on a wedged guest before the read errors out, so the
/// blocking thread issuing the request can't hang forever. Generous: a
/// software-rendered screenshot frame can be slow, but data flowing resets the
/// timer per read syscall, so this only trips when the guest stops responding.
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
    // Serializes the request round-trip. The framed protocol is a single
    // request/response stream with no multiplexing, so concurrent callers
    // (the `SessionClient` handle is `Clone` and shared via `Arc`) must not
    // interleave their `[len][header][payload]` frames on the one fd.
    io: Mutex<()>,
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
        // Resolve the fd outside the `io` lock: the first call blocks up to
        // AGENT_CONNECT_TIMEOUT for the guest to connect, and that wait must
        // not serialize unrelated callers. `fd()` has its own lock.
        let fd = self.fd()?;
        // Hold `io` across the round-trip so concurrent requests on cloned
        // `SessionClient`s cannot interleave frames on the shared fd.
        let _io = self.io.lock().unwrap();
        let mut stream = FdStream(fd);
        let outcome = desktop::send_action(&mut stream, action)
            .and_then(|()| desktop::read_response(&mut stream));
        match outcome {
            Ok((header, payload)) => Ok((header, payload)),
            Err(e) => {
                // A failed or timed-out round-trip can leave a partial frame
                // buffered on the socket; reusing the fd would desync every
                // later request (stale bytes parsed as the next header).
                // Invalidate it so the session fails cleanly instead.
                self.invalidate_fd();
                Err(e.into())
            }
        }
    }

    /// Close and forget the cached agent fd after an I/O failure, so the next
    /// request doesn't read a stale half-frame off a desynced socket.
    fn invalidate_fd(&self) {
        if let Some(fd) = self.fd.lock().unwrap().take() {
            unsafe { libc::close(fd) };
        }
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
        // Bound subsequent reads so a wedged guest can't hang the blocking
        // thread issuing a request indefinitely (`FdStream::read` has no
        // timeout of its own).
        set_recv_timeout(fd, AGENT_READ_TIMEOUT);
        *cached = Some(fd);
        Ok(fd)
    }
}

/// Best-effort `SO_RCVTIMEO` on a socket fd. A failure here only costs us the
/// read-timeout safety net, so we don't surface it as an error.
fn set_recv_timeout(fd: RawFd, dur: Duration) {
    let tv = libc::timeval {
        tv_sec: dur.as_secs() as libc::time_t,
        tv_usec: dur.subsec_micros() as libc::suseconds_t,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
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

        // The guest root is never host-writable: a block rootfs is overlaid
        // with a tmpfs, and a directory rootfs is now mounted read-only on the
        // host + overlaid with a tmpfs in the guest (per-session isolation). So
        // whenever there's a writable workload, the guest has no host-visible
        // surface to drop `.vmette-exit` into. Auto-attach a writable "ctl"
        // virtio-fs share backed by a per-session host temp dir; the guest
        // writes its exit code there and the host reads it back. Clone the
        // config so this injected share never leaks into the caller's `Config`.
        //
        // A truly read-only directory rootfs (`--rootfs-ro`) gets no exit
        // channel — it can't write one and the guest can't either.
        let mut working = config.clone();
        let mut control_dir: Option<ControlDirGuard> = None;
        let needs_ctl = config.rootfs_block.is_some()
            || config.rootfs_share.as_ref().is_some_and(|rs| !rs.read_only);
        let exit_code_file = if needs_ctl {
            // "ctl" is reserved for the exit channel injected below. A caller
            // share with the same tag would produce two virtio-fs devices
            // tagged "ctl" and the guest would mount one of them at /mnt/ctl
            // nondeterministically — silently breaking exit-code read-back.
            // Reject loudly instead.
            if config.shares.iter().any(|s| s.tag == "ctl") {
                return Err(Error::InvalidConfig(
                    "share tag \"ctl\" is reserved for the rootfs exit channel".into(),
                ));
            }
            let dir = make_control_dir()?;
            let p = dir.join(".vmette-exit");
            let _ = std::fs::remove_file(&p);
            working.shares.push(ShareMount {
                tag: "ctl".into(),
                path: dir.clone(),
            });
            control_dir = Some(ControlDirGuard(dir));
            Some(p)
        } else {
            None
        };

        // Ephemeral scratch disk (`--scratch`): only meaningful when the guest
        // builds a writable overlay (`needs_ctl` is exactly that condition — a
        // block rootfs or a writable directory rootfs). A read-only rootfs has
        // no overlay upper to back, so the disk would go unused; skip it. The
        // guard deletes the image when the session drops, keeping the sandbox
        // ephemeral.
        let scratch_file = match (needs_ctl, config.scratch_mib) {
            (true, Some(mib)) => Some(ScratchFileGuard(make_scratch_image(mib)?)),
            _ => None,
        };
        let scratch_path = scratch_file.as_ref().map(|g| g.0.clone());
        // Name the device the same way the guest will see it (attach order in
        // build_vz_config below); the cmdline carries it so /init knows which
        // block device to format ext4.
        let scratch_dev = scratch_path
            .as_ref()
            .map(|_| cmdline::scratch_device_name(&working));

        let cmdline = cmdline::build(&working, vsock_port, scratch_dev.as_deref());

        // C1 (phase 1b, additive): write the typed boot envelope to the `ctl`
        // share alongside the legacy `vmette.*` cmdline tokens. The guest still
        // boots from the cmdline for now; `boot.env` is laid down so the
        // guest-side switch (and the cmdline shrink) can follow without changing
        // the host write path again. Built from the caller's original `config`
        // (not `working`), so the implicit `ctl` share is excluded from `shares`.
        if let Some(guard) = &control_dir {
            let params = crate::boot::from_config(config, scratch_dev.as_deref());
            std::fs::write(guard.0.join("boot.env"), crate::boot::to_env(&params))
                .map_err(Error::Io)?;
        }

        let cfg = build_vz_config(&working, &cmdline, vsock_port, scratch_path.as_deref())?;

        // Private serial queue for this VM. libdispatch services it on its
        // worker pool, so all VZ callbacks fire there without a run loop, and
        // many sessions can run concurrently (each on its own queue).
        let queue = DispatchQueue::new("com.chamuka.vmette.session", None);
        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &cfg, &queue)
        };

        let end = EndSlot::new();
        let timed_out = Arc::new(AtomicBool::new(false));

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
            io: Mutex::new(()),
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

//! `VZVirtioSocketListenerDelegate` implementation. Accepts guest-initiated
//! vsock connections; behavior depends on the [`ListenerMode`]:
//!
//! - [`ListenerMode::Echo`] (snapshot/one-shot): logs incoming bytes to host
//!   stderr (tagged with the port), echoes them back so the guest's caller
//!   unblocks, and fires a one-shot `ready_handler` when the guest writes the
//!   `READY\n` sentinel (snapshot-build mode).
//! - [`ListenerMode::Agent`] (desktop): hands the dup'd connection fd to the
//!   owning [`crate::Session`] over a channel and does *not* echo or read —
//!   the session drives the framed [`crate::desktop`] protocol on that fd.

use std::os::fd::RawFd;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use dispatch2::{DispatchQoS, DispatchQueue, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate,
};

pub(crate) type ReadyHandler = Box<dyn FnOnce() + Send + 'static>;

/// What the listener does with an accepted connection.
pub(crate) enum ListenerMode {
    /// Log + echo bytes; fire `ready_handler` once on `READY\n`.
    Echo {
        /// Snapshot-build READY handler. Shared via Arc so connection
        /// closures get clones; only the closure that actually observes
        /// `READY\n` in its byte stream consumes the handler. A short-lived
        /// probe connection that closes without sending READY no longer
        /// loses the handler for the next, real connection.
        ready_handler: Arc<Mutex<Option<ReadyHandler>>>,
    },
    /// Hand the dup'd fd to the session; do not read or echo. Sent at most
    /// once (the agent makes a single long-lived connection); the `Option`
    /// is taken on first accept so a reconnect can't double-send.
    Agent {
        fd_tx: Mutex<Option<SyncSender<RawFd>>>,
    },
}

pub(crate) struct ListenerState {
    pub port: u32,
    pub mode: ListenerMode,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = ListenerState]
    #[name = "VmetteVsockLogger"]
    pub(crate) struct VsockLogger;

    unsafe impl NSObjectProtocol for VsockLogger {}

    unsafe impl VZVirtioSocketListenerDelegate for VsockLogger {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> Bool {
            let raw_fd = unsafe { connection.fileDescriptor() };
            // Dup so the connection can be released while we keep the fd.
            let fd = unsafe { libc::dup(raw_fd) };
            if fd < 0 {
                return Bool::YES;
            }
            let port = self.ivars().port;
            eprintln!("\r\n[vsock] guest connected on port {} (fd={})\r", port, fd);

            match &self.ivars().mode {
                ListenerMode::Agent { fd_tx } => {
                    // Hand the fd to the session. Take the sender so a
                    // reconnect can't deliver a second fd to a one-shot
                    // receiver. If the session already dropped the receiver
                    // (torn down), close the fd to avoid a leak.
                    let sender = fd_tx.lock().ok().and_then(|mut g| g.take());
                    match sender {
                        Some(tx) => {
                            if tx.send(fd).is_err() {
                                unsafe { libc::close(fd) };
                            }
                        }
                        None => unsafe {
                            libc::close(fd);
                        },
                    }
                }
                ListenerMode::Echo { ready_handler } => {
                    // Clone the shared handler Arc; only consume from inside
                    // the read loop, and only when we actually observe
                    // `READY\n`. A connection that ends without READY does
                    // NOT drop the handler.
                    let ready_handler = Arc::clone(ready_handler);
                    let queue = DispatchQueue::global_queue(
                        GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility),
                    );
                    queue.exec_async(move || echo_loop(fd, port, ready_handler));
                }
            }

            Bool::YES
        }
    }
);

/// The snapshot/one-shot echo + READY-detection read loop. Owns `fd` and
/// closes it on EOF.
fn echo_loop(fd: RawFd, port: u32, ready_handler: Arc<Mutex<Option<ReadyHandler>>>) {
    // Sliding tail across reads so a READY split across two libc::read
    // calls is still detected. Fixed-size stack buffer; no per-iteration
    // allocation.
    const NEEDLE: &[u8] = b"READY\n";
    let mut tail: [u8; 5] = [0; 5]; // NEEDLE.len() - 1
    let mut tail_len: usize = 0;

    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            break;
        }
        let slice = &buf[..n as usize];

        // READY detection (one-shot, for snapshot build mode). Two cheap
        // passes: (1) scan a tiny carry+head window for the boundary case,
        // (2) scan slice itself. Either hit consumes the handler.
        let mut bridge = [0u8; 11]; // tail.len() + NEEDLE.len() - 1
        let take = (NEEDLE.len() - 1).min(slice.len());
        bridge[..tail_len].copy_from_slice(&tail[..tail_len]);
        bridge[tail_len..tail_len + take].copy_from_slice(&slice[..take]);
        let bridge_hit = memchr_seq(&bridge[..tail_len + take], NEEDLE);
        let slice_hit = memchr_seq(slice, NEEDLE);
        if bridge_hit || slice_hit {
            let h_opt = ready_handler.lock().ok().and_then(|mut g| g.take());
            if let Some(h) = h_opt {
                DispatchQueue::main().exec_async(h);
            }
        }
        // Carry forward at most NEEDLE.len()-1 bytes for the next read's
        // bridge. The carry must be the suffix of (tail + slice) combined —
        // if slice is shorter than the carry window, we'd otherwise drop
        // prior tail bytes and miss a needle split across 3+ reads.
        let combined_len = tail_len + slice.len();
        let keep = (NEEDLE.len() - 1).min(combined_len);
        let mut new_tail = [0u8; 5];
        // Source position into the conceptual `tail ++ slice` window.
        let src_start = combined_len - keep;
        for (i, slot) in new_tail.iter_mut().enumerate().take(keep) {
            let src_pos = src_start + i;
            *slot = if src_pos < tail_len {
                tail[src_pos]
            } else {
                slice[src_pos - tail_len]
            };
        }
        tail = new_tail;
        tail_len = keep;

        // Log to host stderr.
        eprint!("[vsock {}] ", port);
        // SAFETY: writing arbitrary bytes is fine for stderr.
        use std::io::Write;
        let _ = std::io::stderr().write_all(slice);
        if *slice.last().unwrap_or(&b' ') != b'\n' {
            eprintln!();
        }

        // Echo back so guest unblocks.
        let mut off = 0usize;
        while off < slice.len() {
            let w =
                unsafe { libc::write(fd, slice[off..].as_ptr() as *const _, slice.len() - off) };
            if w < 0 {
                break;
            }
            off += w as usize;
        }
    }
    unsafe { libc::close(fd) };
    eprintln!("[vsock {}] EOF\r", port);
}

impl VsockLogger {
    pub(crate) fn new(state: ListenerState) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

fn memchr_seq(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

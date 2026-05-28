//! `VZVirtioSocketListenerDelegate` implementation. Accepts guest-
//! initiated vsock connections, logs incoming bytes to host stderr
//! (tagged with the port), echoes them back so the guest's caller
//! unblocks, and — for snapshot-build mode — fires a `ready_handler`
//! block once when the guest writes the `READY\n` sentinel.

use std::sync::Mutex;

use dispatch2::{DispatchQoS, DispatchQueue, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate,
};

pub(crate) type ReadyHandler = Box<dyn FnOnce() + Send + 'static>;

pub(crate) struct ListenerState {
    pub port: u32,
    pub ready_handler: Mutex<Option<ReadyHandler>>,
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
            // Dup so the connection can be released while we keep reading.
            let fd = unsafe { libc::dup(raw_fd) };
            if fd < 0 {
                return Bool::YES;
            }
            let port = self.ivars().port;
            eprintln!("\r\n[vsock] guest connected on port {} (fd={})\r", port, fd);

            // Move the ready handler out (one-shot).
            let ready_taken = self
                .ivars()
                .ready_handler
                .lock()
                .ok()
                .and_then(|mut g| g.take());

            let queue = DispatchQueue::global_queue(
                GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility),
            );
            queue.exec_async(move || {
                let mut ready = ready_taken;
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
                    if n <= 0 {
                        break;
                    }
                    let slice = &buf[..n as usize];

                    // READY detection (one-shot, for snapshot build mode).
                    if let Some(h) = ready.take() {
                        if memchr_seq(slice, b"READY\n") {
                            // Dispatch on main so VM ops happen on the VM's queue.
                            DispatchQueue::main().exec_async(move || h());
                        }
                        // (a miss leaves us without a handler; that's
                        // OK — the next connection's first read will
                        // re-take None and skip, which is the intended
                        // one-shot semantics.)
                    }

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
                        let w = unsafe {
                            libc::write(fd, slice[off..].as_ptr() as *const _, slice.len() - off)
                        };
                        if w < 0 {
                            break;
                        }
                        off += w as usize;
                    }
                }
                unsafe { libc::close(fd) };
                eprintln!("[vsock {}] EOF\r", port);
            });

            Bool::YES
        }
    }
);

impl VsockLogger {
    pub(crate) fn new(state: ListenerState) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

fn memchr_seq(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

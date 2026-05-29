//! `VZVirtualMachineDelegate` implementation: observes guest lifecycle,
//! reads the `/.vmette-exit` file written by the guest's `/init`, and
//! records the terminal state into the session's [`EndSlot`] (which also
//! stops the run loop). It no longer exits the process — that is the
//! caller's job, so the same delegate works for the in-process daemon.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::NSError;
use objc2_virtualization::{VZVirtualMachine, VZVirtualMachineDelegate};

use crate::session::{EndSlot, SessionEnd};

/// State attached to the delegate via objc2 ivars.
pub(crate) struct DelegateState {
    pub exit_code_file: Option<PathBuf>,
    pub timed_out: Arc<AtomicBool>,
    pub end: Arc<EndSlot>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = DelegateState]
    #[name = "VmetteDelegate"]
    pub(crate) struct VmetteDelegate;

    unsafe impl NSObjectProtocol for VmetteDelegate {}

    unsafe impl VZVirtualMachineDelegate for VmetteDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            let state = self.ivars();
            // A timeout-initiated stop already (or will) record TimedOut;
            // first-writer-wins in EndSlot means the exit code is ignored.
            if state.timed_out.load(Ordering::SeqCst) {
                state.end.set(SessionEnd::TimedOut);
                return;
            }
            let code = read_exit_file(state.exit_code_file.as_deref());
            state.end.set(SessionEnd::Exited(code));
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, err: &NSError) {
            let state = self.ivars();
            state
                .end
                .set(SessionEnd::Error(err.localizedDescription().to_string()));
        }
    }
);

impl VmetteDelegate {
    pub(crate) fn new(state: DelegateState) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

/// Read the propagated guest exit code.
///
/// Semantics:
/// - `None` path (RO rootfs, no place for /init to write):
///   we have no signal, so report success (0). The caller knew this
///   trade-off when they passed --rootfs-ro.
/// - Missing file in writable mode: guest crashed before /init's
///   writeback. Report 1 with a warning — silent success would mask
///   the crash.
/// - File present but unparseable: same — corrupt or truncated write
///   from a partial crash; warn and return 1.
fn read_exit_file(path: Option<&std::path::Path>) -> i32 {
    let Some(p) = path else { return 0 };
    match std::fs::read_to_string(p) {
        Ok(s) => match s.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "\r\n[vmette] warning: .vmette-exit unparseable ({:?}); reporting 1\r",
                    s.trim()
                );
                1
            }
        },
        Err(_) => {
            eprintln!(
                "\r\n[vmette] warning: .vmette-exit missing (guest likely crashed); reporting 1\r"
            );
            1
        }
    }
}

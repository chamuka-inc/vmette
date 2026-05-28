//! `VZVirtualMachineDelegate` implementation: observes guest lifecycle,
//! reads the `/.vmette-exit` file written by the guest's `/init`, and
//! exits the host process with the propagated code.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::NSError;
use objc2_virtualization::{VZVirtualMachine, VZVirtualMachineDelegate};

use crate::terminal::restore_terminal;

/// State attached to the delegate via objc2 ivars.
pub(crate) struct DelegateState {
    pub exit_code_file: Option<PathBuf>,
    pub timed_out: Arc<AtomicBool>,
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
            restore_terminal();
            let state = self.ivars();
            if state.timed_out.load(Ordering::SeqCst) {
                eprintln!("\r\n[vmette] guest stopped (timeout, exit 124)\r");
                std::process::exit(124);
            }
            let code = read_exit_file(state.exit_code_file.as_deref());
            eprintln!("\r\n[vmette] guest stopped (exit {})\r", code);
            std::process::exit(code);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, err: &NSError) {
            restore_terminal();
            let msg = err.localizedDescription();
            eprintln!("\r\n[vmette] guest stopped with error: {}\r", msg);
            std::process::exit(1);
        }
    }
);

impl VmetteDelegate {
    pub(crate) fn new(state: DelegateState) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

fn read_exit_file(path: Option<&std::path::Path>) -> i32 {
    let Some(p) = path else { return 0 };
    let Ok(s) = std::fs::read_to_string(p) else { return 0 };
    s.trim().parse().unwrap_or(0)
}

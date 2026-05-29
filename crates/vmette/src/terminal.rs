//! Terminal raw mode + signal handlers. We put stdin into cbreak/raw so
//! keystrokes flow into the guest virtio-console without line buffering
//! or local echo; on exit (clean or via signal) we restore the saved
//! termios.

use nix::sys::termios::{self, SetArg, Termios};
use std::sync::Mutex;

static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Put stdin into raw mode if it's a tty. No-op otherwise. Idempotent.
pub(crate) fn enter_raw_mode() {
    let fd = libc::STDIN_FILENO;
    // SAFETY: STDIN_FILENO is a valid fd; isatty has no preconditions.
    if unsafe { libc::isatty(fd) } == 0 {
        return;
    }
    let Ok(current) = termios::tcgetattr(unsafe { borrowed(fd) }) else {
        return;
    };
    {
        let mut g = SAVED.lock().unwrap();
        if g.is_none() {
            *g = Some(current.clone());
        }
    }
    let mut raw = current;
    termios::cfmakeraw(&mut raw);
    let _ = termios::tcsetattr(unsafe { borrowed(fd) }, SetArg::TCSANOW, &raw);
}

/// Restore the saved termios. Idempotent; safe from signal handlers
/// (only takes the lock if not held).
pub(crate) fn restore_terminal() {
    let saved = match SAVED.try_lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    if let Some(t) = saved {
        let fd = libc::STDIN_FILENO;
        let _ = termios::tcsetattr(unsafe { borrowed(fd) }, SetArg::TCSANOW, &t);
    }
}

/// Install handlers for SIGINT/SIGTERM/SIGHUP that restore the terminal
/// and exit. Cleans up termios before the process dies.
pub(crate) fn install_signal_handlers() {
    extern "C" fn on_signal(sig: libc::c_int) {
        restore_terminal();
        // SAFETY: _exit is async-signal-safe.
        unsafe { libc::_exit(128 + sig) }
    }
    unsafe {
        let h = on_signal as *const () as usize;
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGHUP, h);
        libc::atexit(on_atexit_wrap);
    }
}

extern "C" fn on_atexit_wrap() {
    restore_terminal();
}

// Helper to construct a BorrowedFd from STDIN_FILENO. nix 0.29 requires
// BorrowedFd inputs to its termios calls.
unsafe fn borrowed(fd: libc::c_int) -> std::os::fd::BorrowedFd<'static> {
    std::os::fd::BorrowedFd::borrow_raw(fd)
}

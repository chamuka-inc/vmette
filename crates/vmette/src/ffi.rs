//! C ABI for the vmette library.
//!
//! `cbindgen` reads this file at build time and generates `include/vmette.h`.
//! Non-Rust callers link against `libvmette.dylib` (cdylib) or
//! `libvmette.a` (staticlib) and `#include "vmette.h"`.
//!
//! Conventions:
//!
//! * Opaque types are `#[repr(C)] struct { _private: [u8; 0] }`. Callers
//!   only ever see them as pointers.
//! * Strings are null-terminated `*const c_char` (UTF-8 expected).
//! * Functions returning a status code use the [`VmetteStatus`] enum,
//!   serialized as `int32_t` over the ABI.
//! * Ownership is explicit via paired `*_new` / `*_free`.
//!
//! ## Safety (shared contract)
//!
//! Every function here is `unsafe` because it dereferences raw pointers
//! crossing the C ABI. Unless a function's own `# Safety` note says otherwise,
//! callers must uphold:
//!
//! * Handle pointers (`*mut`/`*const vmette_config_t`, `vmette_run_output_t`)
//!   are either NULL (handled gracefully) or a live value returned by the
//!   matching `*_new`/`vmette_run`, not yet passed to a `*_free`.
//! * `*const c_char` arguments are either NULL or point to a NUL-terminated,
//!   readable string (UTF-8 expected; invalid UTF-8 is rejected, not UB).
//! * No referenced pointer is mutated by another thread for the duration of
//!   the call.

use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::ptr;

use crate::{Config, Error, RootfsShare, RunOutput, ShareMount, VsockPort};

/// Opaque handle to a [`Config`].
#[repr(C)]
pub struct vmette_config_t {
    _private: [u8; 0],
}

/// Opaque handle to a [`RunOutput`].
#[repr(C)]
pub struct vmette_run_output_t {
    _private: [u8; 0],
}

/// Status codes returned by C-ABI functions.
#[repr(i32)]
pub enum VmetteStatus {
    Ok = 0,
    InvalidConfig = 1,
    StartFailed = 2,
    RestoreFailed = 3,
    SaveFailed = 4,
    SnapshotUnsupported = 5,
    Timeout = 6,
    Vsock = 7,
    Io = 8,
    NullArg = 9,
    InvalidUtf8 = 10,
}

impl From<&Error> for VmetteStatus {
    fn from(e: &Error) -> Self {
        match e {
            Error::InvalidConfig(_) => Self::InvalidConfig,
            Error::StartFailed(_) => Self::StartFailed,
            Error::RestoreFailed(_) => Self::RestoreFailed,
            Error::SaveFailed(_) => Self::SaveFailed,
            Error::SnapshotUnsupported => Self::SnapshotUnsupported,
            Error::Timeout(_) => Self::Timeout,
            Error::Vsock(_) => Self::Vsock,
            Error::Io(_) => Self::Io,
        }
    }
}

// ---- helpers ------------------------------------------------------------

unsafe fn cstr_to_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(String::from)
}

unsafe fn cstr_to_pathbuf(p: *const c_char) -> Option<PathBuf> {
    cstr_to_string(p).map(PathBuf::from)
}

unsafe fn cfg_mut<'a>(p: *mut vmette_config_t) -> Option<&'a mut Config> {
    if p.is_null() {
        return None;
    }
    Some(&mut *(p as *mut Config))
}

unsafe fn cfg_ref<'a>(p: *const vmette_config_t) -> Option<&'a Config> {
    if p.is_null() {
        return None;
    }
    Some(&*(p as *const Config))
}

// ---- constructors / destructors ----------------------------------------

/// Construct a new config with the minimum required fields. Returns NULL
/// on null arguments or invalid UTF-8.
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_new(
    kernel: *const c_char,
    initramfs: *const c_char,
) -> *mut vmette_config_t {
    let Some(kernel) = cstr_to_pathbuf(kernel) else {
        return ptr::null_mut();
    };
    let Some(initramfs) = cstr_to_pathbuf(initramfs) else {
        return ptr::null_mut();
    };
    let cfg = Box::new(Config::new(kernel, initramfs));
    Box::into_raw(cfg) as *mut vmette_config_t
}

/// Free a config. No-op on NULL.
///
/// # Safety
/// See the module-level safety contract. After this call `cfg` is dangling
/// and must not be reused.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_free(cfg: *mut vmette_config_t) {
    if cfg.is_null() {
        return;
    }
    drop(Box::from_raw(cfg as *mut Config));
}

// ---- setters -----------------------------------------------------------

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_cmdline(
    cfg: *mut vmette_config_t,
    cmdline: *const c_char,
) {
    let Some(c) = cfg_mut(cfg) else { return };
    if let Some(s) = cstr_to_string(cmdline) {
        c.cmdline = s;
    }
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_rootfs_share(
    cfg: *mut vmette_config_t,
    path: *const c_char,
    read_only: bool,
) {
    let Some(c) = cfg_mut(cfg) else { return };
    if let Some(p) = cstr_to_pathbuf(path) {
        c.rootfs = Some(crate::Rootfs::Share(RootfsShare { path: p, read_only }));
    }
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_add_share(
    cfg: *mut vmette_config_t,
    tag: *const c_char,
    path: *const c_char,
) {
    let Some(c) = cfg_mut(cfg) else { return };
    let Some(tag) = cstr_to_string(tag) else {
        return;
    };
    let Some(path) = cstr_to_pathbuf(path) else {
        return;
    };
    c.shares.push(ShareMount { tag, path });
}

/// Append a `KEY=value` environment variable applied in the guest before the
/// exec command (overrides any OCI image env). Ignored on null/invalid-UTF-8
/// args.
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_add_env(
    cfg: *mut vmette_config_t,
    key: *const c_char,
    value: *const c_char,
) {
    let Some(c) = cfg_mut(cfg) else { return };
    let Some(key) = cstr_to_string(key) else {
        return;
    };
    let Some(value) = cstr_to_string(value) else {
        return;
    };
    c.env.push((key, value));
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_add_disk(cfg: *mut vmette_config_t, path: *const c_char) {
    let Some(c) = cfg_mut(cfg) else { return };
    if let Some(p) = cstr_to_pathbuf(path) {
        c.disks.push(p);
    }
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_exec(cfg: *mut vmette_config_t, cmd: *const c_char) {
    let Some(c) = cfg_mut(cfg) else { return };
    c.exec_cmd = cstr_to_string(cmd);
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_net(cfg: *mut vmette_config_t, enable: bool) {
    if let Some(c) = cfg_mut(cfg) {
        c.net = enable;
    }
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_switch_root(cfg: *mut vmette_config_t, enable: bool) {
    if let Some(c) = cfg_mut(cfg) {
        c.switch_root = enable;
    }
}

/// Set the vsock port policy.
/// `port < 0`  → disable the vsock device entirely.
/// `port == 0` → auto-allocate per invocation (50000..60000).
/// `port > 0`  → use that exact port.
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_vsock_port(cfg: *mut vmette_config_t, port: i32) {
    let Some(c) = cfg_mut(cfg) else { return };
    c.vsock_port = match port {
        n if n < 0 => VsockPort::Disabled,
        0 => VsockPort::Auto,
        n => VsockPort::Fixed(n as u32),
    };
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_guest_vsock_port(cfg: *mut vmette_config_t, port: u32) {
    if let Some(c) = cfg_mut(cfg) {
        c.guest_vsock_port = port;
    }
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_timeout(cfg: *mut vmette_config_t, seconds: u32) {
    if let Some(c) = cfg_mut(cfg) {
        c.timeout_seconds = if seconds == 0 { None } else { Some(seconds) };
    }
}

/// Note: no clamping. A value VZ rejects (e.g. 0) surfaces as
/// `InvalidConfig` from `vmette_run` — same path as the Rust API.
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_vcpus(cfg: *mut vmette_config_t, n: u8) {
    if let Some(c) = cfg_mut(cfg) {
        c.vcpus = n;
    }
}

/// Note: no clamping. See `vmette_config_set_vcpus` for the rationale.
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_mem_mib(cfg: *mut vmette_config_t, n: u64) {
    if let Some(c) = cfg_mut(cfg) {
        c.mem_mib = n;
    }
}

/// Set the ephemeral ext4 scratch disk size in MiB, used as the guest's
/// writable overlay upper so the writable root (and `/tmp`) is bounded by the
/// disk rather than `mem_mib`. Pass `0` to disable (the default — a RAM-backed
/// tmpfs overlay); any non-zero value enables a per-run scratch disk of that
/// size that is created sparse and discarded on teardown. No effect with a
/// read-only directory rootfs (no writable overlay).
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_scratch_mib(cfg: *mut vmette_config_t, mib: u64) {
    if let Some(c) = cfg_mut(cfg) {
        c.scratch_mib = (mib != 0).then_some(mib);
    }
}

/// Path to a snapshot file to write after the guest signals ready.
/// Not yet implemented: returns VmetteStatus::SnapshotUnsupported on every
/// architecture (including Apple Silicon).
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_build_snapshot(
    cfg: *mut vmette_config_t,
    path: *const c_char,
) {
    let Some(c) = cfg_mut(cfg) else { return };
    c.build_snapshot = cstr_to_pathbuf(path);
}

/// Path to a previously-saved snapshot to restore. Not yet implemented:
/// returns VmetteStatus::SnapshotUnsupported on every architecture.
///
/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_config_set_resume_snapshot(
    cfg: *mut vmette_config_t,
    path: *const c_char,
) {
    let Some(c) = cfg_mut(cfg) else { return };
    c.resume_snapshot = cstr_to_pathbuf(path);
}

// ---- run + output ------------------------------------------------------

/// Run a configured guest. Blocks until the guest powers off.
///
/// On success returns [`VmetteStatus::Ok`] and writes a newly-allocated
/// run output handle to `*out` (caller must `vmette_run_output_free`); read the
/// guest's exit code from it with `vmette_run_output_exit_code`. On error
/// returns the matching status and leaves `*out` untouched. The call returns
/// normally — it does not exit the host process.
///
/// # Safety
/// See the module-level safety contract. `out` must be a valid, writable
/// pointer to a `*mut vmette_run_output_t`.
#[no_mangle]
pub unsafe extern "C" fn vmette_run(
    cfg: *const vmette_config_t,
    out: *mut *mut vmette_run_output_t,
) -> VmetteStatus {
    let Some(c) = cfg_ref(cfg) else {
        return VmetteStatus::NullArg;
    };
    if out.is_null() {
        return VmetteStatus::NullArg;
    }
    match crate::run(c) {
        Ok(r) => {
            let boxed = Box::new(r);
            *out = Box::into_raw(boxed) as *mut vmette_run_output_t;
            VmetteStatus::Ok
        }
        Err(e) => VmetteStatus::from(&e),
    }
}

/// # Safety
/// See the module-level safety contract.
#[no_mangle]
pub unsafe extern "C" fn vmette_run_output_exit_code(out: *const vmette_run_output_t) -> i32 {
    if out.is_null() {
        return 0;
    }
    let r = &*(out as *const RunOutput);
    r.exit_code
}

/// # Safety
/// See the module-level safety contract. After this call `out` is dangling
/// and must not be reused.
#[no_mangle]
pub unsafe extern "C" fn vmette_run_output_free(out: *mut vmette_run_output_t) {
    if out.is_null() {
        return;
    }
    drop(Box::from_raw(out as *mut RunOutput));
}

// ---- misc --------------------------------------------------------------

/// Returns the library's semver string (e.g. "0.1.0"). Caller must not free.
#[no_mangle]
pub extern "C" fn vmette_version() -> *const c_char {
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr() as *const c_char
}

//! Top-level [`run`] orchestration: assemble the cmdline, build the VZ
//! config, install the delegate + optional vsock listener + optional
//! timeout, start the VM, and pump the main run loop. The delegate's
//! `guestDidStop` callback calls `std::process::exit` with the
//! propagated exit code, which is why this function's return type is a
//! never-typed-in-the-happy-path `Result<RunOutput, Error>`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::AllocAnyThread;
use objc2_foundation::{NSError, NSRunLoop};
use objc2_virtualization::{
    VZVirtioSocketDevice, VZVirtioSocketListener, VZVirtualMachine,
    VZVirtualMachineDelegate,
};

use crate::error::Error;
use crate::terminal::{enter_raw_mode, install_signal_handlers, restore_terminal};
use crate::vz::config::{build as build_vz_config, resolve_vsock_port};
use crate::vz::delegate::{DelegateState, VmetteDelegate};
use crate::vz::vsock::{ListenerState, VsockLogger};
use crate::{cmdline, Config};

/// Send-wrapper for an objc2 `Retained`. We only ever cross thread
/// boundaries by dispatching to the main queue, where the wrapped
/// object was originally constructed — so the unsoundness window is
/// closed in practice. Used to satisfy `DispatchQueue::after`'s
/// `F: Send` bound when capturing a VM handle for a timeout closure.
struct MainQueueOnly<T>(Retained<T>);
unsafe impl<T> Send for MainQueueOnly<T> {}
unsafe impl<T> Sync for MainQueueOnly<T> {}
impl<T> std::ops::Deref for MainQueueOnly<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Result of a completed [`run`]. Currently only carries the exit code,
/// but kept as a struct so we can grow it without breaking callers.
#[derive(Debug, Clone, Copy)]
pub struct RunOutput {
    pub exit_code: i32,
}

/// Boot the configured guest, exec the command, block until poweroff,
/// then exit the process with the guest's exit code.
///
/// Note: on success the function does not return — it calls
/// `std::process::exit` from the VM's lifecycle delegate. The
/// `Result<RunOutput, Error>` shape is for error paths (config invalid,
/// VM failed to start, snapshot unsupported, etc).
pub fn run(config: &Config) -> Result<RunOutput, Error> {
    // Snapshot dispatch — both build and resume go through here.
    if let Some(p) = &config.build_snapshot {
        crate::vz::snapshot::build(config, p)?;
        return Ok(RunOutput { exit_code: 0 });
    }
    if let Some(p) = &config.resume_snapshot {
        let code = crate::vz::snapshot::resume(config, p)?;
        return Ok(RunOutput { exit_code: code });
    }

    let vsock_port = resolve_vsock_port(config.vsock_port);
    let cmdline = cmdline::build(config, vsock_port);

    // Pre-launch: unlink any stale .vmette-exit so we don't read a previous run.
    let exit_code_file = config.rootfs_share.as_ref().and_then(|rs| {
        if rs.read_only {
            None
        } else {
            let p = rs.path.join(".vmette-exit");
            let _ = std::fs::remove_file(&p);
            Some(p)
        }
    });

    eprint_banner(config, &cmdline, vsock_port);

    let cfg = build_vz_config(config, &cmdline, vsock_port)?;

    install_signal_handlers();
    enter_raw_mode();

    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &cfg)
    };

    // Lifecycle delegate
    let timed_out = Arc::new(AtomicBool::new(false));
    let delegate = VmetteDelegate::new(DelegateState {
        exit_code_file,
        timed_out: timed_out.clone(),
    });
    unsafe {
        let proto: &ProtocolObject<dyn VZVirtualMachineDelegate> =
            ProtocolObject::from_ref(&*delegate);
        vm.setDelegate(Some(proto));
    }

    // Vsock listener. Both `logger` and `listener` must outlive the run
    // loop because VZ holds the delegate weakly — if either is dropped,
    // the listener silently stops accepting connections. We bind them at
    // function scope so they live until run() exits (which it never does
    // in the happy path).
    let _vsock_keepalive: Option<(Retained<VsockLogger>, Retained<VZVirtioSocketListener>)>;
    if let Some(port) = vsock_port {
        let sock_dev = unsafe { vm.socketDevices() };
        if let Some(dev) = sock_dev.firstObject() {
            let dev: Retained<VZVirtioSocketDevice> =
                unsafe { Retained::cast_unchecked(dev) };
            let logger = VsockLogger::new(ListenerState {
                port,
                ready_handler: Arc::new(Mutex::new(None)),
            });
            let listener = unsafe { VZVirtioSocketListener::new() };
            unsafe {
                listener.setDelegate(Some(ProtocolObject::from_ref(&*logger)));
                dev.setSocketListener_forPort(&listener, port);
            }
            _vsock_keepalive = Some((logger, listener));
        } else {
            _vsock_keepalive = None;
        }
    } else {
        _vsock_keepalive = None;
    }

    // Timeout
    if let Some(secs) = config.timeout_seconds {
        let vm_for_timer = MainQueueOnly(vm.clone());
        let timed_out_setter = timed_out.clone();
        let when = DispatchTime::try_from(Duration::from_secs(secs as u64))
            .unwrap_or(DispatchTime::NOW);
        let _ = DispatchQueue::main().after(when, move || {
            eprintln!("\r\n[vmette] timeout {}s reached, force-stopping\r", secs);
            timed_out_setter.store(true, Ordering::SeqCst);
            let stop_cb = RcBlock::new(|_err: *mut NSError| {
                restore_terminal();
                std::process::exit(124);
            });
            unsafe { vm_for_timer.stopWithCompletionHandler(&stop_cb) };
        });
    }

    // Start
    let start_cb = RcBlock::new(move |err: *mut NSError| {
        if !err.is_null() {
            let err = unsafe { &*err };
            restore_terminal();
            eprintln!("[vmette] vm.start failed: {}", err.localizedDescription());
            std::process::exit(1);
        }
    });
    unsafe { vm.startWithCompletionHandler(&start_cb) };

    // Run the main loop forever; delegate's exit() takes us out.
    NSRunLoop::mainRunLoop().run();
    unreachable!()
}

fn eprint_banner(config: &Config, cmdline: &str, vsock_port: Option<u32>) {
    let rootfs = config
        .rootfs_share
        .as_ref()
        .map(|r| format!("{}{}", r.path.display(), if r.read_only { " (ro)" } else { "" }))
        .unwrap_or_else(|| "(none)".into());
    let vsock = match vsock_port {
        None => "(disabled)".into(),
        Some(p) => p.to_string(),
    };
    eprintln!(
        "[vmette] kernel       {}\n\
         [vmette] initramfs    {}\n\
         [vmette] cmdline      {}\n\
         [vmette] rootfs       {}\n\
         [vmette] shares       {}\n\
         [vmette] disks        {}\n\
         [vmette] exec         {}\n\
         [vmette] vsock-port   {}\n\
         [vmette] switch-root  {}\n\
         [vmette] net          {}\n\
         [vmette] timeout      {}s\n\
         [vmette] vcpus        {}, memMiB {}\n",
        config.kernel.display(),
        config.initramfs.display(),
        cmdline,
        rootfs,
        config.shares.len(),
        config.disks.len(),
        config.exec_cmd.as_deref().unwrap_or("(none — interactive)"),
        vsock,
        if config.switch_root { "yes" } else { "no" },
        if config.net { "yes (NAT)" } else { "no" },
        config.timeout_seconds.unwrap_or(0),
        config.vcpus,
        config.mem_mib,
    );
}

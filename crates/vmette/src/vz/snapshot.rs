//! Snapshot/restore. VZ gates `saveMachineStateToURL:` and
//! `restoreMachineStateFromURL:` behind `#if defined(__arm64__)` in the
//! SDK headers, so on Intel hosts these symbols don't exist. We mirror
//! that with a `cfg(target_arch = "aarch64")` split.
//!
//! On non-aarch64 builds, callers that hit the snapshot path receive
//! [`crate::Error::SnapshotUnsupported`].

#[cfg(target_arch = "aarch64")]
pub(crate) use enabled::*;

#[cfg(not(target_arch = "aarch64"))]
pub(crate) use disabled::*;

#[cfg(target_arch = "aarch64")]
mod enabled {
    // Real implementations will live here. Implemented in Phase 5 (daemon
    // snapshot pool needs them); for now the symbols exist so the rest
    // of the crate compiles on aarch64.
    use crate::error::Error;
    use crate::Config;

    pub(crate) fn build(_config: &Config, _path: &std::path::Path) -> Result<(), Error> {
        // TODO(phase 5): port the ObjC snapshot-build flow to objc2.
        Err(Error::SnapshotUnsupported)
    }

    pub(crate) fn resume(_config: &Config, _path: &std::path::Path) -> Result<i32, Error> {
        // TODO(phase 5): port the ObjC snapshot-resume flow.
        Err(Error::SnapshotUnsupported)
    }
}

#[cfg(not(target_arch = "aarch64"))]
mod disabled {
    use crate::error::Error;
    use crate::Config;

    pub(crate) fn build(_config: &Config, _path: &std::path::Path) -> Result<(), Error> {
        Err(Error::SnapshotUnsupported)
    }

    pub(crate) fn resume(_config: &Config, _path: &std::path::Path) -> Result<i32, Error> {
        Err(Error::SnapshotUnsupported)
    }
}

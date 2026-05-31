//! Library face of `vmette-daemon`.
//!
//! The daemon ships as the `vmetted` binary ([`main.rs`](../main.rs)); this lib
//! target exposes the pieces of it that are pure and worth exercising on their
//! own — currently the [`settle`] perception module. Keeping `settle` here (a
//! library module the binary consumes via `vmette_daemon::settle`) lets it be
//! unit-tested and benchmarked in isolation, without standing up a VM, while it
//! still lives in the daemon crate — its only consumer.
pub mod settle;

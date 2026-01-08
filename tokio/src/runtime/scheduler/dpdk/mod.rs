//! DPDK scheduler for low-latency networking.
//!
//! This scheduler is optimized for DPDK-based networking with the following characteristics:
//! - Per-core workers with CPU affinity (no work stealing)
//! - Busy-poll event loop (workers never park)
//! - Dedicated IO thread for standard I/O and timers
//! - smoltcp-based TCP/IP stack over DPDK

mod config;
#[cfg(feature = "dpdk")]
mod device;
#[cfg(feature = "dpdk")]
mod dpdk_driver;
#[cfg(feature = "dpdk")]
mod ffi;
#[cfg(feature = "dpdk")]
mod init;
mod io_thread;
#[cfg(feature = "dpdk")]
mod resolve;

// Internal modules accessible within dpdk
pub(super) mod counters;
pub(super) mod queue;
pub(super) mod stats;

// Public modules
pub(crate) mod handle;
pub(crate) mod worker;

// Re-export what scheduler needs
pub(crate) use handle::Handle;
pub(crate) use worker::Context;

use crate::future::Future;
use crate::loom::sync::Arc;
use crate::runtime::scheduler;

/// DPDK scheduler top-level struct (similar to MultiThread)
#[derive(Debug)]
pub(crate) struct Dpdk {
    /// Scheduler handle
    handle: Arc<Handle>,
}

impl Dpdk {
    /// Returns the scheduler handle
    pub(crate) fn handle(&self) -> &Arc<Handle> {
        &self.handle
    }

    /// Blocks on a future
    pub(crate) fn block_on<F: Future>(&self, _handle: &scheduler::Handle, _future: F) -> F::Output {
        // TODO: Implement proper block_on for DPDK scheduler
        // For now, this is a placeholder - DPDK scheduler uses busy-poll workers
        // and block_on would need different semantics
        unimplemented!("DPDK scheduler block_on not yet implemented - use spawn() instead")
    }

    /// Shuts down the scheduler
    pub(crate) fn shutdown(&mut self, _handle: &scheduler::Handle) {
        self.handle.shutdown();
    }
}

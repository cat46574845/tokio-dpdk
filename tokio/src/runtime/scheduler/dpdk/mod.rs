//! DPDK scheduler for low-latency networking.
//!
//! This scheduler is optimized for DPDK-based networking with the following characteristics:
//! - Per-core workers with CPU affinity (no work stealing)
//! - Busy-poll event loop (workers never park)
//! - Dedicated IO thread for standard I/O and timers
//! - smoltcp-based TCP/IP stack over DPDK

mod config;
mod device;
pub(crate) mod dpdk_driver;
mod ffi;
pub(crate) mod init;
pub(crate) mod io_thread;
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

// Re-export worker context API for TcpDpdkStream
pub(crate) use worker::{current_worker_index, with_current_driver};

// Re-export config for builder
pub(crate) use config::DpdkBuilder;

use crate::future::Future;
use crate::loom::sync::Arc;
use crate::runtime::scheduler;

use std::io;

/// DPDK scheduler top-level struct (similar to MultiThread)
#[derive(Debug)]
pub(crate) struct Dpdk {
    /// Scheduler handle
    handle: Arc<Handle>,
}

impl Dpdk {
    /// Create a new DPDK scheduler with initialized resources.
    ///
    /// This function:
    /// 1. Initializes DPDK EAL
    /// 2. Creates memory pool  
    /// 3. Initializes ports
    /// 4. Creates DpdkDevice for each device
    /// 5. Creates scheduler Handle and worker Launch
    ///
    /// The DPDK resources (devices) will be passed to workers via the Launch struct.
    pub(crate) fn new(
        dpdk_builder: &DpdkBuilder,
        driver_handle: crate::runtime::driver::Handle,
        blocking_spawner: crate::runtime::blocking::Spawner,
        seed_generator: crate::util::RngSeedGenerator,
        config: crate::runtime::Config,
    ) -> io::Result<(Self, worker::Launch, init::DpdkResources)> {
        // Validate DPDK configuration
        dpdk_builder
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

        // Initialize DPDK resources (EAL, mempool, ports, devices)
        let mut resources = init::initialize_dpdk(dpdk_builder)?;

        if resources.devices.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "No DPDK devices initialized",
            ));
        }

        // Extract core IDs from resolved device configurations
        let core_ids: Vec<usize> = resources.devices.iter().map(|d| d.config.core).collect();

        // Create Handle and Launch using worker::create (passes devices for DpdkDriver creation)
        let (handle, launch) = worker::create(
            core_ids,
            std::mem::take(&mut resources.devices), // Move devices to workers
            driver_handle,
            blocking_spawner,
            seed_generator,
            config,
        );

        Ok((Self { handle }, launch, resources))
    }

    /// Returns the scheduler handle
    pub(crate) fn handle(&self) -> &Arc<Handle> {
        &self.handle
    }

    /// Blocks on a future
    ///
    /// For DPDK scheduler, this spawns the future on a worker thread and busy-polls
    /// until completion. Note: DPDK scheduler is optimized for networking workloads
    /// and block_on may have different performance characteristics than multi-thread.
    pub(crate) fn block_on<F: Future>(&self, _handle: &scheduler::Handle, future: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        // For DPDK scheduler, we need to run the future to completion
        // Since workers are busy-polling, we can use a simple waker that does nothing
        // and rely on the scheduler's polling

        // Simple waker that does nothing (workers are already busy-polling)
        fn noop_clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);

        // Pin the future on stack and poll until ready
        let mut future = std::pin::pin!(future);

        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => {
                    // Yield to let worker threads make progress
                    std::hint::spin_loop();
                }
            }
        }
    }

    /// Shuts down the scheduler
    pub(crate) fn shutdown(&mut self, _handle: &scheduler::Handle) {
        self.handle.shutdown();
    }
}

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
pub(crate) mod env_config;
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
pub(crate) struct Dpdk {
    /// Scheduler handle
    handle: Arc<Handle>,
    /// I/O thread handle for shutdown coordination
    io_thread_handle: Option<io_thread::IoThreadHandle>,
    /// DPDK resources (mempool) - must be kept alive for runtime lifetime
    resources: Option<init::DpdkResources>,
}

// Manual Debug implementation since IoThreadHandle is not Debug by default
impl std::fmt::Debug for Dpdk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dpdk")
            .field("handle", &self.handle)
            .field("io_thread_handle", &self.io_thread_handle)
            .finish()
    }
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
    /// The DPDK resources (mempool) will be stored in Self for proper lifecycle management.
    pub(crate) fn new(
        dpdk_builder: &DpdkBuilder,
        driver_handle: crate::runtime::driver::Handle,
        blocking_spawner: crate::runtime::blocking::Spawner,
        seed_generator: crate::util::RngSeedGenerator,
        config: crate::runtime::Config,
    ) -> io::Result<(Self, worker::Launch)> {
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

        Ok((
            Self {
                handle,
                io_thread_handle: None,
                resources: Some(resources),
            },
            launch,
        ))
    }

    /// Sets the I/O thread handle for shutdown coordination.
    /// Called by the builder after spawning the I/O thread.
    pub(crate) fn set_io_thread_handle(&mut self, handle: io_thread::IoThreadHandle) {
        self.io_thread_handle = Some(handle);
    }

    /// Returns the scheduler handle
    pub(crate) fn handle(&self) -> &Arc<Handle> {
        &self.handle
    }

    /// Blocks on a future
    ///
    /// For DPDK scheduler, this enters the runtime context and blocks on the future.
    /// Spawned tasks will be executed on the worker threads.
    pub(crate) fn block_on<F: Future>(&self, handle: &scheduler::Handle, future: F) -> F::Output {
        crate::runtime::context::enter_runtime(handle, true, |blocking| {
            blocking
                .block_on(future)
                .expect("failed to block on DPDK runtime")
        })
    }

    /// Shuts down the scheduler
    pub(crate) fn shutdown(&mut self, _handle: &scheduler::Handle) {
        // First shut down the I/O thread so it stops processing events
        if let Some(mut io_handle) = self.io_thread_handle.take() {
            io_handle.shutdown_and_join();
        }

        // Then shut down the scheduler
        self.handle.shutdown();

        // Finally, clean up DPDK resources (stop devices, free mempool)
        if let Some(ref mut resources) = self.resources {
            resources.cleanup();
        }
    }
}

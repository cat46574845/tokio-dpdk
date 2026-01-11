//! DPDK scheduler for low-latency networking.
//!
//! This scheduler is optimized for DPDK-based networking with the following characteristics:
//! - Per-core workers with CPU affinity (no work stealing)
//! - Busy-poll event loop (workers never park)
//! - Dedicated IO thread for standard I/O and timers
//! - smoltcp-based TCP/IP stack over DPDK

mod allocation;
mod config;
mod device;
pub(crate) mod dpdk_driver;
pub(crate) mod env_config;
mod ffi;
mod flow_rules;
pub(crate) mod init;
pub(crate) mod io_thread;
mod resolve;
mod resource_lock;

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
    resources: Option<DpdkResourcesHolder>,
    /// Resource locks (devices and cores) - released when runtime is dropped
    #[allow(dead_code)]
    resource_lock: resource_lock::ResourceLock,
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

/// DPDK resources holder that can be either single-queue or multi-queue mode.
pub(crate) enum DpdkResourcesHolder {
    /// Single-queue mode resources (legacy path using devices())
    SingleQueue(init::DpdkResources),
    /// Multi-queue mode resources (new path using AllocationPlan)
    MultiQueue(init::DpdkResourcesMultiQueue),
}

impl DpdkResourcesHolder {
    fn cleanup(&mut self) {
        match self {
            DpdkResourcesHolder::SingleQueue(r) => r.cleanup(),
            DpdkResourcesHolder::MultiQueue(r) => r.cleanup(),
        }
    }
}

impl Dpdk {
    /// Create a new DPDK scheduler with initialized resources.
    ///
    /// This function supports two paths:
    /// 1. Legacy path: When `devices()` was called, uses the original initialization
    /// 2. New path: When no devices configured, uses AllocationPlan from env.json
    ///
    /// The DPDK resources (mempool) will be stored in Self for proper lifecycle management.
    pub(crate) fn new(
        dpdk_builder: &DpdkBuilder,
        driver_handle: crate::runtime::driver::Handle,
        blocking_spawner: crate::runtime::blocking::Spawner,
        seed_generator: crate::util::RngSeedGenerator,
        config: crate::runtime::Config,
    ) -> io::Result<(Self, worker::Launch)> {
        // Determine which initialization path to use:
        // - If devices() was called: use legacy path
        // - Otherwise: use AllocationPlan path
        let use_allocation_plan = dpdk_builder.get_devices().is_empty();

        if use_allocation_plan {
            Self::new_with_allocation_plan(
                dpdk_builder,
                driver_handle,
                blocking_spawner,
                seed_generator,
                config,
            )
        } else {
            Self::new_legacy(
                dpdk_builder,
                driver_handle,
                blocking_spawner,
                seed_generator,
                config,
            )
        }
    }

    /// Legacy initialization path using devices() configuration.
    fn new_legacy(
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

        // Acquire resource locks for devices and cores to prevent other processes
        // from using the same resources.
        let mut resource_lock = resource_lock::ResourceLock::new();

        // Lock devices by PCI address
        let pci_addresses: Vec<String> = resources
            .devices
            .iter()
            .filter_map(|d| {
                // Try to get PCI from env_config lookup by PCI or name
                let env_config = env_config::DpdkEnvConfig::load().ok()?;
                env_config
                    .find_by_pci(&d.config.name)
                    .or_else(|| env_config.find_by_name(&d.config.name))
                    .map(|dev| dev.pci_address.clone())
            })
            .collect();

        if !pci_addresses.is_empty() {
            // Best-effort locking - don't fail if lock dir doesn't exist
            let _ = resource_lock.acquire_devices(&pci_addresses);
        }

        // Lock cores
        let core_ids: Vec<usize> = resources.devices.iter().map(|d| d.config.core).collect();
        if !core_ids.is_empty() {
            // Best-effort locking - don't fail if lock dir doesn't exist
            let _ = resource_lock.acquire_cores(&core_ids);
        }

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
                resources: Some(DpdkResourcesHolder::SingleQueue(resources)),
                resource_lock,
            },
            launch,
        ))
    }

    /// New initialization path using AllocationPlan from env.json.
    fn new_with_allocation_plan(
        dpdk_builder: &DpdkBuilder,
        driver_handle: crate::runtime::driver::Handle,
        blocking_spawner: crate::runtime::blocking::Spawner,
        seed_generator: crate::util::RngSeedGenerator,
        config: crate::runtime::Config,
    ) -> io::Result<(Self, worker::Launch)> {
        // Load environment configuration
        let env_config = env_config::DpdkEnvConfig::load()?;

        // Create allocation plan based on available resources
        let requested_devices: Option<&[String]> = if dpdk_builder.get_pci_addresses().is_empty() {
            None
        } else {
            Some(dpdk_builder.get_pci_addresses())
        };
        let requested_num_workers = dpdk_builder.get_num_workers();

        let plan = allocation::create_allocation_plan(
            &env_config,
            requested_devices,
            requested_num_workers,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

        if plan.workers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AllocationPlan has no workers",
            ));
        }

        // Acquire resource locks BEFORE initialization
        let mut resource_lock = resource_lock::ResourceLock::new();

        // Lock devices
        let pci_addresses: Vec<String> =
            plan.workers.iter().map(|w| w.pci_address.clone()).collect();
        let _ = resource_lock.acquire_devices(&pci_addresses);

        // Lock cores
        let core_ids: Vec<usize> = plan.workers.iter().map(|w| w.core_id).collect();
        let _ = resource_lock.acquire_cores(&core_ids);

        // Initialize DPDK from allocation plan
        let (resources, initialized_workers) =
            init::initialize_dpdk_from_plan(&plan, dpdk_builder.get_eal_args())?;

        // Create flow rules for multi-queue traffic routing
        // This is best-effort - if flow rules fail (e.g., AWS ENA limited support),
        // traffic will still work but may not be correctly routed
        let mut _flow_rules = Vec::new();
        for (pci_address, _num_queues) in allocation::get_device_queue_counts(&plan) {
            // Find port_id for this PCI address
            if let Some(worker) = plan.workers.iter().find(|w| w.pci_address == pci_address) {
                if let Some(port) = resources.ports.iter().find(|&&p| {
                    // Match by looking up in initialized_workers
                    initialized_workers
                        .iter()
                        .any(|iw| iw.port_id == p && iw.mac == worker.mac)
                }) {
                    // Collect IP -> queue_id mappings for this port
                    let allocations: Vec<(smoltcp::wire::IpCidr, u16)> = plan
                        .workers
                        .iter()
                        .filter(|w| w.pci_address == pci_address)
                        .flat_map(|w| {
                            let mut ips = Vec::new();
                            if let Some(ipv4) = w.ipv4 {
                                ips.push((ipv4, w.queue_id));
                            }
                            if let Some(ipv6) = w.ipv6 {
                                ips.push((ipv6, w.queue_id));
                            }
                            ips
                        })
                        .collect();

                    if !allocations.is_empty() {
                        // Create flow rules - this is best-effort
                        match flow_rules::create_flow_rules(*port, &allocations) {
                            Ok(rules) => _flow_rules.extend(rules),
                            Err(e) => {
                                eprintln!(
                                    "[DPDK] Warning: Failed to create flow rules for port {}: {}",
                                    port, e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Create Handle and Launch using the new worker::create_from_workers
        let (handle, launch) = worker::create_from_workers(
            initialized_workers,
            driver_handle,
            blocking_spawner,
            seed_generator,
            config,
        );

        Ok((
            Self {
                handle,
                io_thread_handle: None,
                resources: Some(DpdkResourcesHolder::MultiQueue(resources)),
                resource_lock,
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

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
use crate::runtime::builder::ThreadNameFn;
use crate::runtime::Callback;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// DPDK scheduler top-level struct (similar to MultiThread)
///
/// The DPDK resource cleanup (mempool, EAL) is handled by `Shared::dpdk_resources`,
/// which is dropped when the last `Arc<Handle>` is released. This ensures proper
/// drop order: DpdkDevices drop first (releasing mbufs), then mempool is freed.
pub(crate) struct Dpdk {
    /// Worker thread handles for joining during shutdown
    worker_handles: Vec<std::thread::JoinHandle<()>>,
    /// Worker 0 - reserved for main thread during block_on
    worker_0: Option<Arc<worker::Worker>>,
    /// I/O thread handle for shutdown coordination
    io_thread_handle: Option<io_thread::IoThreadHandle>,
    /// Resource locks (devices and cores) - released when runtime is dropped.
    #[allow(dead_code)]
    resource_lock: resource_lock::ResourceLock,
    /// Scheduler handle - contains Shared which holds DpdkDrivers/DpdkDevices
    /// and the DpdkResourcesCleaner for mempool/EAL cleanup.
    handle: Arc<Handle>,
    /// Flag to detect concurrent block_on calls (not supported by DPDK scheduler)
    block_on_in_progress: AtomicBool,
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
    /// Uses AllocationPlan from env.json to determine device/core/IP allocation.
    /// The `requested_worker_count` maps to `Builder::worker_threads()`.
    pub(crate) fn new(
        dpdk_builder: &DpdkBuilder,
        driver_handle: crate::runtime::driver::Handle,
        blocking_spawner: crate::runtime::blocking::Spawner,
        seed_generator: crate::util::RngSeedGenerator,
        config: crate::runtime::Config,
        requested_worker_count: Option<usize>,
        thread_name: ThreadNameFn,
        thread_stack_size: Option<usize>,
        after_start: Option<Callback>,
        before_stop: Option<Callback>,
    ) -> io::Result<(Self, worker::Launch)> {
        // Validate configuration
        dpdk_builder
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

        // Load environment configuration
        let env_config = env_config::DpdkEnvConfig::load()?;

        // Validate env config
        env_config.validate()?;

        // Create allocation plan based on available resources
        let requested_devices: Option<&[String]> = if dpdk_builder.get_pci_addresses().is_empty() {
            None
        } else {
            Some(dpdk_builder.get_pci_addresses())
        };

        let plan = allocation::create_allocation_plan(
            &env_config,
            requested_devices,
            requested_worker_count,
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

        // Merge eal_args from env config with builder's extra args.
        // Env config args come first, builder args follow (allowing overrides).
        let mut combined_eal_args = env_config.eal_args.clone();
        combined_eal_args.extend(dpdk_builder.get_eal_args().iter().cloned());

        // Initialize DPDK from allocation plan with configurable parameters
        let (resources, initialized_workers) = init::initialize_dpdk_from_plan(
            &plan,
            &combined_eal_args,
            dpdk_builder.get_mempool_size(),
            dpdk_builder.get_cache_size(),
            dpdk_builder.get_queue_descriptors(),
        )?;

        // Create flow rules for multi-queue traffic routing
        // In multi-queue mode, rte_flow is REQUIRED for correct traffic routing.
        // If flow rules fail, we PANIC instead of silently falling back to RSS,
        // which would cause traffic to be routed to wrong workers.
        let mut _flow_rules = Vec::new();
        let is_multi_queue = plan.workers.len() > 1;

        // Log allocation plan details for debugging
        eprintln!(
            "[DPDK] AllocationPlan: {} workers, is_multi_queue={}",
            plan.workers.len(), is_multi_queue
        );
        let queue_counts = allocation::get_device_queue_counts(&plan);
        for (pci, num_q) in &queue_counts {
            eprintln!("[DPDK]   Device {}: {} queues", pci, num_q);
        }

        for (pci_address, num_queues) in queue_counts {
            // Skip flow rule creation for single-queue devices
            if num_queues <= 1 {
                eprintln!(
                    "[DPDK] Skipping flow rules for device {} (only {} queue)",
                    pci_address, num_queues
                );
                continue;
            }

            // Find port_id for this PCI address
            if let Some(worker) = plan.workers.iter().find(|w| w.pci_address == pci_address) {
                if let Some(port) = resources.ports.iter().find(|&&p| {
                    // Match by looking up in initialized_workers
                    initialized_workers
                        .iter()
                        .any(|iw| iw.port_id == p && iw.mac == worker.mac)
                }) {
                    // Check if this port supports rte_flow before attempting to create rules
                    if !flow_rules::check_flow_support(*port) {
                        // In multi-queue mode, rte_flow is REQUIRED
                        // Panic instead of silently falling back to incorrect behavior
                        panic!(
                            "[DPDK] FATAL: Port {} (device {}) does not support rte_flow. \
                             Multi-queue mode REQUIRES rte_flow for IP-based queue steering. \
                             Without rte_flow, traffic will NOT be correctly routed to workers. \
                             Options: \
                             (1) Use single worker per NIC (worker_threads(1)), \
                             (2) Use multiple NICs with one worker each, \
                             (3) Use hardware that supports rte_flow (not AWS ENA).",
                            port, pci_address
                        );
                    }

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
                        // Create flow rules - REQUIRED for multi-queue
                        match flow_rules::create_flow_rules(*port, &allocations) {
                            Ok(rules) => {
                                eprintln!(
                                    "[DPDK] Created {} flow rules for port {} (device {})",
                                    rules.len(), port, pci_address
                                );
                                _flow_rules.extend(rules);
                            }
                            Err(e) => {
                                // Flow rule creation failed - this is FATAL in multi-queue mode
                                panic!(
                                    "[DPDK] FATAL: Failed to create flow rules for port {} (device {}): {}. \
                                     Multi-queue mode REQUIRES flow rules for correct operation. \
                                     Without flow rules, traffic will NOT be correctly routed to workers.",
                                    port, pci_address, e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Log summary
        if is_multi_queue && !_flow_rules.is_empty() {
            eprintln!(
                "[DPDK] Multi-queue mode initialized with {} flow rules across {} workers",
                _flow_rules.len(), plan.workers.len()
            );
        }

        // Create Handle and Launch using worker::create
        let (handle, mut launch) = worker::create(
            initialized_workers,
            resources.mempool,
            resources.ports.clone(),
            driver_handle,
            blocking_spawner,
            seed_generator,
            config,
            thread_name,
            thread_stack_size,
            after_start,
            before_stop,
        );

        // Take worker 0 for main thread to use during block_on
        let worker_0 = launch.take_worker_0();

        Ok((
            Self {
                worker_handles: Vec::new(),
                worker_0,
                io_thread_handle: None,
                resource_lock,
                handle,
                block_on_in_progress: AtomicBool::new(false),
            },
            launch,
        ))
    }

    /// Sets the I/O thread handle for shutdown coordination.
    /// Called by the builder after spawning the I/O thread.
    pub(crate) fn set_io_thread_handle(&mut self, handle: io_thread::IoThreadHandle) {
        self.io_thread_handle = Some(handle);
    }

    /// Sets the worker thread handles for join during shutdown.
    /// Called by the builder after launching worker threads.
    pub(crate) fn set_worker_handles(&mut self, handles: Vec<std::thread::JoinHandle<()>>) {
        self.worker_handles = handles;
    }

    /// Returns the scheduler handle
    pub(crate) fn handle(&self) -> &Arc<Handle> {
        &self.handle
    }

    /// Blocks on a future
    ///
    /// For DPDK scheduler, the main thread becomes DPDK worker 0 during this call.
    /// This allows DPDK network operations (TcpDpdkStream, TcpDpdkListener) to work
    /// directly within block_on, as the main thread has full worker context.
    ///
    /// The main thread will:
    /// 1. Set CPU affinity to worker 0's core
    /// 2. Set up DPDK worker context
    /// 3. Run the event loop while polling the future and DPDK driver
    /// 4. Return when the future completes
    ///
    /// # Example
    ///
    /// ```ignore
    /// rt.block_on(async {
    ///     // Main thread is now worker 0 with full DPDK access
    ///     let stream = TcpDpdkStream::connect("1.2.3.4:80").await?;
    ///     stream.write_all(b"hello").await?;
    ///     Ok(())
    /// });
    /// ```
    pub(crate) fn block_on<F: Future>(&self, handle: &scheduler::Handle, future: F) -> F::Output {
        // Check for concurrent block_on calls - DPDK scheduler only supports single block_on
        if self.block_on_in_progress.swap(true, Ordering::SeqCst) {
            panic!(
                "DPDK scheduler does not support concurrent block_on calls. \
                 Only one thread may call block_on at a time."
            );
        }

        // Ensure we clear the flag when we're done (even on panic)
        struct ClearOnDrop<'a>(&'a AtomicBool);
        impl Drop for ClearOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = ClearOnDrop(&self.block_on_in_progress);

        // Get worker 0 for main thread
        let worker_0 = self
            .worker_0
            .as_ref()
            .expect("DPDK runtime has no worker 0 - this should never happen")
            .clone();

        // Enter runtime context and run as worker 0
        crate::runtime::context::enter_runtime(handle, true, |_blocking| {
            worker::run_with_future(worker_0, handle, future)
        })
    }

    /// Shuts down the scheduler.
    ///
    /// ## Shutdown Flow (per user specification):
    /// 1. Each worker cleans up all tasks on its own core (CPU affinity preserved)
    /// 2. After worker 0 (main thread) finishes cleanup, join all other workers
    /// 3. Main thread cleans up global (inject queue)
    /// 4. Finalize shutdown
    pub(crate) fn shutdown(&mut self, _handle: &scheduler::Handle) {
        // Step 1: Shut down the I/O thread first so it stops processing events
        if let Some(mut io_handle) = self.io_thread_handle.take() {
            io_handle.shutdown_and_join();
        }

        // Step 2: Signal shutdown to all workers
        // This closes inject queue and owned tasks, setting is_shutdown flag
        self.handle.shutdown();

        // Step 3: Worker 0 (main thread) drains its own queues
        // The main thread still has CPU affinity from block_on, so this is correct.
        if let Some(ref worker_0) = self.worker_0 {
            if let Some(mut core) = worker_0.core.take() {
                core.is_shutdown = true;

                // Drain local queues on worker 0's core
                while let Some(task) = core.run_queue.pop() {
                    drop(task);
                }
                while let Some(task) = core.local_overflow.pop_front() {
                    drop(task);
                }
                drop(core.lifo_slot.take());

                // Report this core as shutdown
                self.handle.shutdown_core(core);
            }
        }

        // Step 4: Wait for all other worker threads to complete
        for handle in self.worker_handles.drain(..) {
            let _ = handle.join();
        }

        // Step 5: Clean up global queues
        {
            let synced = self.handle.shared.synced.lock();
            if !self.handle.shared.inject.is_closed(&synced.inject) {
                drop(synced);
                while let Some(task) = self.handle.next_remote_task() {
                    drop(task);
                }
            }
        }

        // NOTE: DPDK resources cleanup (mempool, EAL) is handled by
        // Shared::dpdk_resources, which is dropped when the last Arc<Handle>
        // is released. This ensures proper drop order:
        // 1. DpdkDevices drop first (freeing mbufs)
        // 2. DpdkResourcesCleaner drops (freeing mempool, calling rte_eal_cleanup)
    }
}

// No custom Drop implementation needed - mempool cleanup is handled by
// Shared::dpdk_resources which is dropped when the last Arc<Handle> is released.

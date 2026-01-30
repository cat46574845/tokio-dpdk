//! DPDK scheduler worker implementation.
//!
//! This is adapted from `multi_thread/worker.rs` with the following changes:
//! - No work stealing (workers are independent)
//! - No parking (busy-poll)
//! - No Idle management
//! - Per-core DPDK driver

use crate::loom::sync::atomic::AtomicBool;
use crate::loom::sync::{Arc, Mutex};
use crate::runtime::scheduler::defer::Defer;
use crate::runtime::scheduler::inject;
use crate::runtime::task::{self, OwnedTasks};
use crate::runtime::{blocking, driver, Config, WorkerMetrics};
use crate::util::atomic_cell::AtomicCell;
use crate::util::rand::FastRand;

use std::cell::RefCell;
use std::collections::VecDeque;

use super::counters::Counters;
use super::dpdk_driver::DpdkDriver;
use super::init::DpdkResourcesCleaner;
use super::queue::{self, Local};
use super::stats::Stats;

use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Instant;

// =============================================================================
// DPDK Debug Module (enabled with `dpdk-debug` feature)
// =============================================================================

#[cfg(feature = "dpdk-debug")]
pub mod debug {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::cell::Cell;

    /// Thread-local timing storage for event loop phases
    thread_local! {
        static PHASE_TIMINGS: Cell<PhaseTimings> = const { Cell::new(PhaseTimings::new()) };
        static TIMING_HISTORY: std::cell::RefCell<TimingHistory> = std::cell::RefCell::new(TimingHistory::new());
    }

    #[derive(Clone, Copy, Default)]
    pub struct PhaseTimings {
        pub tick_start_ns: u64,
        pub poll_driver_start_ns: u64,
        pub poll_driver_end_ns: u64,
        pub next_task_start_ns: u64,
        pub next_task_end_ns: u64,
        pub run_task_start_ns: u64,
        pub run_task_end_ns: u64,
        pub tick_end_ns: u64,
    }

    impl PhaseTimings {
        const fn new() -> Self {
            Self {
                tick_start_ns: 0,
                poll_driver_start_ns: 0,
                poll_driver_end_ns: 0,
                next_task_start_ns: 0,
                next_task_end_ns: 0,
                run_task_start_ns: 0,
                run_task_end_ns: 0,
                tick_end_ns: 0,
            }
        }
    }

    pub struct TimingHistory {
        pub samples: Vec<PhaseTimings>,
        pub capacity: usize,
    }

    impl TimingHistory {
        fn new() -> Self {
            Self {
                samples: Vec::with_capacity(100_000),
                capacity: 100_000,
            }
        }
    }

    #[inline(always)]
    pub fn now_ns() -> u64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    pub fn record_tick_start() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.tick_start_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_poll_driver_start() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.poll_driver_start_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_poll_driver_end() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.poll_driver_end_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_next_task_start() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.next_task_start_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_next_task_end() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.next_task_end_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_run_task_start() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.run_task_start_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_run_task_end() {
        PHASE_TIMINGS.with(|t| {
            let mut timings = t.get();
            timings.run_task_end_ns = now_ns();
            t.set(timings);
        });
    }

    pub fn record_tick_end() {
        PHASE_TIMINGS.with(|t| {
            let timings = t.get();
            TIMING_HISTORY.with(|h| {
                let mut history = h.borrow_mut();
                if history.samples.len() < history.capacity {
                    history.samples.push(timings);
                }
            });
        });
    }

    /// Get timing statistics for the event loop phases
    pub fn get_timing_stats() -> Option<TimingStats> {
        TIMING_HISTORY.with(|h| {
            let history = h.borrow();
            if history.samples.is_empty() {
                return None;
            }

            let mut poll_driver_durations: Vec<u64> = history.samples.iter()
                .map(|t| t.poll_driver_end_ns.saturating_sub(t.poll_driver_start_ns))
                .collect();
            let mut run_task_durations: Vec<u64> = history.samples.iter()
                .filter(|t| t.run_task_end_ns > 0)
                .map(|t| t.run_task_end_ns.saturating_sub(t.run_task_start_ns))
                .collect();
            let mut tick_durations: Vec<u64> = history.samples.iter()
                .map(|t| t.tick_end_ns.saturating_sub(t.tick_start_ns))
                .collect();

            poll_driver_durations.sort();
            run_task_durations.sort();
            tick_durations.sort();

            Some(TimingStats {
                sample_count: history.samples.len(),
                poll_driver_p50_ns: percentile(&poll_driver_durations, 0.50),
                poll_driver_p99_ns: percentile(&poll_driver_durations, 0.99),
                run_task_p50_ns: percentile(&run_task_durations, 0.50),
                run_task_p99_ns: percentile(&run_task_durations, 0.99),
                tick_p50_ns: percentile(&tick_durations, 0.50),
                tick_p99_ns: percentile(&tick_durations, 0.99),
                run_task_count: run_task_durations.len(),
            })
        })
    }

    fn percentile(sorted: &[u64], p: f64) -> u64 {
        if sorted.is_empty() { return 0; }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    #[derive(Debug, Clone)]
    pub struct TimingStats {
        pub sample_count: usize,
        pub poll_driver_p50_ns: u64,
        pub poll_driver_p99_ns: u64,
        pub run_task_p50_ns: u64,
        pub run_task_p99_ns: u64,
        pub tick_p50_ns: u64,
        pub tick_p99_ns: u64,
        pub run_task_count: usize,
    }

    pub fn clear_timing_history() {
        TIMING_HISTORY.with(|h| {
            h.borrow_mut().samples.clear();
        });
    }
}

#[cfg(feature = "dpdk-debug")]
pub use debug::{get_timing_stats, clear_timing_history, TimingStats};

// =============================================================================
// CPU Affinity Helpers
// =============================================================================

/// Set CPU affinity of the current thread to a specific core.
#[cfg(target_os = "linux")]
fn set_cpu_affinity(core_id: usize) {
    unsafe {
        let mut cpuset = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(core_id, &mut cpuset);
        libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &cpuset,
        );
    }
}

/// Get current CPU affinity of the current thread.
#[cfg(target_os = "linux")]
fn get_cpu_affinity() -> libc::cpu_set_t {
    unsafe {
        let mut cpuset = std::mem::zeroed::<libc::cpu_set_t>();
        libc::pthread_getaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut cpuset,
        );
        cpuset
    }
}

/// Restore CPU affinity of the current thread.
#[cfg(target_os = "linux")]
fn restore_cpu_affinity(cpuset: &libc::cpu_set_t) {
    unsafe {
        libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            cpuset,
        );
    }
}

// ============================================================================
// Symmetric State Transfer Types
// ============================================================================

/// Wrapper to make !Send types sendable across thread boundaries.
///
/// # Safety
/// The caller must guarantee that the wrapped value is only accessed
/// from a single thread at a time, and that proper synchronization
/// (e.g., channels) is used when transferring ownership.
#[allow(dead_code)] // Reserved for Symmetric State Transfer protocol (future use)
pub(crate) struct SendWrapper<T>(T);

// SAFETY: We manually ensure single-threaded access via channel synchronization.
// The Core is transferred from worker 0 thread to main thread, and only one
// thread accesses it at any time.
unsafe impl<T> Send for SendWrapper<T> {}

impl<T> SendWrapper<T> {
    /// Create a new SendWrapper.
    ///
    /// # Safety
    /// Caller must ensure that the wrapped value will only be accessed
    /// from one thread at a time with proper synchronization.
    #[allow(dead_code)] // Reserved for Symmetric State Transfer protocol
    pub(crate) unsafe fn new(val: T) -> Self {
        SendWrapper(val)
    }

    /// Consume the wrapper and return the inner value.
    #[allow(dead_code)] // Reserved for Symmetric State Transfer protocol
    pub(crate) fn into_inner(self) -> T {
        self.0
    }
}

/// Symmetric State Transfer protocol for transferring Core between worker 0 and main thread.
///
/// The protocol uses Barriers for bidirectional synchronization:
/// 1. Main thread sets transfer_requested + shutdown
/// 2. Worker 0 sees shutdown, checks transfer_requested, puts Core in transfer_core
/// 3. Both arrive at pause_barrier.wait() - synchronized handoff
/// 4. Main thread takes Core, runs block_on
/// 5. Main thread puts Core back, both arrive at resume_barrier.wait()
/// 6. Worker 0 resumes (or exits if runtime is shutting down)

/// Type-erased state for polling a block_on future.
///
/// This allows Context::run() to poll the block_on future without knowing
/// its concrete type. The poll_fn closure captures the future and result cell.
pub(crate) struct BlockOnState {
    /// Type-erased poll function. Returns Poll<()> where Ready means future completed.
    /// The actual result is stored in a captured OnceCell.
    pub poll_fn: Box<dyn FnMut(&mut std::task::Context<'_>) -> Poll<()>>,
    /// Whether the future has completed.
    pub completed: bool,
}

/// A scheduler worker
pub(crate) struct Worker {
    /// Reference to scheduler's handle
    pub(super) handle: Arc<Handle>,

    /// Index holding this worker's remote state
    pub(super) index: usize,

    /// CPU core ID this worker is bound to
    pub(super) core_id: usize,

    /// Used to hand-off a worker's core to another thread.
    pub(super) core: AtomicCell<Core>,
}

/// Core data - each worker owns one
pub(crate) struct Core {
    /// Used to schedule bookkeeping tasks every so often.
    /// Incremented on every event loop iteration (including idle).
    pub(crate) tick: u32,

    /// Counter for actual task executions.
    /// Only incremented when a task is actually polled/executed.
    /// Useful for distinguishing real work from idle polling.
    pub(crate) exec_count: u64,

    /// LIFO optimization slot - last scheduled task runs first.
    pub(crate) lifo_slot: Option<Notified>,

    /// When `true`, locally scheduled tasks go to the LIFO slot.
    pub(crate) lifo_enabled: bool,

    /// The worker-local run queue.
    pub(crate) run_queue: Local<Arc<Handle>>,

    /// True if the scheduler is being shutdown.
    pub(crate) is_shutdown: bool,

    /// Per-worker runtime stats.
    pub(crate) stats: Stats,

    /// How often to check the global queue.
    pub(crate) global_queue_interval: u32,

    /// Fast random number generator.
    pub(crate) rand: FastRand,

    /// Local overflow queue for tasks when run_queue is full.
    /// Lock-free since only the owning worker accesses it.
    pub(crate) local_overflow: VecDeque<Notified>,
}

/// State shared across all workers
///
/// **IMPORTANT**: Field order determines Drop order. Fields are dropped in declaration order.
/// `dpdk_resources` MUST be after `drivers` so that:
/// 1. DpdkDevice instances drop first (freeing mbufs)
/// 2. Then DpdkResourcesCleaner drops (freeing mempool and calling rte_eal_cleanup)
pub(crate) struct Shared {
    /// Per-worker remote state.
    pub(super) remotes: Box<[Remote]>,

    /// Global task queue.
    pub(super) inject: inject::Shared<Arc<Handle>>,

    /// Collection of all active tasks spawned onto this executor.
    pub(crate) owned: OwnedTasks<Arc<Handle>>,

    /// Data synchronized by the scheduler mutex.
    pub(super) synced: Mutex<Synced>,

    /// Cores that have observed the shutdown signal.
    #[allow(clippy::vec_box)]
    pub(super) shutdown_cores: Mutex<Vec<Box<Core>>>,

    /// Scheduler configuration options.
    pub(super) config: Config,

    /// Per-worker metrics.
    pub(super) worker_metrics: Box<[WorkerMetrics]>,

    /// DPDK drivers (one per worker/core) for network I/O polling.
    /// MUST be before dpdk_resources so devices drop before mempool cleanup!
    pub(super) drivers: Box<[std::sync::Mutex<DpdkDriver>]>,

    /// DPDK resource cleanup (mempool, EAL). MUST BE AFTER drivers!
    /// When dropped, this frees the mempool and calls rte_eal_cleanup().
    /// This ensures all DpdkDevices have released their mbufs first.
    /// Note: This field is not read, but its Drop impl performs cleanup.
    #[allow(dead_code)]
    pub(super) dpdk_resources: Option<DpdkResourcesCleaner>,

    /// Internal counters (only active with cfg flag).
    pub(super) _counters: Counters,

    /// Core storage for transfer between worker 0 and main thread.
    /// Worker 0 puts Core here when pausing, main thread takes it.
    pub(super) transfer_core: crate::util::atomic_cell::AtomicCell<Core>,

    /// Indicates whether the blocked on thread was woken.
    /// Used by the Wake trait implementation to signal that the block_on future
    /// should be polled again.
    pub(super) woken: AtomicBool,
}

/// Data synchronized by the scheduler mutex
pub(crate) struct Synced {
    /// Synchronized state for `Inject`.
    pub(crate) inject: inject::Synced,
}

/// Used to communicate with a worker from other threads.
/// Unlike multi_thread, no Steal handle since DPDK doesn't use work stealing.
pub(super) struct Remote {
    /// CPU core ID for this worker.
    /// Reserved for future NUMA-aware scheduling and diagnostics.
    #[allow(dead_code)]
    pub(super) core_id: usize,

    /// Shutdown signal for this worker.
    pub(super) shutdown: AtomicBool,

    /// Transfer request flag (per-worker).
    /// When true with shutdown, worker should pause and transfer Core.
    pub(super) transfer_requested: AtomicBool,

    /// Barrier for pause synchronization.
    /// Worker and main thread both wait here when worker is pausing.
    pub(super) pause_barrier: std::sync::Barrier,

    /// Barrier for resume synchronization.
    /// Worker and main thread both wait here when worker is resuming.
    pub(super) resume_barrier: std::sync::Barrier,

    /// Per-worker inject queue for spawn_on (targeted task spawning).
    /// Tasks pushed here will only be consumed by this specific worker.
    pub(super) per_inject: inject::Inject<Arc<Handle>>,

    /// Queue of factory closures for spawn_local_on (!Send task spawning).
    /// Each closure creates a !Send future and spawns it locally on this worker.
    pub(super) local_spawn_queue: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send + 'static>>>,
}

/// Thread-local context
pub(crate) struct Context {
    /// Worker.
    pub(crate) worker: Arc<Worker>,

    /// Core data.
    pub(crate) core: RefCell<Option<Box<Core>>>,

    /// Tasks to wake after polling. Used for yielded tasks.
    pub(crate) defer: Defer,

    /// Block-on future state (if running in block_on mode).
    /// This is set when the main thread runs Context::run() during block_on.
    pub(crate) block_on_state: RefCell<Option<BlockOnState>>,
}

/// Starts the workers.
///
/// Contains worker references and thread configuration from the Builder.
pub(crate) struct Launch {
    pub(super) workers: Vec<Arc<Worker>>,
    pub(super) thread_name: crate::runtime::builder::ThreadNameFn,
    pub(super) thread_stack_size: Option<usize>,
    pub(super) after_start: Option<crate::runtime::Callback>,
    pub(super) before_stop: Option<crate::runtime::Callback>,
}

/// Running a task may consume the core.
pub(crate) type RunResult = Result<Box<Core>, ()>;

/// A notified task handle
pub(crate) type Notified = task::Notified<Arc<Handle>>;

/// Value picked out of thin-air for LIFO polls.
const MAX_LIFO_POLLS_PER_TICK: usize = 3;

// Forward declaration - Handle is defined in handle.rs
use super::Handle;
use crate::runtime::{TaskHooks, TimerFlavor};
use crate::util::RngSeedGenerator;

/// Creates DPDK scheduler workers from initialized workers.
///
/// Accepts `InitializedWorker` which contains pre-configured DpdkDevices
/// with specific queue IDs for multi-queue support.
///
/// Also takes thread configuration from the Builder (thread_name, stack_size,
/// after_start, before_stop callbacks).
pub(super) fn create(
    workers: Vec<super::init::InitializedWorker>,
    mempool: *mut super::ffi::rte_mempool,
    ports: Vec<u16>,
    driver_handle: driver::Handle,
    blocking_spawner: blocking::Spawner,
    seed_generator: RngSeedGenerator,
    config: Config,
    thread_name: crate::runtime::builder::ThreadNameFn,
    thread_stack_size: Option<usize>,
    after_start: Option<crate::runtime::Callback>,
    before_stop: Option<crate::runtime::Callback>,
) -> (Arc<Handle>, Launch) {
    let size = workers.len();
    let mut cores = Vec::with_capacity(size);
    let mut remotes = Vec::with_capacity(size);
    let mut worker_metrics = Vec::with_capacity(size);
    let mut drivers = Vec::with_capacity(size);
    let mut core_ids = Vec::with_capacity(size);

    // Create DpdkDrivers from initialized workers
    for worker in workers {
        let driver = DpdkDriver::new(
            worker.device,
            worker.mac,
            worker.addresses,
            worker.gateway_v4,
            worker.gateway_v6,
        );
        drivers.push(std::sync::Mutex::new(driver));
        core_ids.push(worker.core_id);
    }

    // Create the local queues
    for &core_id in &core_ids {
        let run_queue = queue::local();
        let metrics = WorkerMetrics::from_config(&config);
        let stats = Stats::new(&metrics);

        cores.push(Box::new(Core {
            tick: 0,
            exec_count: 0,
            lifo_slot: None,
            // DPDK scheduler: disable LIFO by default for fair multi-connection scheduling
            // LIFO causes unfair task scheduling when multiple wakers are woken in same poll
            lifo_enabled: false,
            run_queue,
            is_shutdown: false,
            global_queue_interval: stats.tuned_global_queue_interval(&config),
            stats,
            rand: FastRand::from_seed(config.seed_generator.next_seed()),
            local_overflow: VecDeque::new(),
        }));

        remotes.push(Remote {
            core_id,
            shutdown: AtomicBool::new(false),
            transfer_requested: AtomicBool::new(false),
            pause_barrier: std::sync::Barrier::new(2),
            resume_barrier: std::sync::Barrier::new(2),
            per_inject: inject::Inject::new(),
            local_spawn_queue: std::sync::Mutex::new(Vec::new()),
        });
        worker_metrics.push(metrics);
    }

    let (inject, inject_synced) = inject::Shared::new();

    let handle = Arc::new(Handle {
        task_hooks: TaskHooks::from_config(&config),
        shared: Shared {
            remotes: remotes.into_boxed_slice(),
            inject,
            owned: OwnedTasks::new(size),
            synced: Mutex::new(Synced {
                inject: inject_synced,
            }),
            shutdown_cores: Mutex::new(vec![]),
            config,
            worker_metrics: worker_metrics.into_boxed_slice(),
            drivers: drivers.into_boxed_slice(),
            dpdk_resources: Some(DpdkResourcesCleaner::new(mempool, ports)),
            _counters: Counters,
            transfer_core: crate::util::atomic_cell::AtomicCell::new(None),
            woken: AtomicBool::new(false),
        },
        driver: driver_handle,
        blocking_spawner,
        seed_generator,
        timer_flavor: TimerFlavor::Traditional,
        is_shutdown: AtomicBool::new(false),
    });

    let mut launch = Launch {
        workers: vec![],
        thread_name,
        thread_stack_size,
        after_start,
        before_stop,
    };

    for (index, (core, &core_id)) in cores.drain(..).zip(core_ids.iter()).enumerate() {
        launch.workers.push(Arc::new(Worker {
            handle: handle.clone(),
            index,
            core_id,
            core: AtomicCell::new(Some(core)),
        }));
    }

    (handle, launch)
}

impl Launch {
    /// Launch all worker threads including worker 0.
    ///
    /// All workers start running immediately after build().
    /// When block_on is called, it signals worker 0 to transfer its Core,
    /// then the main thread takes over worker 0's role.
    ///
    /// Uses Builder's thread configuration (thread_name, stack_size, callbacks).
    ///
    /// Returns the JoinHandles for all spawned worker threads so they can be
    /// joined during shutdown.
    pub(crate) fn launch(&mut self) -> Vec<std::thread::JoinHandle<()>> {
        let mut handles = Vec::with_capacity(self.workers.len());

        // Launch ALL workers including worker 0
        // Worker 0 will transfer its Core to main thread when block_on is called
        for worker in self.workers.iter() {
            let worker_clone = worker.clone();
            let core_id = worker.core_id;
            let after_start = self.after_start.clone();
            let before_stop = self.before_stop.clone();
            let thread_name = (self.thread_name)();

            let mut builder = std::thread::Builder::new().name(thread_name);
            if let Some(stack_size) = self.thread_stack_size {
                builder = builder.stack_size(stack_size);
            }

            let handle = builder
                .spawn(move || {
                    #[cfg(target_os = "linux")]
                    set_cpu_affinity(core_id);

                    if let Some(ref f) = after_start {
                        f();
                    }
                    run(worker_clone);
                    if let Some(ref f) = before_stop {
                        f();
                    }
                })
                .expect("failed to spawn worker thread");

            handles.push(handle);
        }

        handles
    }

    /// Get worker 0 for the main thread to use during block_on.
    pub(crate) fn take_worker_0(&mut self) -> Option<Arc<Worker>> {
        if self.workers.is_empty() {
            None
        } else {
            Some(self.workers[0].clone())
        }
    }
}

/// Worker thread entry point
pub(crate) fn run(worker: Arc<Worker>) {
    // Abort on worker thread panic. Worker thread panics mean the scheduler
    // itself has a bug (task panics are already caught by the task harness).
    // A dead worker would leave the runtime in an inconsistent state and
    // could cause barrier deadlocks if worker 0 dies before a block_on call.
    #[allow(dead_code)]
    struct AbortOnPanic;

    impl Drop for AbortOnPanic {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("DPDK worker thread panicking; aborting process");
                std::process::abort();
            }
        }
    }

    #[cfg(debug_assertions)]
    let _abort_on_panic = AbortOnPanic;

    // Take ownership of the core
    let core = match worker.core.take() {
        Some(core) => core,
        None => return,
    };

    // Build scheduler handle wrapper
    let handle = crate::runtime::scheduler::Handle::Dpdk(worker.handle.clone());

    // Enter the runtime context
    crate::runtime::context::enter_runtime(&handle, false, |_blocking| {
        // Create thread-local context
        let cx = crate::runtime::scheduler::Context::Dpdk(Context {
            worker: worker.clone(),
            core: RefCell::new(None),
            defer: Defer::new(),
            block_on_state: RefCell::new(None),
        });

        crate::runtime::context::set_scheduler(&cx, || {
            // Set scheduler context
            CURRENT.with(|current| {
                // Safety: We control this pointer and it's valid for the duration
                let cx_ref = match &cx {
                    crate::runtime::scheduler::Context::Dpdk(c) => c,
                    _ => unreachable!(),
                };
                current.set(cx_ref as *const Context as *const ());

                // Run the main loop with core
                let _ = cx_ref.run(core);
            });
        });
    });
}

/// Run a worker with a future, returning when the future completes.
///
/// This is used by block_on to make the main thread become a DPDK worker.
///
/// ## Symmetric State Transfer Protocol
///
/// 1. Save main thread's current CPU affinity
/// 2. Set `transfer_requested` to signal worker 0 to transfer its Core
/// 3. Wait on TransferSync condvar for worker 0 to deliver its Core
/// 4. Set main thread's CPU affinity to worker 0's core
/// 5. Create Context with BlockOnState and run unified event loop
/// 6. After completion, put Core back and spawn new worker 0
/// 7. Restore main thread's CPU affinity
/// 8. Return the result
///
/// This ensures spawned tasks are properly executed alongside the main future,
/// and that maintenance (including buffer pool replenishment) runs correctly.
pub(crate) fn run_with_future<F: std::future::Future>(
    worker: Arc<Worker>,
    _handle: &crate::runtime::scheduler::Handle,
    future: F,
) -> F::Output {
    use std::cell::UnsafeCell;
    use std::pin::Pin;

    /// RAII guard ensuring worker 0 is always unblocked from resume_barrier,
    /// even if the block_on future panics during Phase 5.
    ///
    /// On normal completion, Phase 6+7 run explicitly and the guard is defused.
    /// On panic, the guard's Drop recovers the Core, signals the barrier, and
    /// restores CPU affinity, allowing the panic to propagate to the caller.
    struct TransferGuard<'a> {
        worker: &'a Arc<Worker>,
        /// RefCell holding Core inside the scheduler Context (populated during event loop)
        core_cell: &'a RefCell<Option<Box<Core>>>,
        /// RefCell holding Core after event loop returns normally
        returned_core: &'a RefCell<Option<Box<Core>>>,
        #[cfg(target_os = "linux")]
        original_cpuset: &'a libc::cpu_set_t,
        defused: bool,
    }

    impl Drop for TransferGuard<'_> {
        fn drop(&mut self) {
            if self.defused {
                return;
            }

            // Panic path: Phase 6 was skipped due to unwinding.
            // Recover Core from either returned_core or the Context's core cell.
            let core = self
                .returned_core
                .borrow_mut()
                .take()
                .or_else(|| self.core_cell.borrow_mut().take());

            if let Some(mut core) = core {
                core.is_shutdown = false;
                self.worker.handle.shared.transfer_core.set(core);
            }

            // Always signal resume_barrier so worker 0 is unblocked.
            // If Core was not recovered, worker 0 will find transfer_core
            // empty and exit gracefully via shutdown_core().
            self.worker.handle.shared.remotes[0].resume_barrier.wait();

            #[cfg(target_os = "linux")]
            restore_cpu_affinity(self.original_cpuset);
        }
    }

    // ========================================================================
    // Phase 1: Save main thread's CPU affinity
    // ========================================================================
    #[cfg(target_os = "linux")]
    let original_cpuset = get_cpu_affinity();

    // ========================================================================
    // Phase 2: Request worker 0 to pause and transfer its Core
    // Set transfer_requested + shutdown flags, wait for stopped signal
    // ========================================================================
    worker.handle.shared.remotes[0]
        .transfer_requested
        .store(true, Ordering::Release);
    worker.handle.shared.remotes[0]
        .shutdown
        .store(true, Ordering::Release);

    // Wait for worker 0 at the pause barrier (worker puts Core in transfer_core before arriving)
    worker.handle.shared.remotes[0].pause_barrier.wait();

    // ========================================================================
    // Phase 3: Take Core and clear flags
    // ========================================================================
    let core = worker
        .handle
        .shared
        .transfer_core
        .take()
        .expect("worker 0 must put Core in transfer_core");

    // Clear flags - main thread runs as worker 0 and should not see shutdown
    worker.handle.shared.remotes[0]
        .transfer_requested
        .store(false, Ordering::Release);
    worker.handle.shared.remotes[0]
        .shutdown
        .store(false, Ordering::Release);

    // ========================================================================
    // Phase 4: Set main thread's CPU affinity to worker 0's core
    // ========================================================================
    #[cfg(target_os = "linux")]
    set_cpu_affinity(worker.core_id);

    // ========================================================================
    // Phase 5: Create BlockOnState and run unified event loop
    // ========================================================================

    // Pin the future on the stack and store result in UnsafeCell
    let mut future = std::pin::pin!(future);
    let result_cell: UnsafeCell<Option<F::Output>> = UnsafeCell::new(None);

    // Create the type-erased poll function using raw pointers
    let future_ptr = &mut future as *mut Pin<&mut F>;
    let result_ptr = &result_cell as *const UnsafeCell<Option<F::Output>>;

    let poll_fn: Box<dyn FnMut(&mut std::task::Context<'_>) -> Poll<()> + '_> =
        Box::new(move |cx: &mut std::task::Context<'_>| {
            let pinned = unsafe { &mut *future_ptr };
            match pinned.as_mut().poll(cx) {
                Poll::Ready(output) => {
                    unsafe { *(*result_ptr).get() = Some(output) };
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            }
        });

    // Transmute to 'static lifetime for storage
    let poll_fn_static: Box<dyn FnMut(&mut std::task::Context<'_>) -> Poll<()>> =
        unsafe { std::mem::transmute(poll_fn) };

    let block_on_state = BlockOnState {
        poll_fn: poll_fn_static,
        completed: false,
    };

    // Track the returned core for respawn
    let returned_core: std::cell::RefCell<Option<Box<Core>>> = std::cell::RefCell::new(None);

    let _sched_handle = crate::runtime::scheduler::Handle::Dpdk(worker.handle.clone());

    // Create thread-local context with block_on_state
    // Note: enter_runtime is already called by Dpdk::block_on, so we only need set_scheduler
    let cx = crate::runtime::scheduler::Context::Dpdk(Context {
        worker: worker.clone(),
        core: RefCell::new(None),
        defer: Defer::new(),
        block_on_state: RefCell::new(Some(block_on_state)),
    });

    // Extract cx_ref before set_scheduler so TransferGuard can reference cx.core
    let cx_ref = match &cx {
        crate::runtime::scheduler::Context::Dpdk(c) => c,
        _ => unreachable!(),
    };

    // Create guard BEFORE entering the event loop. If Phase 5 panics,
    // the guard's Drop will fire during unwind and handle cleanup.
    let mut transfer_guard = TransferGuard {
        worker: &worker,
        core_cell: &cx_ref.core,
        returned_core: &returned_core,
        #[cfg(target_os = "linux")]
        original_cpuset: &original_cpuset,
        defused: false,
    };

    crate::runtime::context::set_scheduler(&cx, || {
        CURRENT.with(|current| {
            current.set(cx_ref as *const Context as *const ());

            // Run the unified event loop - it will poll block_on_state
            // The run() will return when block_on future completes (is_shutdown set)
            match cx_ref.run(core) {
                Ok(core) => {
                    // Normal completion - save core for respawn
                    *returned_core.borrow_mut() = Some(core);
                }
                Err(()) => {
                    // Shutdown path - try to get core from context
                    if let Some(core) = cx_ref.core.borrow_mut().take() {
                        *returned_core.borrow_mut() = Some(core);
                    }
                }
            }

            current.set(std::ptr::null());
        });
    });

    // ========================================================================
    // Phase 6: Put Core back and resume worker 0
    // ========================================================================
    if let Some(mut core) = returned_core.borrow_mut().take() {
        // Reset shutdown flag for next use
        core.is_shutdown = false;

        // Put Core back into transfer_core for worker 0 to retrieve
        worker.handle.shared.transfer_core.set(core);

        // Signal worker 0 to resume via the resume barrier
        worker.handle.shared.remotes[0].resume_barrier.wait();
    }

    // ========================================================================
    // Phase 7: Restore main thread's CPU affinity
    // ========================================================================
    #[cfg(target_os = "linux")]
    restore_cpu_affinity(&original_cpuset);

    // Phase 6+7 completed normally — defuse the guard so Drop is a no-op.
    transfer_guard.defused = true;

    // ========================================================================
    // Phase 8: Return the result
    // ========================================================================
    unsafe { (*result_cell.get()).take() }.expect("block_on future completed but result not stored")
}

thread_local! {
    static CURRENT: std::cell::Cell<*const ()> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// Get current context if on a worker thread
pub(crate) fn with_current<R>(f: impl FnOnce(Option<&Context>) -> R) -> R {
    CURRENT.with(|current| {
        let ptr = current.get();
        if ptr.is_null() {
            f(None)
        } else {
            // Safety: ptr was set by us in run()
            f(Some(unsafe { &*(ptr as *const Context) }))
        }
    })
}

/// Get the current DPDK worker's tick count for debugging.
///
/// Returns `None` if not on a DPDK worker thread.
///
/// The tick is incremented once per iteration of the event loop.
/// This is useful for debugging scheduling frequency and identifying
/// scheduling issues.
pub fn current_tick() -> Option<u32> {
    with_current(|ctx| {
        let ctx = ctx?;
        let core = ctx.core.borrow();
        core.as_ref().map(|c| c.tick)
    })
}

/// Get the current DPDK worker's execution count for debugging.
///
/// Returns `None` if not on a DPDK worker thread.
///
/// Unlike `tick` which increments on every event loop iteration (including idle),
/// `exec_count` only increments when a task is actually executed.
/// This is useful for measuring actual work vs idle polling.
pub fn current_exec_count() -> Option<u64> {
    with_current(|ctx| {
        let ctx = ctx?;
        let core = ctx.core.borrow();
        core.as_ref().map(|c| c.exec_count)
    })
}

/// Get DPDK scheduler debug statistics.
///
/// Returns `None` if not on a DPDK worker thread.
///
/// This includes:
/// - `tick`: Current event loop iteration count (includes idle)
/// - `exec_count`: Actual task execution count (only incremented on task run)
/// - `lifo_enabled`: Whether LIFO slot is enabled
/// - `local_queue_len`: Number of tasks in local run queue
/// - `overflow_len`: Number of tasks in overflow queue
#[derive(Debug, Clone, Copy)]
pub struct DpdkSchedulerStats {
    /// Event loop iteration count (increments on every loop, including idle)
    pub tick: u32,
    /// Actual task execution count (only increments when a task is polled)
    pub exec_count: u64,
    /// Whether LIFO slot optimization is enabled
    pub lifo_enabled: bool,
    /// Number of tasks in local run queue
    pub local_queue_len: usize,
    /// Number of tasks in overflow queue
    pub overflow_len: usize,
}

/// Get DPDK scheduler statistics for the current worker.
pub fn current_scheduler_stats() -> Option<DpdkSchedulerStats> {
    with_current(|ctx| {
        let ctx = ctx?;
        let core = ctx.core.borrow();
        let c = core.as_ref()?;
        Some(DpdkSchedulerStats {
            tick: c.tick,
            exec_count: c.exec_count,
            lifo_enabled: c.lifo_enabled,
            local_queue_len: c.run_queue.len(),
            overflow_len: c.local_overflow.len(),
        })
    })
}

/// Access the current worker's DpdkDriver.
///
/// Returns `None` if:
/// - Not on a DPDK worker thread
///
/// This is the primary API for TcpDpdkStream to access the network stack.
/// Uses blocking lock since the driver is only held briefly during poll.
pub(crate) fn with_current_driver<R>(f: impl FnOnce(&mut DpdkDriver) -> R) -> Option<R> {
    with_current(|ctx| {
        let ctx = ctx?;
        let index = ctx.worker.index;
        let driver_mutex = ctx.worker.handle.shared.drivers.get(index)?;
        // Use blocking lock since contention is brief (during driver.poll())
        let mut driver = driver_mutex.lock().ok()?;
        Some(f(&mut driver))
    })
}

/// Get the current worker's index.
///
/// Returns `None` if not on a DPDK worker thread.
pub(crate) fn current_worker_index() -> Option<usize> {
    with_current(|ctx| ctx.map(|c| c.worker.index))
}

impl Context {
    /// Main event loop - busy-poll variant (no parking)
    fn run(&self, core: Box<Core>) -> RunResult {
        // Store core in context
        *self.core.borrow_mut() = Some(core);

        // Track LIFO poll count to prevent starvation
        let mut lifo_polls = 0usize;

        loop {
            // Check if runtime is shutting down (handle-level check)
            if self.worker.handle.is_shutdown() {
                return self.shutdown_core();
            }

            // Check per-worker shutdown signal (from Remote)
            // This is set by run_with_future to trigger Core transfer
            let remote = &self.worker.handle.shared.remotes[self.worker.index];
            let remote_shutdown = remote.shutdown.load(Ordering::Acquire);
            if remote_shutdown {
                // Worker 0 special case: transfer Core to main thread for block_on
                let transfer_req = remote.transfer_requested.load(Ordering::Acquire);
                if self.worker.index == 0 && transfer_req {
                    // Take Core from self and put in transfer_core
                    let core = self
                        .core
                        .borrow_mut()
                        .take()
                        .expect("worker must have core");
                    self.worker.handle.shared.transfer_core.set(core);

                    // Wait at pause barrier - main thread will also arrive here
                    remote.pause_barrier.wait();

                    // Wait at resume barrier - main thread signals when Core is ready
                    remote.resume_barrier.wait();

                    // Check if runtime is shutting down after resume
                    if self.worker.handle.is_shutdown() {
                        return self.shutdown_core();
                    }

                    // Take Core back from transfer_core
                    match self.worker.handle.shared.transfer_core.take() {
                        Some(core) => {
                            *self.core.borrow_mut() = Some(core);
                            // Continue event loop
                            continue;
                        }
                        None => {
                            // Core was lost (main thread panicked and could not
                            // recover it). Shut down this worker gracefully.
                            return self.shutdown_core();
                        }
                    }
                }
                return self.shutdown_core();
            }

            // Check core-level shutdown (including block_on completion or transfer request)
            let is_core_shutdown = self
                .core
                .borrow()
                .as_ref()
                .map(|core| core.is_shutdown)
                .unwrap_or(false);
            if is_core_shutdown {
                // Core shutdown means block_on future completed (main thread running)
                // or normal shutdown - just exit the loop
                return self.shutdown_core();
            }

            // Debug: record tick start
            #[cfg(feature = "dpdk-debug")]
            debug::record_tick_start();

            // Increment tick
            self.tick();

            // Debug: record poll_dpdk_driver start
            #[cfg(feature = "dpdk-debug")]
            debug::record_poll_driver_start();

            // Poll DPDK network stack (receive packets, process TCP/IP, wake socket tasks)
            self.poll_dpdk_driver();

            // Debug: record poll_dpdk_driver end
            #[cfg(feature = "dpdk-debug")]
            debug::record_poll_driver_end();

            // Process any pending factory closures from spawn_local_on
            self.process_local_spawn_queue();

            // Start tracking scheduled task processing
            if let Some(core) = self.core.borrow_mut().as_mut() {
                core.stats.start_processing_scheduled_tasks();
            }

            // Debug: record next_task start
            #[cfg(feature = "dpdk-debug")]
            debug::record_next_task_start();

            // Poll for next task with LIFO starvation prevention
            if let Some(task) = self.next_task_with_fairness(&mut lifo_polls) {
                // Debug: record next_task end and run_task start
                #[cfg(feature = "dpdk-debug")]
                {
                    debug::record_next_task_end();
                    debug::record_run_task_start();
                }

                self.run_task(task)?;

                // Debug: record run_task end
                #[cfg(feature = "dpdk-debug")]
                debug::record_run_task_end();
            } else {
                // Debug: record next_task end (no task)
                #[cfg(feature = "dpdk-debug")]
                debug::record_next_task_end();
            }

            // End tracking scheduled task processing
            if let Some(core) = self.core.borrow_mut().as_mut() {
                core.stats.end_processing_scheduled_tasks();
            }

            // Poll block_on future if present (unified event loop design)
            // This allows the block_on future to be polled alongside spawned tasks
            // and DPDK network processing, with full maintenance support.
            {
                let mut state_opt = self.block_on_state.borrow_mut();
                if let Some(ref mut state) = *state_opt {
                    if !state.completed {
                        // Use real waker from Handle to ensure spawned tasks can wake
                        let waker = Handle::waker_ref(&self.worker.handle);
                        let mut cx = std::task::Context::from_waker(&waker);
                        // Wrap in coop budget so the block_on future yields
                        // fairly with spawned tasks.
                        let poll_result = crate::task::coop::budget(|| {
                            (state.poll_fn)(&mut cx)
                        });
                        if poll_result.is_ready() {
                            state.completed = true;
                            // Signal shutdown to exit the loop
                            drop(state_opt); // Drop borrow before borrowing core
                            if let Some(core) = self.core.borrow_mut().as_mut() {
                                core.is_shutdown = true;
                            }
                        }
                    }
                }
            }

            // Maintenance every event_interval ticks (includes defer.wake())
            self.maybe_maintenance();

            // Debug: record tick end
            #[cfg(feature = "dpdk-debug")]
            debug::record_tick_end();
        }
    }

    /// Poll the DPDK network driver for this worker.
    ///
    /// This is called on every tick to:
    /// 1. Receive packets from DPDK
    /// 2. Process them through smoltcp TCP/IP stack
    /// 3. Wake async tasks waiting on socket readiness
    fn poll_dpdk_driver(&self) {
        // Get worker index to access correct driver
        let index = self.worker.index;

        // Access the driver for this worker (each worker has dedicated driver)
        if let Some(driver_mutex) = self.worker.handle.shared.drivers.get(index) {
            if let Ok(mut driver) = driver_mutex.try_lock() {
                // Skip poll if no sockets are registered (optimization)
                if !driver.has_registered_sockets() {
                    return;
                }
                // Poll with current time
                let now = Instant::now();
                driver.poll(now);
            }
            // If lock fails, another thread is using it - skip this tick
        }
    }

    fn tick(&self) {
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.tick = core.tick.wrapping_add(1);
        }
    }

    /// Get next task with fairness - uses MAX_LIFO_POLLS_PER_TICK and rand
    /// This implementation matches the multi_thread scheduler behavior:
    /// - Periodically check global queue
    /// - Batch-pull tasks from inject queue to local queue
    /// - LIFO slot with starvation prevention
    fn next_task_with_fairness(&self, lifo_polls: &mut usize) -> Option<Notified> {
        // Check per-worker inject queue first (targeted tasks from spawn_on).
        // This is checked every tick since the atomic load is cheap and
        // targeted tasks should be picked up promptly.
        if let Some(task) = self.next_per_worker_task() {
            *lifo_polls = 0;
            return Some(task);
        }

        let mut core = self.core.borrow_mut();
        let core = core.as_mut()?;

        // Check global queue periodically
        if core.tick % core.global_queue_interval == 0 {
            // Try to get from remote (inject) queue
            if let Some(task) = self.worker.handle.next_remote_task() {
                *lifo_polls = 0;
                return Some(task);
            }

            // If inject queue has more tasks, batch-pull them to local queue
            // (similar to multi_thread scheduler's next_task logic)
            self.batch_pull_from_inject(core);
        } else {
            // Use rand for occasional global queue checks (for fairness)
            if core.rand.fastrand_n(64) == 0 {
                if let Some(task) = self.worker.handle.next_remote_task() {
                    *lifo_polls = 0;
                    return Some(task);
                }
            }
        }

        // Check LIFO slot with starvation prevention
        if core.lifo_enabled && *lifo_polls < MAX_LIFO_POLLS_PER_TICK {
            if let Some(task) = core.lifo_slot.take() {
                *lifo_polls += 1;
                super::counters::inc_lifo_schedules();
                return Some(task);
            }
        } else if *lifo_polls >= MAX_LIFO_POLLS_PER_TICK {
            // LIFO polling was capped to prevent starvation
            core.lifo_enabled = false;
            super::counters::inc_lifo_capped();
            *lifo_polls = 0;
        }

        // Check local run queue first (primary source)
        if let Some(task) = core.run_queue.pop() {
            return Some(task);
        }

        // Check local overflow queue (tasks pushed when run_queue was full)
        if let Some(task) = core.local_overflow.pop_front() {
            return Some(task);
        }

        // As last resort, check LIFO slot even if we exceeded count
        core.lifo_slot.take()
    }

    /// Get next task for block_on event loop.
    /// This is the same as next_task_with_fairness but exposed for use in run_with_future.
    #[allow(dead_code)] // Reserved for alternative block_on implementations
    pub(crate) fn next_task_block_on(&self, lifo_polls: &mut usize) -> Option<Notified> {
        // Delegate to next_task_with_fairness
        self.next_task_with_fairness(lifo_polls)
    }

    /// Batch-pull tasks from inject queue to local queue (like multi_thread scheduler)
    fn batch_pull_from_inject(&self, core: &mut Core) {
        let inject = &self.worker.handle.shared.inject;

        // Check if inject queue has tasks
        if inject.is_empty() {
            return;
        }

        // Calculate how many tasks we can pull (use remaining_slots and max_capacity)
        let cap = usize::min(
            core.run_queue.remaining_slots(),
            core.run_queue.max_capacity() / 2,
        );

        if cap == 0 {
            return;
        }

        // Calculate batch size based on number of workers
        let num_workers = self.worker.handle.shared.remotes.len();
        let inject_len = inject.len();
        let n = usize::min(inject_len / num_workers + 1, cap);
        let n = usize::max(1, n);

        // Pop tasks from inject queue and push to local queue
        let tasks: Vec<_> = {
            let mut synced = self.worker.handle.shared.synced.lock();
            // Safety: passing in the correct inject::Synced
            unsafe { inject.pop_n(&mut synced.inject, n) }.collect()
        };
        // synced is dropped here, lock released

        // Push tasks to local queue, with overflow handling.
        // Between the capacity check above and now, other threads may have
        // pushed tasks to us (via inject queue scheduling), so we must
        // handle the case where run_queue is now full.
        let actual_remaining = core.run_queue.remaining_slots();
        if actual_remaining >= tasks.len() {
            // Safe to push all directly
            core.run_queue.push_back(tasks.into_iter());
        } else {
            // Split: push what fits to run_queue, rest to local_overflow
            let (fit, overflow) = {
                let mut v = tasks;
                let overflow = v.split_off(actual_remaining);
                (v, overflow)
            };
            if !fit.is_empty() {
                core.run_queue.push_back(fit.into_iter());
            }
            for task in overflow {
                core.local_overflow.push_back(task);
                core.stats.incr_overflow_count();
            }
        }
    }

    fn run_task(&self, task: Notified) -> Result<(), ()> {
        // Start poll tracking and increment execution counter
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.stats.start_poll();
            // Increment exec_count - only counts actual task executions
            core.exec_count = core.exec_count.wrapping_add(1);
        } else {
            // No core available - should not happen
            return Err(());
        }

        // Poll the task with cooperative budget so a single task cannot
        // monopolize the worker. IO operations and sync primitives call
        // poll_proceed() internally, which decrements this budget.
        let task = self.worker.handle.shared.owned.assert_owner(task);
        crate::task::coop::budget(|| {
            task.run();
        });

        // End poll tracking (LIFO stays disabled in DPDK scheduler)
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.stats.end_poll();
            // LIFO stays disabled in DPDK for fair multi-connection scheduling
            // core.lifo_enabled = false; // already false, no need to reset
        }

        // Core remains in self.core
        Ok(())
    }

    fn maybe_maintenance(&self) {
        let should_maintain = {
            let core = self.core.borrow();
            if let Some(c) = core.as_ref() {
                let ei = self.worker.handle.shared.config.event_interval;
                c.tick % ei == 0
            } else {
                false
            }
        };

        if should_maintain {
            self.maintenance();
        }
    }

    fn maintenance(&self) {
        super::counters::inc_num_maintenance();

        // Tune global queue interval and submit metrics
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.global_queue_interval = core
                .stats
                .tuned_global_queue_interval(&self.worker.handle.shared.config);

            // Submit worker stats
            let worker_idx = self.worker.index;
            core.stats
                .submit(&self.worker.handle.shared.worker_metrics[worker_idx]);

            // Check if we have pending work (for diagnostics)
            if core.has_tasks() && core.should_notify_others() {
                // In standard multi-thread scheduler this would be used
                // for work stealing, but DPDK doesn't steal work
                super::counters::inc_num_maintenance();
            }
        }

        // Buffer pool replenishment - inline to avoid hot-path allocation
        self.replenish_buffer_pool_if_needed();

        // Reset LIFO enabled state
        self.reset_lifo_enabled();

        // Wake deferred tasks (yield_now) - only during maintenance to prevent starvation
        // This matches original tokio behavior: defer.wake() is called in park_internal,
        // which is triggered during maintenance (event_interval ticks).
        self.defer.wake();
    }

    /// Replenish buffer pool if below low watermark.
    /// Called during maintenance to keep allocation off the hot path.
    fn replenish_buffer_pool_if_needed(&self) {
        const REPLENISH_BATCH_SIZE: usize = 16;

        let index = self.worker.index;
        if let Some(driver_mutex) = self.worker.handle.shared.drivers.get(index) {
            // Use try_lock to avoid blocking if driver is busy
            if let Ok(mut guard) = driver_mutex.try_lock() {
                let driver = &mut *guard;
                let needs = driver.buffer_pool_needs_replenish();
                if needs {
                    let replenished = driver.buffer_pool_replenish(REPLENISH_BATCH_SIZE);
                    if replenished > 0 {
                        super::counters::inc_num_maintenance(); // Track as maintenance work
                    }
                }
            }
        }
    }

    fn shutdown_core(&self) -> RunResult {
        let core = self.core.borrow_mut().take();
        if let Some(mut core) = core {
            core.is_shutdown = true;

            // Drain local queue
            while let Some(task) = core.run_queue.pop() {
                drop(task);
            }

            // Drain local overflow queue
            while let Some(task) = core.local_overflow.pop_front() {
                drop(task);
            }

            // Drain LIFO slot
            drop(core.lifo_slot.take());

            // Drain per-worker inject queue
            let remote = &self.worker.handle.shared.remotes[self.worker.index];
            while let Some(task) = remote.per_inject.pop() {
                drop(task);
            }

            // Drain local_spawn_queue (discard pending factory closures)
            {
                let mut queue = remote.local_spawn_queue.lock().unwrap();
                queue.clear();
            }

            // Check if this is a block_on scenario - if so, return Core instead of
            // giving it to handle.shutdown_core(), so it can be respawned
            if self.block_on_state.borrow().is_some() {
                // Reset is_shutdown for next use
                core.is_shutdown = false;
                return Ok(core);
            }

            // Normal shutdown path - add to shutdown cores
            self.worker.handle.shutdown_core(core);
        }
        Err(())
    }

    /// Defer a waker for later waking
    pub(crate) fn defer(&self, waker: &std::task::Waker) {
        self.defer.defer(waker);
    }

    /// Reset LIFO enabled state
    /// Note: LIFO is always disabled in DPDK scheduler for fair scheduling
    pub(crate) fn reset_lifo_enabled(&self) {
        // In DPDK scheduler, LIFO stays disabled for fair multi-connection scheduling
        // No-op since lifo_enabled is always false
    }

    /// Process pending factory closures from the local_spawn_queue.
    ///
    /// This drains closures pushed by `spawn_local_on` and executes them
    /// on this worker thread, where they create and spawn !Send futures.
    fn process_local_spawn_queue(&self) {
        let remote = &self.worker.handle.shared.remotes[self.worker.index];
        let factories: Vec<Box<dyn FnOnce() + Send + 'static>> = {
            let mut queue = remote.local_spawn_queue.lock().unwrap();
            if queue.is_empty() {
                return;
            }
            std::mem::take(&mut *queue)
        };
        for factory in factories {
            factory();
        }
    }

    /// Pop a task from this worker's per-worker inject queue.
    fn next_per_worker_task(&self) -> Option<Notified> {
        self.worker.handle.shared.remotes[self.worker.index]
            .per_inject
            .pop()
    }
}

impl Core {
    /// Check if there are tasks pending
    pub(crate) fn has_tasks(&self) -> bool {
        self.lifo_slot.is_some() || self.run_queue.has_tasks()
    }

    /// Check if we should notify other workers
    pub(crate) fn should_notify_others(&self) -> bool {
        // In DPDK, we don't notify others since there's no work stealing
        false
    }
}

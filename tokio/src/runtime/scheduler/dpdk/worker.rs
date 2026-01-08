//! DPDK scheduler worker implementation.
//!
//! This is adapted from `multi_thread/worker.rs` with the following changes:
//! - No work stealing (workers are independent)
//! - No parking (busy-poll)
//! - No Idle management
//! - Per-core DPDK driver

use crate::loom::cell::UnsafeCell;
use crate::loom::sync::atomic::AtomicBool;
use crate::loom::sync::{Arc, Mutex, MutexGuard};
use crate::runtime::scheduler::defer::Defer;
use crate::runtime::scheduler::inject;
use crate::runtime::task::{self, OwnedTasks};
use crate::runtime::{blocking, driver, Config, SchedulerMetrics, WorkerMetrics};
use crate::util::atomic_cell::AtomicCell;
use crate::util::rand::FastRand;

use std::cell::RefCell;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};

use super::counters::Counters;
use super::queue::{self, Local, Overflow};
use super::stats::Stats;

/// A scheduler worker
pub(super) struct Worker {
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
    pub(crate) tick: u32,

    /// LIFO optimization slot - last scheduled task runs first.
    pub(crate) lifo_slot: Option<Notified>,

    /// When `true`, locally scheduled tasks go to the LIFO slot.
    pub(crate) lifo_enabled: bool,

    /// The worker-local run queue.
    pub(crate) run_queue: Local<Arc<Handle>>,

    /// True if the scheduler is being shutdown.
    pub(crate) is_shutdown: bool,

    /// True if the scheduler is being traced.
    pub(crate) is_traced: bool,

    /// Per-worker runtime stats.
    pub(crate) stats: Stats,

    /// How often to check the global queue.
    pub(crate) global_queue_interval: u32,

    /// Fast random number generator.
    pub(crate) rand: FastRand,
}

/// State shared across all workers
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

    /// Collects metrics from the runtime.
    pub(super) scheduler_metrics: SchedulerMetrics,

    /// Per-worker metrics.
    pub(super) worker_metrics: Box<[WorkerMetrics]>,

    /// Internal counters (only active with cfg flag).
    pub(super) _counters: Counters,
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
    pub(super) core_id: usize,

    /// Shutdown signal for this worker.
    pub(super) shutdown: AtomicBool,
}

/// Thread-local context
pub(crate) struct Context {
    /// Worker.
    pub(crate) worker: Arc<Worker>,

    /// Core data.
    pub(crate) core: RefCell<Option<Box<Core>>>,

    /// Tasks to wake after polling. Used for yielded tasks.
    pub(crate) defer: Defer,
}

/// Starts the workers
pub(crate) struct Launch(pub(super) Vec<Arc<Worker>>);

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
use std::sync::atomic::Ordering;

/// Creates the DPDK scheduler workers.
///
/// Unlike multi_thread, this takes a list of core IDs for CPU affinity.
pub(super) fn create(
    core_ids: Vec<usize>,
    driver_handle: driver::Handle,
    blocking_spawner: blocking::Spawner,
    seed_generator: RngSeedGenerator,
    config: Config,
) -> (Arc<Handle>, Launch) {
    let size = core_ids.len();
    let mut cores = Vec::with_capacity(size);
    let mut remotes = Vec::with_capacity(size);
    let mut worker_metrics = Vec::with_capacity(size);

    // Create the local queues
    for &core_id in &core_ids {
        let run_queue = queue::local();
        let metrics = WorkerMetrics::from_config(&config);
        let stats = Stats::new(&metrics);

        cores.push(Box::new(Core {
            tick: 0,
            lifo_slot: None,
            lifo_enabled: !config.disable_lifo_slot,
            run_queue,
            is_shutdown: false,
            is_traced: false,
            global_queue_interval: stats.tuned_global_queue_interval(&config),
            stats,
            rand: FastRand::from_seed(config.seed_generator.next_seed()),
        }));

        remotes.push(Remote {
            core_id,
            shutdown: AtomicBool::new(false),
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
            scheduler_metrics: SchedulerMetrics::new(),
            worker_metrics: worker_metrics.into_boxed_slice(),
            _counters: Counters,
        },
        driver: driver_handle,
        blocking_spawner,
        seed_generator,
        timer_flavor: TimerFlavor::Traditional,
        is_shutdown: AtomicBool::new(false),
    });

    let mut launch = Launch(vec![]);

    for (index, (core, &core_id)) in cores.drain(..).zip(core_ids.iter()).enumerate() {
        launch.0.push(Arc::new(Worker {
            handle: handle.clone(),
            index,
            core_id,
            core: AtomicCell::new(Some(core)),
        }));
    }

    (handle, launch)
}

impl Launch {
    /// Launch all worker threads.
    pub(crate) fn launch(self) {
        for worker in self.0 {
            let worker_clone = worker.clone();
            let core_id = worker.core_id;

            // Spawn worker thread with CPU affinity
            std::thread::Builder::new()
                .name(format!("dpdk-worker-{}", core_id))
                .spawn(move || {
                    // Set CPU affinity
                    #[cfg(target_os = "linux")]
                    {
                        use std::os::unix::thread::JoinHandleExt;
                        let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
                        unsafe {
                            libc::CPU_ZERO(&mut cpuset);
                            libc::CPU_SET(core_id, &mut cpuset);
                            libc::pthread_setaffinity_np(
                                libc::pthread_self(),
                                std::mem::size_of::<libc::cpu_set_t>(),
                                &cpuset,
                            );
                        }
                    }

                    run(worker_clone);
                })
                .expect("failed to spawn worker thread");
        }
    }
}

/// Worker thread entry point
pub(crate) fn run(worker: Arc<Worker>) {
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

impl Context {
    /// Main event loop - busy-poll variant (no parking)
    fn run(&self, core: Box<Core>) -> RunResult {
        // Store core in context
        *self.core.borrow_mut() = Some(core);

        loop {
            // Check shutdown
            if let Some(core) = self.core.borrow().as_ref() {
                if core.is_shutdown {
                    return self.shutdown_core();
                }
            }

            // Increment tick
            self.tick();

            // Poll for next task
            if let Some(task) = self.next_task() {
                self.run_task(task)?;
            }

            // Maintenance every event_interval ticks
            self.maybe_maintenance();

            // Wake deferred tasks
            self.defer.wake();
        }
    }

    fn tick(&self) {
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.tick = core.tick.wrapping_add(1);
        }
    }

    fn next_task(&self) -> Option<Notified> {
        let mut core = self.core.borrow_mut();
        let core = core.as_mut()?;

        // Check global queue periodically
        if core.tick % core.global_queue_interval == 0 {
            if let Some(task) = self.worker.handle.next_remote_task() {
                return Some(task);
            }
        }

        // Check LIFO slot
        if let Some(task) = core.lifo_slot.take() {
            return Some(task);
        }

        // Check local queue
        core.run_queue.pop()
    }

    fn run_task(&self, task: Notified) -> RunResult {
        let core = self.core.borrow_mut().take();
        let mut core = match core {
            Some(c) => c,
            None => return Err(()),
        };

        core.stats.start_poll();

        // Poll the task using the owned tasks assert_owner pattern
        // Safety: we own this task on this worker
        let task = self.worker.handle.shared.owned.assert_owner(task);
        task.run();

        core.stats.end_poll();

        // Reset LIFO enabled after each task
        core.lifo_enabled = !self.worker.handle.shared.config.disable_lifo_slot;

        // Put core back
        *self.core.borrow_mut() = Some(core);

        Ok(self.core.borrow_mut().take().unwrap())
    }

    fn maybe_maintenance(&self) {
        let should_maintain = {
            let core = self.core.borrow();
            if let Some(c) = core.as_ref() {
                c.tick % self.worker.handle.shared.config.event_interval == 0
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

        // Tune global queue interval
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.global_queue_interval = core
                .stats
                .tuned_global_queue_interval(&self.worker.handle.shared.config);

            // Submit stats
            let worker_idx = self.worker.index;
            core.stats
                .submit(&self.worker.handle.shared.worker_metrics[worker_idx]);
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

            // Drain LIFO slot
            drop(core.lifo_slot.take());

            // Add to shutdown cores
            self.worker.handle.shutdown_core(core);
        }
        Err(())
    }

    /// Defer a waker for later waking
    pub(crate) fn defer(&self, waker: &std::task::Waker) {
        self.defer.defer(waker);
    }

    /// Reset LIFO enabled state
    pub(crate) fn reset_lifo_enabled(&self) {
        if let Some(core) = self.core.borrow_mut().as_mut() {
            core.lifo_enabled = !self.worker.handle.shared.config.disable_lifo_slot;
        }
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

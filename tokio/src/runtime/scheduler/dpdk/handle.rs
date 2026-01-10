//! Handle to the DPDK scheduler.
//!
//! This is adapted from `multi_thread/handle.rs`.

use crate::future::Future;
use crate::loom::sync::atomic::{AtomicBool, Ordering};
use crate::loom::sync::Arc;
use crate::runtime::task::{
    self, JoinHandle, Notified, SpawnLocation, Task, TaskHarnessScheduleHooks,
};
use crate::runtime::{blocking, driver, TaskHooks, TaskMeta, TimerFlavor};
use crate::util::RngSeedGenerator;

use std::fmt;
use std::num::NonZeroU64;

use super::worker::{self, Notified as WorkerNotified, Shared};

/// Handle to the DPDK scheduler
pub(crate) struct Handle {
    /// Task spawner (shared state)
    pub(super) shared: Shared,

    /// Resource driver handles
    pub(crate) driver: driver::Handle,

    /// Blocking pool spawner
    pub(crate) blocking_spawner: blocking::Spawner,

    /// Current random number generator seed
    pub(crate) seed_generator: RngSeedGenerator,

    /// User-supplied hooks to invoke
    pub(crate) task_hooks: TaskHooks,

    /// Timer flavor (always Traditional for DPDK, reserved for future high-precision timers)
    #[allow(dead_code)]
    pub(crate) timer_flavor: TimerFlavor,

    /// Indicates that the runtime is shutting down
    pub(crate) is_shutdown: AtomicBool,
}

impl Handle {
    /// Spawns a future onto the thread pool
    pub(crate) fn spawn<F>(
        me: &Arc<Self>,
        future: F,
        id: task::Id,
        spawned_at: SpawnLocation,
    ) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Self::bind_new_task(me, future, id, spawned_at)
    }

    /// Returns whether the runtime is shutting down
    pub(crate) fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::SeqCst)
    }

    /// Shuts down the runtime.
    ///
    /// This properly cleans up all spawned tasks by:
    /// 1. Closing the inject queue to prevent new tasks
    /// 2. Shutting down all owned tasks (dropping futures)
    /// 3. Draining any remaining tasks from the inject queue
    pub(crate) fn shutdown(&self) {
        // First, close the inject queue and signal workers to stop
        self.close();

        // Mark as shutting down
        self.is_shutdown.store(true, Ordering::SeqCst);

        // Shut down all owned tasks. This calls shutdown() on each task,
        // which causes futures to be dropped and resources to be released.
        // The parameter 0 is the starting shard for iteration.
        self.shared.owned.close_and_shutdown_all(0);

        // Drain any remaining tasks from the inject queue
        let mut synced = self.shared.synced.lock();
        // Safety: `synced.inject` was created together with `self.shared.inject`
        // in the same `Shared` struct construction.
        while let Some(task) = unsafe { self.shared.inject.pop(&mut synced.inject) } {
            drop(task);
        }
    }

    /// Binds a new task to the scheduler
    #[track_caller]
    pub(super) fn bind_new_task<T>(
        me: &Arc<Self>,
        future: T,
        id: task::Id,
        spawned_at: SpawnLocation,
    ) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        let (handle, notified) = me.shared.owned.bind(future, me.clone(), id, spawned_at);

        me.task_hooks.spawn(&TaskMeta {
            id,
            spawned_at,
            _phantom: Default::default(),
        });

        me.schedule_option_task_without_yield(notified);

        handle
    }

    /// Closes the inject queue
    pub(crate) fn close(&self) {
        if self
            .shared
            .inject
            .close(&mut self.shared.synced.lock().inject)
        {
            // Signal workers to stop
            for remote in self.shared.remotes.iter() {
                remote.shutdown.store(true, Ordering::Release);
            }
        }
    }

    /// Schedules a task
    pub(crate) fn schedule_task(&self, task: WorkerNotified, is_yield: bool) {
        // Try to schedule locally if we're on a worker thread
        worker::with_current(|maybe_cx| {
            if let Some(cx) = maybe_cx {
                // Check if same runtime by comparing shared state pointers
                // cx.worker.handle.shared and self.shared should be identical if same runtime
                let same_runtime = std::ptr::eq(
                    &cx.worker.handle.shared as *const _,
                    &self.shared as *const _,
                );

                if same_runtime {
                    // Schedule locally
                    if let Some(core) = cx.core.borrow_mut().as_mut() {
                        self.schedule_local(core, task, is_yield);
                        return;
                    }
                }
            }

            // Fall back to global queue
            self.push_remote_task(task);
        });
    }

    /// Schedules a task on the local queue
    pub(crate) fn schedule_local(
        &self,
        core: &mut worker::Core,
        task: WorkerNotified,
        is_yield: bool,
    ) {
        core.stats.inc_local_schedule_count();

        // If LIFO is enabled and not yielding, use LIFO slot
        if core.lifo_enabled && !is_yield {
            if let Some(prev) = core.lifo_slot.replace(task) {
                // Push previous to run queue, overflow to local_overflow if full
                self.push_to_local_queue_or_overflow(core, prev);
            }
            super::counters::inc_lifo_schedules();
        } else {
            // Push to run queue, overflow to local_overflow if full
            self.push_to_local_queue_or_overflow(core, task);
        }
    }

    /// Push task to local run queue, or local overflow queue if full.
    /// This keeps tasks on the same worker (no cross-worker migration).
    fn push_to_local_queue_or_overflow(&self, core: &mut worker::Core, task: WorkerNotified) {
        // Check if run_queue has space
        if core.run_queue.remaining_slots() > 0 {
            // Safe to push directly (won't trigger Overflow trait)
            core.run_queue.push_back(std::iter::once(task));
        } else {
            // Queue full - use local overflow (lock-free, same worker)
            core.local_overflow.push_back(task);
            core.stats.incr_overflow_count();
        }
    }

    /// Schedules an optional task (used in bind_new_task)
    pub(crate) fn schedule_option_task_without_yield(&self, task: Option<WorkerNotified>) {
        if let Some(task) = task {
            self.schedule_task(task, false);
        }
    }

    /// Gets the next task from the global queue
    pub(crate) fn next_remote_task(&self) -> Option<WorkerNotified> {
        // Safety: we hold the synced lock
        unsafe {
            self.shared
                .inject
                .pop(&mut self.shared.synced.lock().inject)
        }
    }

    /// Pushes a task to the global queue
    pub(crate) fn push_remote_task(&self, task: WorkerNotified) {
        // Safety: we hold the synced lock
        unsafe {
            self.shared
                .inject
                .push(&mut self.shared.synced.lock().inject, task);
        }
    }

    /// Shuts down a worker's core
    pub(crate) fn shutdown_core(&self, core: Box<worker::Core>) {
        self.shared.shutdown_cores.lock().push(core);
    }

    /// Returns the owned tasks ID (for runtime::Handle::id())
    pub(crate) fn owned_id(&self) -> NonZeroU64 {
        self.shared.owned.id
    }

    // ---- Methods required by scheduler::Handle compatibility ----

    /// Returns the number of worker threads
    pub(crate) fn num_workers(&self) -> usize {
        self.shared.remotes.len()
    }

    /// Returns the number of alive tasks
    pub(crate) fn num_alive_tasks(&self) -> usize {
        self.shared.owned.num_alive_tasks()
    }

    /// Returns the depth of the injection queue
    pub(crate) fn injection_queue_depth(&self) -> usize {
        self.shared.inject.len()
    }

    /// Returns worker metrics for a specific worker
    pub(crate) fn worker_metrics(&self, worker: usize) -> &crate::runtime::WorkerMetrics {
        &self.shared.worker_metrics[worker]
    }
}

impl task::Schedule for Arc<Handle> {
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>> {
        self.shared.owned.remove(task)
    }

    fn schedule(&self, task: Notified<Self>) {
        self.schedule_task(task, false);
    }

    fn hooks(&self) -> TaskHarnessScheduleHooks {
        TaskHarnessScheduleHooks {
            task_terminate_callback: self.task_hooks.task_terminate_callback.clone(),
        }
    }

    fn yield_now(&self, task: Notified<Self>) {
        self.schedule_task(task, true);
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("dpdk::Handle { ... }").finish()
    }
}

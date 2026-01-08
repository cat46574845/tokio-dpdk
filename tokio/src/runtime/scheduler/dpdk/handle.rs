//! Handle to the DPDK scheduler.
//!
//! This is adapted from `multi_thread/handle.rs`.

use crate::future::Future;
use crate::loom::sync::atomic::{AtomicBool, Ordering};
use crate::loom::sync::{Arc, Mutex, MutexGuard};
use crate::runtime::scheduler::inject;
use crate::runtime::task::{
    self, JoinHandle, Notified, SpawnLocation, Task, TaskHarnessScheduleHooks,
};
use crate::runtime::{blocking, driver, TaskHooks, TaskMeta, TimerFlavor};
use crate::util::RngSeedGenerator;

use std::fmt;
use std::num::NonZeroU64;

use super::queue::Overflow;
use super::worker::{self, Notified as WorkerNotified, Shared, Synced};

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

    /// Timer flavor (always Traditional for DPDK)
    pub(crate) timer_flavor: TimerFlavor,

    /// Indicates that the runtime is shutting down
    pub(crate) is_shutdown: AtomicBool,
}

/// Guard for accessing inject queue
pub(crate) struct InjectGuard<'a> {
    lock: MutexGuard<'a, Synced>,
}

impl<'a> AsMut<inject::Synced> for InjectGuard<'a> {
    fn as_mut(&mut self) -> &mut inject::Synced {
        &mut self.lock.inject
    }
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

    /// Shuts down the runtime
    pub(crate) fn shutdown(&self) {
        self.close();
        self.is_shutdown.store(true, Ordering::SeqCst);
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
                // Check if same runtime
                if Arc::ptr_eq(&cx.worker.handle, &Arc::new(todo!())) {
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
                // Push previous to run queue
                core.run_queue
                    .push_back_or_overflow(prev, self, &mut core.stats);
            }
            super::counters::inc_lifo_schedules();
        } else {
            // Push to run queue
            core.run_queue
                .push_back_or_overflow(task, self, &mut core.stats);
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

    /// Pointer equality check
    pub(crate) fn ptr_eq(&self, other: &Handle) -> bool {
        std::ptr::eq(self, other)
    }

    /// Returns the owned tasks ID (for runtime::Handle::id())
    pub(crate) fn owned_id(&self) -> NonZeroU64 {
        self.shared.owned.id
    }

    /// Notify all workers (no-op for DPDK since workers never park)
    pub(crate) fn notify_all(&self) {
        // DPDK workers are always running (busy-poll), no need to wake them
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

impl Overflow<Arc<Handle>> for Handle {
    fn push(&self, task: task::Notified<Arc<Handle>>) {
        self.push_remote_task(task);
    }

    fn push_batch<I>(&self, iter: I)
    where
        I: Iterator<Item = task::Notified<Arc<Handle>>>,
    {
        // Safety: we hold the synced lock
        unsafe {
            self.shared
                .inject
                .push_batch(&mut self.shared.synced.lock().inject, iter);
        }
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

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
use std::net::{Ipv4Addr, Ipv6Addr};
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
    /// 4. Cleaning up all DPDK devices (releasing mbufs back to mempool)
    pub(crate) fn shutdown(&self) {
        // First, close the inject queue and signal workers to stop
        self.close();

        // Close per-worker inject queues to prevent new targeted tasks
        for remote in self.shared.remotes.iter() {
            remote.per_inject.close();
        }

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
        drop(synced);

        // Drain per-worker inject queues
        for remote in self.shared.remotes.iter() {
            while let Some(task) = remote.per_inject.pop() {
                drop(task);
            }
        }

        // Note: DPDK device cleanup (mbufs) is handled automatically by DpdkDevice::drop,
        // which is guaranteed to run BEFORE DpdkResourcesCleaner::drop (mempool/EAL cleanup)
        // due to field ordering in Shared.
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

    /// Spawns a `Send` task targeted to a specific worker's queue.
    ///
    /// The task is placed in the target worker's per-worker inject queue, which
    /// guarantees it will be picked up only by that worker. If the caller is
    /// already on the target worker, the task is scheduled directly to the local queue.
    #[track_caller]
    pub(crate) fn spawn_on_worker<F>(
        me: &Arc<Self>,
        worker_index: usize,
        future: F,
        id: task::Id,
        spawned_at: SpawnLocation,
    ) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        assert!(
            worker_index < me.shared.remotes.len(),
            "spawn_on_worker: worker_index {} is out of range (num_workers: {})",
            worker_index,
            me.shared.remotes.len()
        );

        let (handle, notified) = me.shared.owned.bind(future, me.clone(), id, spawned_at);

        me.task_hooks.spawn(&TaskMeta {
            id,
            spawned_at,
            _phantom: Default::default(),
        });

        if let Some(task) = notified {
            // If already on the target worker, schedule locally for lower latency
            if worker::current_worker_index() == Some(worker_index) {
                worker::with_current(|ctx| {
                    if let Some(cx) = ctx {
                        if let Some(core) = cx.core.borrow_mut().as_mut() {
                            me.schedule_local(core, task, false);
                            return;
                        }
                    }
                    me.push_worker_task(worker_index, task);
                });
            } else {
                me.push_worker_task(worker_index, task);
            }
        }

        handle
    }

    /// Spawns a `!Send` future on the current DPDK worker thread.
    ///
    /// # Safety
    ///
    /// This uses `bind_local` which creates a task that is not `Send`.
    /// This is safe because DPDK workers:
    /// - Have CPU affinity (pinned to a specific core)
    /// - Never steal work from other workers
    /// - Only consume tasks from their own local queue
    ///
    /// These properties guarantee single-threaded execution for !Send futures.
    ///
    /// Returns `None` if not called from a DPDK worker thread of this runtime.
    #[track_caller]
    pub(crate) fn spawn_local_impl<F>(
        me: &Arc<Self>,
        future: F,
        id: task::Id,
        spawned_at: SpawnLocation,
    ) -> Option<JoinHandle<F::Output>>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        worker::with_current(|maybe_cx| {
            let cx = maybe_cx?;

            // Verify we're in the same runtime
            if !std::ptr::eq(&cx.worker.handle.shared as *const _, &me.shared as *const _) {
                return None;
            }

            // Safety: DPDK workers never migrate tasks (no work stealing, CPU affinity).
            // Tasks in the local queue are only consumed by the owning worker.
            let (handle, notified) = unsafe {
                me.shared.owned.bind_local(future, me.clone(), id, spawned_at)
            };

            me.task_hooks.spawn(&TaskMeta {
                id,
                spawned_at,
                _phantom: Default::default(),
            });

            // Schedule to local queue
            if let Some(task) = notified {
                if let Some(core) = cx.core.borrow_mut().as_mut() {
                    me.schedule_local(core, task, false);
                }
            }

            Some(handle)
        })
    }

    /// Pushes a task to a specific worker's per-worker inject queue.
    pub(crate) fn push_worker_task(&self, worker_index: usize, task: WorkerNotified) {
        self.shared.remotes[worker_index].per_inject.push(task);
    }

    /// Pops a task from a specific worker's per-worker inject queue.
    #[allow(dead_code)]
    pub(crate) fn next_worker_task(&self, worker_index: usize) -> Option<WorkerNotified> {
        self.shared.remotes[worker_index].per_inject.pop()
    }

    /// Pushes a factory closure to a specific worker's local_spawn_queue.
    ///
    /// The closure will be executed on the target worker thread, where it can
    /// create and spawn !Send futures.
    pub(crate) fn push_local_spawn_factory(
        &self,
        worker_index: usize,
        factory: Box<dyn FnOnce() + Send + 'static>,
    ) {
        assert!(worker_index < self.shared.remotes.len());
        let mut queue = self.shared.remotes[worker_index]
            .local_spawn_queue
            .lock()
            .unwrap();
        queue.push(factory);
    }

    /// Returns the IPv4 address configured on the specified worker's DPDK interface.
    pub(crate) fn worker_ipv4(&self, worker_index: usize) -> Option<Ipv4Addr> {
        let driver_mutex = self.shared.drivers.get(worker_index)?;
        let driver = driver_mutex.lock().ok()?;
        // smoltcp::wire::Ipv4Address is a type alias for std::net::Ipv4Addr
        driver.get_ipv4_address()
    }

    /// Returns the IPv6 address configured on the specified worker's DPDK interface.
    pub(crate) fn worker_ipv6(&self, worker_index: usize) -> Option<Ipv6Addr> {
        let driver_mutex = self.shared.drivers.get(worker_index)?;
        let driver = driver_mutex.lock().ok()?;
        // smoltcp::wire::Ipv6Address is a type alias for std::net::Ipv6Addr
        driver.get_ipv6_address()
    }

    /// Shuts down a worker's core.
    ///
    /// IMPORTANT: Each worker MUST drain its own local queues before calling this,
    /// because DPDK tasks have CPU affinity - they must be dropped on their own core.
    ///
    /// This function:
    /// 1. Accepts the already-drained core
    /// 2. Tracks shutdown progress
    /// 3. When all workers have shut down, drains the inject queue (no affinity requirement)
    pub(crate) fn shutdown_core(&self, core: Box<worker::Core>) {
        // Note: core.run_queue, local_overflow, and lifo_slot should already
        // be empty - drained by Context::shutdown_core on the correct CPU core.

        // Push to shutdown_cores for tracking
        let mut cores = self.shared.shutdown_cores.lock();
        cores.push(core);

        // If all workers have shut down, drain the inject queue and per-worker queues
        // The inject queue has no CPU affinity requirement
        if cores.len() == self.shared.remotes.len() {
            while let Some(task) = self.next_remote_task() {
                drop(task);
            }

            // Drain any remaining per-worker inject queue tasks
            for remote in self.shared.remotes.iter() {
                while let Some(task) = remote.per_inject.pop() {
                    drop(task);
                }
            }
        }
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

// ===== Wake trait implementation =====

use crate::util::{waker_ref, Wake, WakerRef};

impl Wake for Handle {
    fn wake(arc_self: Arc<Self>) {
        Wake::wake_by_ref(&arc_self);
    }

    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Set the woken flag to true
        arc_self.shared.woken.store(true, Ordering::Release);
    }
}

impl Handle {
    /// Creates a waker reference for the block_on future.
    /// Sets woken to true initially to ensure the future is polled at least once.
    pub(crate) fn waker_ref(me: &Arc<Self>) -> WakerRef<'_> {
        // Set woken to true when entering block_on, ensure outer future
        // be polled for the first time when entering loop
        me.shared.woken.store(true, Ordering::Release);
        waker_ref(me)
    }

    /// Reset woken to false and return original value.
    #[allow(dead_code)] // Reserved for future use
    pub(crate) fn reset_woken(&self) -> bool {
        self.shared.woken.swap(false, Ordering::AcqRel)
    }
}

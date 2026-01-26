//! Public API for the DPDK runtime scheduler.
//!
//! This module provides functions for querying worker state, spawning local
//! (`!Send`) tasks, and spawning tasks on specific workers in a DPDK runtime.
//!
//! # Worker Model
//!
//! The DPDK scheduler uses a thread-per-core model where each worker is pinned
//! to a specific CPU core. Workers never steal work from each other, which
//! enables support for `!Send` futures via [`spawn_local`].
//!
//! # Usage
//!
//! ```no_run
//! use tokio::runtime::dpdk;
//!
//! // Inside a DPDK runtime block_on:
//! # fn example() {
//! // Query current worker
//! let worker = dpdk::current_worker();
//!
//! // Spawn a !Send future on the current worker
//! let handle = dpdk::spawn_local(async {
//!     let rc = std::rc::Rc::new(42);
//!     *rc
//! });
//!
//! // Spawn a Send future on a specific worker
//! let workers = dpdk::workers();
//! let handle = dpdk::spawn_on(workers[0], async { 42 });
//! # }
//! ```

use crate::future::Future;
use crate::runtime::context;
use crate::runtime::scheduler;
use crate::runtime::task::{self, SpawnLocation};
use crate::task::JoinHandle;

use std::net::{Ipv4Addr, Ipv6Addr};

/// Identifies a specific DPDK worker thread.
///
/// Workers are numbered from `0` to `num_workers() - 1`. Each worker is
/// pinned to a dedicated CPU core and has its own DPDK network interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId {
    index: usize,
}

impl WorkerId {
    pub(crate) fn new(index: usize) -> Self {
        Self { index }
    }

    /// Returns the zero-based index of this worker.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkerId({})", self.index)
    }
}

// ============================================================================
// Query APIs
// ============================================================================

/// Returns the [`WorkerId`] of the current DPDK worker thread.
///
/// # Panics
///
/// Panics if not called from a DPDK worker thread.
#[track_caller]
pub fn current_worker() -> WorkerId {
    try_current_worker().expect("current_worker() must be called from a DPDK worker thread")
}

/// Returns the [`WorkerId`] of the current DPDK worker thread, or `None`
/// if not on a DPDK worker thread.
pub fn try_current_worker() -> Option<WorkerId> {
    crate::runtime::scheduler::dpdk::worker::current_worker_index().map(WorkerId::new)
}

/// Returns a list of all worker IDs in the current DPDK runtime.
///
/// This can be called from any thread that has a handle to the DPDK runtime
/// (including worker threads, the `block_on` thread, and spawned blocking tasks).
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context.
#[track_caller]
pub fn workers() -> Vec<WorkerId> {
    let n = num_workers();
    (0..n).map(WorkerId::new).collect()
}

/// Returns the number of workers in the current DPDK runtime.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context.
#[track_caller]
pub fn num_workers() -> usize {
    with_dpdk_handle(|h| h.num_workers())
}

/// Returns the IPv4 address configured on the specified worker's DPDK interface.
///
/// Returns `None` if the worker has no IPv4 address configured.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context.
#[track_caller]
pub fn worker_ipv4(id: WorkerId) -> Option<Ipv4Addr> {
    with_dpdk_handle(|h| h.worker_ipv4(id.index))
}

/// Returns the IPv6 address configured on the specified worker's DPDK interface.
///
/// Returns `None` if the worker has no IPv6 address configured.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context.
#[track_caller]
pub fn worker_ipv6(id: WorkerId) -> Option<Ipv6Addr> {
    with_dpdk_handle(|h| h.worker_ipv6(id.index))
}

/// Returns the IPv4 address configured on the current worker's DPDK interface.
///
/// Convenience function equivalent to `worker_ipv4(current_worker())`.
///
/// # Panics
///
/// Panics if not called from a DPDK worker thread.
#[track_caller]
pub fn current_ipv4() -> Option<Ipv4Addr> {
    worker_ipv4(current_worker())
}

/// Returns the IPv6 address configured on the current worker's DPDK interface.
///
/// Convenience function equivalent to `worker_ipv6(current_worker())`.
///
/// # Panics
///
/// Panics if not called from a DPDK worker thread.
#[track_caller]
pub fn current_ipv6() -> Option<Ipv6Addr> {
    worker_ipv6(current_worker())
}

// ============================================================================
// Spawn APIs
// ============================================================================

/// Spawns a `!Send` future on the current DPDK worker thread.
///
/// Unlike [`tokio::spawn`], the future does not need to be `Send` because
/// DPDK workers are pinned to CPU cores and never migrate tasks between
/// threads (no work stealing).
///
/// # Panics
///
/// Panics if not called from a DPDK worker thread.
///
/// # Examples
///
/// ```no_run
/// use tokio::runtime::dpdk;
/// use std::rc::Rc;
///
/// # fn example() {
/// // Rc is !Send, but works with spawn_local
/// let handle = dpdk::spawn_local(async {
///     let data = Rc::new(vec![1, 2, 3]);
///     data.iter().sum::<i32>()
/// });
/// # }
/// ```
///
/// [`tokio::spawn`]: crate::spawn
#[track_caller]
pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let id = task::Id::next();
    let spawned_at = SpawnLocation::capture();

    with_dpdk_handle_arc(|h| {
        scheduler::dpdk::Handle::spawn_local_impl(h, future, id, spawned_at)
            .expect("spawn_local must be called from a DPDK worker thread")
    })
}

/// Spawns a `Send` future on a specific DPDK worker.
///
/// The task will be placed in the target worker's queue and is guaranteed
/// to execute on that worker. If the caller is already on the target worker,
/// the task is scheduled directly to the local queue for lower latency.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context, or if the
/// worker ID is out of range.
///
/// # Examples
///
/// ```no_run
/// use tokio::runtime::dpdk;
///
/// # fn example() {
/// let workers = dpdk::workers();
/// let handle = dpdk::spawn_on(workers[0], async {
///     println!("running on worker 0");
///     42
/// });
/// # }
/// ```
#[track_caller]
pub fn spawn_on<F>(worker: WorkerId, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let id = task::Id::next();
    let spawned_at = SpawnLocation::capture();

    with_dpdk_handle_arc(|h| {
        scheduler::dpdk::Handle::spawn_on_worker(h, worker.index, future, id, spawned_at)
    })
}

/// Spawns a `!Send` future on a specific DPDK worker using a factory closure.
///
/// The factory closure (`create`) is `Send` and will be transferred to the
/// target worker, where it is called to produce the `!Send` future. This
/// pattern allows creating `!Send` state (e.g., `Rc`, thread-local handles)
/// directly on the target worker.
///
/// Returns a `JoinHandle` whose output type must be `Send` so the result
/// can be retrieved from any thread.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context, or if the
/// worker ID is out of range.
///
/// # Examples
///
/// ```no_run
/// use tokio::runtime::dpdk;
/// use std::rc::Rc;
///
/// # async fn example() {
/// let workers = dpdk::workers();
/// let handle = dpdk::spawn_local_on(workers[1], || async {
///     // This closure runs on worker 1, creating !Send state
///     let data = Rc::new(vec![1, 2, 3]);
///     data.iter().sum::<i32>()
/// });
///
/// // The result (i32) is Send and can be awaited from any thread
/// let sum = handle.await.unwrap();
/// # }
/// ```
#[track_caller]
pub fn spawn_local_on<F, Fut>(worker: WorkerId, create: F) -> JoinHandle<Fut::Output>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future + 'static,
    Fut::Output: Send + 'static,
{
    let spawned_at = SpawnLocation::capture();

    with_dpdk_handle_arc(|h| {
        let worker_index = worker.index;
        assert!(
            worker_index < h.num_workers(),
            "spawn_local_on: worker index {} is out of range (num_workers: {})",
            worker_index,
            h.num_workers()
        );

        // Create a oneshot channel to bridge the !Send result back
        let (tx, rx) = crate::sync::oneshot::channel::<Fut::Output>();

        // Build the factory closure that will run on the target worker.
        // It creates the !Send future and spawns it locally.
        let handle_clone = h.clone();
        let factory: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
            let future = create();
            let bridge = async move {
                let result = future.await;
                let _ = tx.send(result);
            };

            let id = task::Id::next();

            // We're now on the target worker - spawn locally with bind_local
            let _ = scheduler::dpdk::Handle::spawn_local_impl(
                &handle_clone,
                bridge,
                id,
                spawned_at,
            );
        });

        // Push the factory to the target worker's local_spawn_queue
        h.push_local_spawn_factory(worker_index, factory);

        // Spawn a Send bridge task that awaits the oneshot receiver
        let bridge_id = task::Id::next();
        scheduler::dpdk::Handle::spawn(
            h,
            async move {
                rx.await.expect("spawn_local_on: !Send task was dropped before completing")
            },
            bridge_id,
            spawned_at,
        )
    })
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Access the DPDK handle from the current runtime context.
///
/// Panics if not in a DPDK runtime.
#[track_caller]
fn with_dpdk_handle<R>(f: impl FnOnce(&scheduler::dpdk::Handle) -> R) -> R {
    context::with_current(|handle| match handle {
        scheduler::Handle::Dpdk(h) => f(h),
        _ => panic!("not running in a DPDK runtime"),
    })
    .expect("must be called from within a Tokio runtime")
}

/// Access the DPDK handle Arc from the current runtime context.
///
/// Panics if not in a DPDK runtime.
#[track_caller]
fn with_dpdk_handle_arc<R>(f: impl FnOnce(&crate::loom::sync::Arc<scheduler::dpdk::Handle>) -> R) -> R {
    context::with_current(|handle| match handle {
        scheduler::Handle::Dpdk(h) => f(h),
        _ => panic!("not running in a DPDK runtime"),
    })
    .expect("must be called from within a Tokio runtime")
}

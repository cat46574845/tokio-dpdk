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

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::io;

pub use crate::runtime::scheduler::dpdk::{
    RawTailHandle, RawTailInput, RawTailParseDisposition, RawTailParserBinding,
    RawTailUnregisterStatus,
};
use crate::runtime::scheduler::dpdk::DpdkRuntimeId;

/// Identifies a specific DPDK worker thread.
///
/// Workers are numbered from `0` to `num_workers() - 1`. Each worker is
/// pinned to a dedicated CPU core and has its own DPDK network interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId {
    runtime_id: DpdkRuntimeId,
    index: usize,
}

impl WorkerId {
    pub(crate) fn new(runtime_id: DpdkRuntimeId, index: usize) -> Self {
        Self { runtime_id, index }
    }

    /// Returns the zero-based index of this worker.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WorkerId(runtime={}, index={})",
            self.runtime_id.as_u64(),
            self.index
        )
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
    crate::runtime::scheduler::dpdk::worker::current_worker_identity()
        .map(|owner| WorkerId::new(owner.runtime_id, owner.worker_index))
}

/// Stop diverting a raw-tail flow and free any retained mbufs.
///
/// This may be called between `Runtime::block_on` calls on the worker's owner
/// OS thread. [`RawTailUnregisterStatus::OwnerReclaimed`] means runtime
/// shutdown already removed the binding, so parser-local state can be dropped.
#[track_caller]
pub fn unregister_raw_tail(handle: RawTailHandle) -> io::Result<RawTailUnregisterStatus> {
    let owner = handle.owner();
    if crate::runtime::scheduler::dpdk::worker::current_worker_identity() == Some(owner) {
        return crate::runtime::scheduler::dpdk::worker::with_current_driver(|driver| {
            driver.unregister_raw_tail(handle)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "DPDK driver unavailable"))?
        .map(|()| RawTailUnregisterStatus::Unregistered);
    }

    let Some(cleanup) = crate::runtime::scheduler::dpdk::driver_cleanup_for_identity(owner) else {
        return Ok(RawTailUnregisterStatus::OwnerReclaimed);
    };
    match cleanup.with_driver(|driver| driver.unregister_raw_tail(handle))? {
        Some(result) => result.map(|()| RawTailUnregisterStatus::Unregistered),
        None => Ok(RawTailUnregisterStatus::OwnerReclaimed),
    }
}

/// Reserve a raw-tail RSS hash before the TCP connection is established.
#[track_caller]
pub fn reserve_raw_tail(local: SocketAddr, remote: SocketAddr) -> io::Result<RawTailHandle> {
    crate::runtime::scheduler::dpdk::worker::with_current_driver(|driver| {
        driver.reserve_raw_tail(local, remote)
    })
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "DPDK driver unavailable"))?
}

/// Install a pinned synchronous parser and start capturing a reserved flow.
///
/// # Safety
///
/// The parser binding's context must remain pinned and valid until
/// [`unregister_raw_tail`] succeeds or returns
/// [`RawTailUnregisterStatus::OwnerReclaimed`]. Its callbacks must return
/// synchronously, and the caller must statically exclude DPDK driver re-entry.
/// `stream` and its registered smoltcp socket must also remain alive until that
/// same unregister boundary; dropping it earlier could recycle its index for a
/// different socket while the non-owning parser binding is still active.
#[track_caller]
pub unsafe fn activate_reserved_raw_tail_parser(
    stream: &crate::net::TcpDpdkStream,
    handle: RawTailHandle,
    parser: RawTailParserBinding,
) -> io::Result<()> {
    unsafe { stream.activate_reserved_raw_tail_parser(handle, parser) }
}

/// Poll the unique receiver for the current driver-side parser publication.
#[track_caller]
pub fn poll_raw_tail_publication_ready(
    handle: RawTailHandle,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<io::Result<()>> {
    crate::runtime::scheduler::dpdk::worker::with_current_driver(|driver| {
        driver.poll_raw_tail_publication_ready(handle, cx)
    })
    .unwrap_or_else(|| {
        std::task::Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Other,
            "DPDK driver unavailable",
        )))
    })
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
    with_dpdk_handle(|handle| {
        (0..handle.num_workers())
            .map(|index| WorkerId::new(handle.runtime_id(), index))
            .collect()
    })
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
    with_worker_handle(id, |h| h.worker_ipv4(id.index))
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
    with_worker_handle(id, |h| h.worker_ipv6(id.index))
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

/// Returns all IPv4 addresses configured on the specified worker's DPDK interface.
///
/// Returns an empty Vec if the worker has no IPv4 addresses configured.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context.
#[track_caller]
pub fn worker_ipv4s(id: WorkerId) -> Vec<Ipv4Addr> {
    with_worker_handle(id, |h| h.worker_ipv4s(id.index))
}

/// Returns all IPv6 addresses configured on the specified worker's DPDK interface.
///
/// Link-local addresses (fe80::) are excluded.
/// Returns an empty Vec if the worker has no IPv6 addresses configured.
///
/// # Panics
///
/// Panics if not called from within a DPDK runtime context.
#[track_caller]
pub fn worker_ipv6s(id: WorkerId) -> Vec<Ipv6Addr> {
    with_worker_handle(id, |h| h.worker_ipv6s(id.index))
}

/// Returns all IPv4 addresses configured on the current worker's DPDK interface.
///
/// Convenience function equivalent to `worker_ipv4s(current_worker())`.
///
/// # Panics
///
/// Panics if not called from a DPDK worker thread.
#[track_caller]
pub fn current_ipv4s() -> Vec<Ipv4Addr> {
    worker_ipv4s(current_worker())
}

/// Returns all IPv6 addresses configured on the current worker's DPDK interface.
///
/// Convenience function equivalent to `worker_ipv6s(current_worker())`.
///
/// # Panics
///
/// Panics if not called from a DPDK worker thread.
#[track_caller]
pub fn current_ipv6s() -> Vec<Ipv6Addr> {
    worker_ipv6s(current_worker())
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
        assert_worker_runtime(h, worker);
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
        assert_worker_runtime(h, worker);
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

#[track_caller]
fn with_worker_handle<R>(
    worker: WorkerId,
    f: impl FnOnce(&scheduler::dpdk::Handle) -> R,
) -> R {
    with_dpdk_handle(|handle| {
        assert_worker_runtime(handle, worker);
        f(handle)
    })
}

#[track_caller]
fn assert_worker_runtime(handle: &scheduler::dpdk::Handle, worker: WorkerId) {
    assert_eq!(
        worker.runtime_id,
        handle.runtime_id(),
        "WorkerId belongs to DPDK runtime {} but current runtime is {}",
        worker.runtime_id.as_u64(),
        handle.runtime_id().as_u64()
    );
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

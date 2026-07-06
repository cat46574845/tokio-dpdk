use std::sync::OnceLock;

pub(crate) type BeginHook = fn(span_id: u16, track_id: u32, aux: u64) -> u64;
pub(crate) type EndHook = fn(start_ns: u64, span_id: u16, track_id: u32, aux: u64);
pub(crate) type CompleteHook =
    fn(start_ns: u64, dur_ns: u64, span_id: u16, track_id: u32, aux: u64);
pub(crate) type CounterHook = fn(counter_id: u16, track_id: u32, value: u64);
pub(crate) type TaskQueueEnterHook = fn(queue_source: u8, queue_wait_ns: u64);
pub(crate) type TaskQueueExitHook = fn();

pub(crate) const TRACK_TOKIO_CURRENT: u32 = 40_000;
pub(crate) const TRACK_DPDK_BASE: u32 = 41_000;

pub(crate) const SPAN_TOKIO_CURRENT_RUN_TASK: u16 = 100;
pub(crate) const SPAN_TOKIO_CURRENT_PARK: u16 = 101;
pub(crate) const SPAN_TOKIO_CURRENT_PARK_YIELD: u16 = 102;
pub(crate) const SPAN_DPDK_DRIVER_POLL: u16 = 110;
pub(crate) const SPAN_DPDK_FLUSH_TX: u16 = 111;
pub(crate) const SPAN_DPDK_DRAIN_RX: u16 = 112;
pub(crate) const SPAN_DPDK_FLUSH_ACKS: u16 = 113;
pub(crate) const SPAN_DPDK_SMOLTCP_POLL: u16 = 115;
pub(crate) const SPAN_DPDK_RUN_TASK: u16 = 116;
pub(crate) const SPAN_DPDK_POLL_DRIVER_OUTER: u16 = 117;
pub(crate) const SPAN_DPDK_BLOCK_ON_POLL: u16 = 118;
pub(crate) const SPAN_DPDK_NEXT_TASK: u16 = 119;
pub(crate) const SPAN_DPDK_POLL_DRIVER_IDLE: u16 = 120;
pub(crate) const SPAN_DPDK_PROCESS_LOCAL_SPAWN: u16 = 121;
pub(crate) const SPAN_DPDK_MAINTENANCE: u16 = 122;
pub(crate) const SPAN_DPDK_SMOLTCP_INGRESS: u16 = 123;
pub(crate) const SPAN_DPDK_SMOLTCP_EGRESS: u16 = 124;
pub(crate) const COUNTER_DPDK_QUEUE_TOTAL_DEPTH: u16 = 130;
pub(crate) const COUNTER_DPDK_RUN_QUEUE_DEPTH: u16 = 131;
pub(crate) const COUNTER_DPDK_OVERFLOW_DEPTH: u16 = 132;
pub(crate) const COUNTER_DPDK_PER_WORKER_INJECT_DEPTH: u16 = 133;
pub(crate) const COUNTER_DPDK_GLOBAL_INJECT_DEPTH: u16 = 134;
pub(crate) const COUNTER_DPDK_LIFO_DEPTH: u16 = 135;

struct Hooks {
    begin: BeginHook,
    end: EndHook,
    complete: CompleteHook,
    counter: CounterHook,
}

struct TaskQueueHooks {
    enter: TaskQueueEnterHook,
    exit: TaskQueueExitHook,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();
static TASK_QUEUE_HOOKS: OnceLock<TaskQueueHooks> = OnceLock::new();
static SCHED_DETAIL: OnceLock<bool> = OnceLock::new();

pub(crate) const QUEUE_SOURCE_UNKNOWN: u8 = 0;
pub(crate) const QUEUE_SOURCE_LOCAL_RUN: u8 = 1;
pub(crate) const QUEUE_SOURCE_LOCAL_OVERFLOW: u8 = 2;
pub(crate) const QUEUE_SOURCE_LIFO: u8 = 3;
pub(crate) const QUEUE_SOURCE_PER_WORKER_INJECT: u8 = 4;
pub(crate) const QUEUE_SOURCE_SHARED_INJECT_OUTSIDE_WORKER: u8 = 6;
pub(crate) const QUEUE_SOURCE_SHARED_INJECT_DIFFERENT_RUNTIME: u8 = 7;
pub(crate) const QUEUE_SOURCE_PER_WORKER_INJECT_NO_CORE: u8 = 9;

const QUEUE_SOURCE_SHIFT: u64 = 56;
const QUEUE_WAIT_MASK: u64 = (1u64 << QUEUE_SOURCE_SHIFT) - 1;

#[inline(always)]
pub(crate) fn pack_task_aux(queue_source: u8, queue_wait_ns: u64) -> u64 {
    ((queue_source as u64) << QUEUE_SOURCE_SHIFT) | queue_wait_ns.min(QUEUE_WAIT_MASK)
}

#[inline(always)]
pub(crate) fn sched_detail_enabled() -> bool {
    *SCHED_DETAIL.get_or_init(|| {
        std::env::var_os("TOKIO_DPDK_MARKET_TRACE_SCHED_DETAIL")
            .map(|value| value != "0" && value != "false" && value != "FALSE")
            .unwrap_or(false)
    })
}

/// Registers the host application's trace recorder callbacks.
pub fn set_hooks(
    begin: fn(span_id: u16, track_id: u32, aux: u64) -> u64,
    end: fn(start_ns: u64, span_id: u16, track_id: u32, aux: u64),
    complete: fn(start_ns: u64, dur_ns: u64, span_id: u16, track_id: u32, aux: u64),
    counter: fn(counter_id: u16, track_id: u32, value: u64),
) -> Result<(), ()> {
    HOOKS
        .set(Hooks {
            begin,
            end,
            complete,
            counter,
        })
        .map_err(|_| ())
}

/// Registers hooks that expose the currently polled task queue source to the host trace recorder.
pub fn set_task_queue_hooks(
    enter: fn(queue_source: u8, queue_wait_ns: u64),
    exit: fn(),
) -> Result<(), ()> {
    TASK_QUEUE_HOOKS
        .set(TaskQueueHooks { enter, exit })
        .map_err(|_| ())
}

pub(crate) struct TaskQueueGuard {
    active: bool,
}

impl Drop for TaskQueueGuard {
    #[inline(always)]
    fn drop(&mut self) {
        if self.active {
            if let Some(hooks) = TASK_QUEUE_HOOKS.get() {
                (hooks.exit)();
            }
        }
    }
}

#[inline(always)]
pub(crate) fn enter_task_queue(queue_source: u8, queue_wait_ns: u64) -> TaskQueueGuard {
    let active = if let Some(hooks) = TASK_QUEUE_HOOKS.get() {
        (hooks.enter)(queue_source, queue_wait_ns);
        true
    } else {
        false
    };
    TaskQueueGuard { active }
}

#[inline(always)]
pub(crate) fn dpdk_track(worker_index: usize) -> u32 {
    TRACK_DPDK_BASE.saturating_add(worker_index.min(u32::MAX as usize) as u32)
}

#[inline(always)]
pub(crate) fn scope(span_id: u16, track_id: u32, aux: u64) -> Scope {
    let start_ns = HOOKS
        .get()
        .map(|hooks| (hooks.begin)(span_id, track_id, aux))
        .unwrap_or(0);
    Scope {
        start_ns,
        span_id,
        track_id,
        aux,
    }
}

#[inline(always)]
pub(crate) fn now_ns() -> u64 {
    HOOKS
        .get()
        .map(|hooks| (hooks.begin)(0, 0, 0))
        .unwrap_or(0)
}

#[inline(always)]
pub(crate) fn counter(counter_id: u16, track_id: u32, value: u64) {
    if let Some(hooks) = HOOKS.get() {
        (hooks.counter)(counter_id, track_id, value);
    }
}

#[inline(always)]
pub(crate) fn complete(start_ns: u64, dur_ns: u64, span_id: u16, track_id: u32, aux: u64) {
    if start_ns == 0 {
        return;
    }
    if let Some(hooks) = HOOKS.get() {
        (hooks.complete)(start_ns, dur_ns, span_id, track_id, aux);
    }
}

pub(crate) struct Scope {
    start_ns: u64,
    span_id: u16,
    track_id: u32,
    aux: u64,
}

impl Drop for Scope {
    #[inline(always)]
    fn drop(&mut self) {
        if self.start_ns == 0 {
            return;
        }
        if let Some(hooks) = HOOKS.get() {
            (hooks.end)(self.start_ns, self.span_id, self.track_id, self.aux);
        }
    }
}

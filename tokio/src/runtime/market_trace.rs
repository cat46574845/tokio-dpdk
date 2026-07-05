use std::sync::OnceLock;

pub(crate) type BeginHook = fn(span_id: u16, track_id: u32, aux: u64) -> u64;
pub(crate) type EndHook = fn(start_ns: u64, span_id: u16, track_id: u32, aux: u64);
pub(crate) type CompleteHook =
    fn(start_ns: u64, dur_ns: u64, span_id: u16, track_id: u32, aux: u64);
pub(crate) type CounterHook = fn(counter_id: u16, track_id: u32, value: u64);

pub(crate) const TRACK_TOKIO_CURRENT: u32 = 40_000;
pub(crate) const TRACK_DPDK_BASE: u32 = 41_000;

pub(crate) const SPAN_TOKIO_CURRENT_RUN_TASK: u16 = 100;
pub(crate) const SPAN_TOKIO_CURRENT_PARK: u16 = 101;
pub(crate) const SPAN_TOKIO_CURRENT_PARK_YIELD: u16 = 102;
pub(crate) const SPAN_DPDK_DRIVER_POLL: u16 = 110;
pub(crate) const SPAN_DPDK_FLUSH_TX: u16 = 111;
pub(crate) const SPAN_DPDK_FLUSH_ACKS: u16 = 113;
pub(crate) const SPAN_DPDK_YIELD_RAW_TAIL: u16 = 114;
pub(crate) const SPAN_DPDK_SMOLTCP_POLL: u16 = 115;
pub(crate) const SPAN_DPDK_RUN_TASK: u16 = 116;
pub(crate) const SPAN_DPDK_POLL_DRIVER_LOCK: u16 = 117;
pub(crate) const SPAN_DPDK_BLOCK_ON_POLL: u16 = 118;
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

static HOOKS: OnceLock<Hooks> = OnceLock::new();

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

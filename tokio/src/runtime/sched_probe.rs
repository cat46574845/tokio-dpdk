//! Runtime scheduler probe used by latency experiments.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

const HIST_LEN: usize = 65;

static START: OnceLock<Instant> = OnceLock::new();
static PROBE: OnceLock<Probe> = OnceLock::new();

/// Initializes the scheduler probe clock and storage.
pub fn init() {
    let _ = START.get_or_init(Instant::now);
    let _ = PROBE.get_or_init(Probe::new);
}

/// Writes the current scheduler probe snapshot as JSON.
pub fn write_json(path: impl AsRef<Path>) -> io::Result<()> {
    init();
    let mut file = File::create(path)?;
    write_snapshot(&mut file)
}

pub(crate) fn now_ns() -> u64 {
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn record_current_local_schedule(depth: usize) {
    let probe = probe();
    probe.current_local_schedules.fetch_add(1, Ordering::Relaxed);
    probe.current_local_depth.record(depth as u64);
}

pub(crate) fn record_current_remote_schedule(inject_depth: usize) {
    let probe = probe();
    probe.current_remote_schedules.fetch_add(1, Ordering::Relaxed);
    probe.current_inject_depth.record(inject_depth as u64);
}

pub(crate) fn record_current_task_start(wait_ns: u64, local_depth: usize, inject_depth: usize) {
    let probe = probe();
    probe.current_queue_wait_ns.record(wait_ns);
    probe.current_local_depth.record(local_depth as u64);
    probe.current_inject_depth.record(inject_depth as u64);
}

pub(crate) fn record_current_task_poll_ns(poll_ns: u64) {
    probe().current_task_poll_ns.record(poll_ns);
}

pub(crate) fn record_current_park_yield_ns(ns: u64) {
    probe().current_park_yield_ns.record(ns);
}

pub(crate) fn record_current_park_ns(ns: u64) {
    probe().current_park_ns.record(ns);
}

pub(crate) fn record_localset_local_schedule(depth: usize) {
    let probe = probe();
    probe.localset_local_schedules.fetch_add(1, Ordering::Relaxed);
    probe.localset_local_depth.record(depth as u64);
}

pub(crate) fn record_localset_remote_schedule(depth: usize) {
    let probe = probe();
    probe.localset_remote_schedules.fetch_add(1, Ordering::Relaxed);
    probe.localset_remote_depth.record(depth as u64);
}

pub(crate) fn record_localset_task_start(wait_ns: u64, local_depth: usize, remote_depth: usize) {
    let probe = probe();
    probe.localset_queue_wait_ns.record(wait_ns);
    probe.localset_local_depth.record(local_depth as u64);
    probe.localset_remote_depth.record(remote_depth as u64);
}

pub(crate) fn record_localset_task_poll_ns(poll_ns: u64) {
    probe().localset_task_poll_ns.record(poll_ns);
}

pub(crate) fn record_io_turn(ns: u64, ready_count: u64, event_count: u64, max_wait_ns: Option<u64>) {
    let probe = probe();
    probe.io_turn_ns.record(ns);
    probe.io_ready_events.record(ready_count);
    probe.io_total_events.record(event_count);
    match max_wait_ns {
        Some(max_wait_ns) => {
            probe.io_timed_turns.fetch_add(1, Ordering::Relaxed);
            probe.io_max_wait_ns.record(max_wait_ns);
        }
        None => {
            probe.io_blocking_turns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn probe() -> &'static Probe {
    PROBE.get_or_init(Probe::new)
}

fn write_snapshot(out: &mut dyn Write) -> io::Result<()> {
    let probe = probe();
    writeln!(out, "{{")?;
    writeln!(out, "  \"elapsed_ns\": {},", now_ns())?;
    write_counter(out, "current_local_schedules", &probe.current_local_schedules, true)?;
    write_counter(out, "current_remote_schedules", &probe.current_remote_schedules, true)?;
    write_dist(out, "current_queue_wait_ns", &probe.current_queue_wait_ns, true)?;
    write_dist(out, "current_task_poll_ns", &probe.current_task_poll_ns, true)?;
    write_dist(out, "current_local_depth", &probe.current_local_depth, true)?;
    write_dist(out, "current_inject_depth", &probe.current_inject_depth, true)?;
    write_dist(out, "current_park_yield_ns", &probe.current_park_yield_ns, true)?;
    write_dist(out, "current_park_ns", &probe.current_park_ns, true)?;
    write_counter(out, "localset_local_schedules", &probe.localset_local_schedules, true)?;
    write_counter(out, "localset_remote_schedules", &probe.localset_remote_schedules, true)?;
    write_dist(out, "localset_queue_wait_ns", &probe.localset_queue_wait_ns, true)?;
    write_dist(out, "localset_task_poll_ns", &probe.localset_task_poll_ns, true)?;
    write_dist(out, "localset_local_depth", &probe.localset_local_depth, true)?;
    write_dist(out, "localset_remote_depth", &probe.localset_remote_depth, true)?;
    write_dist(out, "io_turn_ns", &probe.io_turn_ns, true)?;
    write_dist(out, "io_ready_events", &probe.io_ready_events, true)?;
    write_dist(out, "io_total_events", &probe.io_total_events, true)?;
    write_counter(out, "io_timed_turns", &probe.io_timed_turns, true)?;
    write_counter(out, "io_blocking_turns", &probe.io_blocking_turns, true)?;
    write_dist(out, "io_max_wait_ns", &probe.io_max_wait_ns, false)?;
    writeln!(out, "}}")
}

fn write_counter(
    out: &mut dyn Write,
    name: &str,
    counter: &AtomicU64,
    comma: bool,
) -> io::Result<()> {
    let suffix = if comma { "," } else { "" };
    writeln!(
        out,
        "  \"{}\": {}{}",
        name,
        counter.load(Ordering::Relaxed),
        suffix
    )
}

fn write_dist(out: &mut dyn Write, name: &str, dist: &Dist, comma: bool) -> io::Result<()> {
    let suffix = if comma { "," } else { "" };
    writeln!(out, "  \"{}\": {{", name)?;
    writeln!(out, "    \"count\": {},", dist.count.load(Ordering::Relaxed))?;
    writeln!(out, "    \"total\": {},", dist.total.load(Ordering::Relaxed))?;
    writeln!(out, "    \"max\": {},", dist.max.load(Ordering::Relaxed))?;
    writeln!(out, "    \"hist_log2\": [")?;
    for (idx, bucket) in dist.hist.iter().enumerate() {
        let bucket_suffix = if idx + 1 == HIST_LEN { "" } else { "," };
        writeln!(
            out,
            "      {}{}",
            bucket.load(Ordering::Relaxed),
            bucket_suffix
        )?;
    }
    writeln!(out, "    ]")?;
    writeln!(out, "  }}{}", suffix)
}

struct Probe {
    current_local_schedules: AtomicU64,
    current_remote_schedules: AtomicU64,
    current_queue_wait_ns: Dist,
    current_task_poll_ns: Dist,
    current_local_depth: Dist,
    current_inject_depth: Dist,
    current_park_yield_ns: Dist,
    current_park_ns: Dist,
    localset_local_schedules: AtomicU64,
    localset_remote_schedules: AtomicU64,
    localset_queue_wait_ns: Dist,
    localset_task_poll_ns: Dist,
    localset_local_depth: Dist,
    localset_remote_depth: Dist,
    io_turn_ns: Dist,
    io_ready_events: Dist,
    io_total_events: Dist,
    io_timed_turns: AtomicU64,
    io_blocking_turns: AtomicU64,
    io_max_wait_ns: Dist,
}

impl Probe {
    fn new() -> Self {
        Self {
            current_local_schedules: AtomicU64::new(0),
            current_remote_schedules: AtomicU64::new(0),
            current_queue_wait_ns: Dist::new(),
            current_task_poll_ns: Dist::new(),
            current_local_depth: Dist::new(),
            current_inject_depth: Dist::new(),
            current_park_yield_ns: Dist::new(),
            current_park_ns: Dist::new(),
            localset_local_schedules: AtomicU64::new(0),
            localset_remote_schedules: AtomicU64::new(0),
            localset_queue_wait_ns: Dist::new(),
            localset_task_poll_ns: Dist::new(),
            localset_local_depth: Dist::new(),
            localset_remote_depth: Dist::new(),
            io_turn_ns: Dist::new(),
            io_ready_events: Dist::new(),
            io_total_events: Dist::new(),
            io_timed_turns: AtomicU64::new(0),
            io_blocking_turns: AtomicU64::new(0),
            io_max_wait_ns: Dist::new(),
        }
    }
}

struct Dist {
    count: AtomicU64,
    total: AtomicU64,
    max: AtomicU64,
    hist: [AtomicU64; HIST_LEN],
}

impl Dist {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total: AtomicU64::new(0),
            max: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn record(&self, value: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(value, Ordering::Relaxed);
        self.record_max(value);
        self.hist[bucket(value)].fetch_add(1, Ordering::Relaxed);
    }

    fn record_max(&self, value: u64) {
        let mut old = self.max.load(Ordering::Relaxed);
        while value > old {
            match self
                .max
                .compare_exchange_weak(old, value, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(next) => old = next,
            }
        }
    }
}

fn bucket(value: u64) -> usize {
    if value == 0 {
        0
    } else {
        (u64::BITS - value.leading_zeros()) as usize
    }
}

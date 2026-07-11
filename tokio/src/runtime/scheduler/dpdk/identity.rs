use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DPDK_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DpdkRuntimeId(u64);

impl DpdkRuntimeId {
    pub(crate) fn allocate() -> io::Result<Self> {
        NEXT_DPDK_RUNTIME_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(Self)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "DPDK runtime id space exhausted; ids are never reused within a process",
                )
            })
    }

    pub(crate) const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WorkerIdentity {
    pub(crate) runtime_id: DpdkRuntimeId,
    pub(crate) worker_index: usize,
}

impl WorkerIdentity {
    pub(crate) const fn new(runtime_id: DpdkRuntimeId, worker_index: usize) -> Self {
        Self {
            runtime_id,
            worker_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpdk_runtime_ids_are_distinct_and_monotonic() {
        let first = DpdkRuntimeId::allocate().expect("first runtime id must allocate");
        let second = DpdkRuntimeId::allocate().expect("second runtime id must allocate");
        assert!(second.as_u64() > first.as_u64());
        assert_ne!(first, second);
    }
}

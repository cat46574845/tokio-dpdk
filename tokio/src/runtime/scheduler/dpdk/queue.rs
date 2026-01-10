//! Local run-queue for the DPDK scheduler.
//!
//! This is adapted from `multi_thread/queue.rs` with work-stealing (Steal) removed.
//! DPDK workers do not steal from each other to avoid cross-core cache contention.

use crate::loom::cell::UnsafeCell;
use crate::loom::sync::Arc;
use crate::runtime::task;

use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};

// Use wider integers when possible to increase ABA resilience.
cfg_has_atomic_u64! {
    type UnsignedShort = u32;
    type UnsignedLong = u64;
    type AtomicUnsignedShort = crate::loom::sync::atomic::AtomicU32;
    type AtomicUnsignedLong = crate::loom::sync::atomic::AtomicU64;
}
cfg_not_has_atomic_u64! {
    type UnsignedShort = u16;
    type UnsignedLong = u32;
    type AtomicUnsignedShort = crate::loom::sync::atomic::AtomicU16;
    type AtomicUnsignedLong = crate::loom::sync::atomic::AtomicU32;
}

/// Producer handle. May only be used from a single thread.
/// Unlike multi_thread, there is no Steal handle since DPDK doesn't use work-stealing.
pub(crate) struct Local<T: 'static> {
    inner: Arc<Inner<T>>,
}

pub(crate) struct Inner<T: 'static> {
    /// Queue head pointer.
    ///
    /// Contains two `UnsignedShort` values packed together for ABA resilience.
    /// In DPDK mode, the "steal" half is unused but kept for code compatibility.
    head: AtomicUnsignedLong,

    /// Queue tail pointer. Only updated by producer thread.
    tail: AtomicUnsignedShort,

    /// Ring buffer of task slots.
    buffer: Box<[UnsafeCell<MaybeUninit<task::Notified<T>>>; LOCAL_QUEUE_CAPACITY]>,
}

unsafe impl<T> Send for Inner<T> {}
unsafe impl<T> Sync for Inner<T> {}

#[cfg(not(loom))]
const LOCAL_QUEUE_CAPACITY: usize = 256;

#[cfg(loom)]
const LOCAL_QUEUE_CAPACITY: usize = 4;

const MASK: usize = LOCAL_QUEUE_CAPACITY - 1;

// Helper to construct fixed-size array from Vec
fn make_fixed_size<T>(buffer: Box<[T]>) -> Box<[T; LOCAL_QUEUE_CAPACITY]> {
    assert_eq!(buffer.len(), LOCAL_QUEUE_CAPACITY);
    // Safety: We verified the length matches.
    unsafe { Box::from_raw(Box::into_raw(buffer).cast()) }
}

/// Create a new local run-queue.
///
/// Unlike multi_thread which returns `(Steal<T>, Local<T>)`, DPDK only returns `Local<T>`
/// since work-stealing is not supported.
pub(crate) fn local<T: 'static>() -> Local<T> {
    let mut buffer = Vec::with_capacity(LOCAL_QUEUE_CAPACITY);

    for _ in 0..LOCAL_QUEUE_CAPACITY {
        buffer.push(UnsafeCell::new(MaybeUninit::uninit()));
    }

    let inner = Arc::new(Inner {
        head: AtomicUnsignedLong::new(0),
        tail: AtomicUnsignedShort::new(0),
        buffer: make_fixed_size(buffer.into_boxed_slice()),
    });

    Local { inner }
}

impl<T> Local<T> {
    /// Returns the number of entries in the queue.
    pub(crate) fn len(&self) -> usize {
        let (_, head) = unpack(self.inner.head.load(Acquire));
        // Safety: this is the **only** thread that updates this cell.
        let tail = unsafe { self.inner.tail.unsync_load() };
        len(head, tail)
    }

    /// How many tasks can be pushed into the queue.
    pub(crate) fn remaining_slots(&self) -> usize {
        let (steal, _) = unpack(self.inner.head.load(Acquire));
        // Safety: this is the **only** thread that updates this cell.
        let tail = unsafe { self.inner.tail.unsync_load() };

        LOCAL_QUEUE_CAPACITY - len(steal, tail)
    }

    /// Returns the maximum capacity of the queue.
    pub(crate) fn max_capacity(&self) -> usize {
        LOCAL_QUEUE_CAPACITY
    }

    /// Returns true if there are any entries in the queue.
    pub(crate) fn has_tasks(&self) -> bool {
        self.len() != 0
    }

    /// Pushes a batch of tasks to the back of the queue.
    ///
    /// # Panics
    ///
    /// Panics if there is not enough capacity.
    pub(crate) fn push_back(&mut self, tasks: impl ExactSizeIterator<Item = task::Notified<T>>) {
        let len = tasks.len();
        assert!(len <= LOCAL_QUEUE_CAPACITY);

        if len == 0 {
            return;
        }

        let head = self.inner.head.load(Acquire);
        let (steal, _) = unpack(head);

        // Safety: this is the **only** thread that updates this cell.
        let mut tail = unsafe { self.inner.tail.unsync_load() };

        if tail.wrapping_sub(steal) <= (LOCAL_QUEUE_CAPACITY - len) as UnsignedShort {
            // There is capacity
        } else {
            panic!("queue full");
        }

        for task in tasks {
            let idx = tail as usize & MASK;

            self.inner.buffer[idx].with_mut(|ptr| {
                // Safety: There is only one producer and we verified capacity.
                unsafe {
                    ptr::write((*ptr).as_mut_ptr(), task);
                }
            });

            tail = tail.wrapping_add(1);
        }

        self.inner.tail.store(tail, Release);
    }

    /// Pops a task from the local queue.
    pub(crate) fn pop(&mut self) -> Option<task::Notified<T>> {
        let mut head = self.inner.head.load(Acquire);

        let idx = loop {
            let (steal, real) = unpack(head);

            // Safety: this is the **only** thread that updates this cell.
            let tail = unsafe { self.inner.tail.unsync_load() };

            if real == tail {
                // Queue is empty
                return None;
            }

            let next_real = real.wrapping_add(1);

            // Update both steal and real since there's no concurrent stealer in DPDK
            let next = if steal == real {
                pack(next_real, next_real)
            } else {
                // This shouldn't happen in DPDK, but handle it safely
                assert_ne!(steal, next_real);
                pack(steal, next_real)
            };

            let res = self
                .inner
                .head
                .compare_exchange(head, next, AcqRel, Acquire);

            match res {
                Ok(_) => break real as usize & MASK,
                Err(actual) => head = actual,
            }
        };

        Some(self.inner.buffer[idx].with(|ptr| unsafe { ptr::read(ptr).assume_init() }))
    }
}

impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert!(self.pop().is_none(), "queue not empty");
        }
    }
}

/// Calculate the length of the queue using head and tail.
fn len(head: UnsignedShort, tail: UnsignedShort) -> usize {
    tail.wrapping_sub(head) as usize
}

/// Split the head value into the real head and the steal index.
fn unpack(n: UnsignedLong) -> (UnsignedShort, UnsignedShort) {
    let real = n & UnsignedShort::MAX as UnsignedLong;
    let steal = n >> (mem::size_of::<UnsignedShort>() * 8);

    (steal as UnsignedShort, real as UnsignedShort)
}

/// Join the two head values.
fn pack(steal: UnsignedShort, real: UnsignedShort) -> UnsignedLong {
    (real as UnsignedLong) | ((steal as UnsignedLong) << (mem::size_of::<UnsignedShort>() * 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_queue_capacity() {
        assert!(LOCAL_QUEUE_CAPACITY - 1 <= u8::MAX as usize);
    }
}

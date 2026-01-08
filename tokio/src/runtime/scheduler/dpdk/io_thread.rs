//! Dedicated I/O thread for the DPDK scheduler.
//!
//! This thread handles:
//! - Standard I/O operations (files, signals) via mio
//! - Timer processing via the Traditional timer wheel
//!
//! Unlike DPDK workers which busy-poll, this thread parks on the driver
//! when there are no events to process.

use crate::loom::sync::atomic::{AtomicBool, Ordering};
use crate::loom::sync::Arc;
use crate::runtime::driver::{self, Driver};

use super::Handle;

/// Dedicated I/O thread that handles standard I/O and timers.
///
/// DPDK workers are busy-polling the network; this thread handles
/// everything else (filesystem, signals, timers).
pub(crate) struct IoThread {
    /// The tokio I/O driver (mio + timer wheel).
    /// This thread has exclusive ownership.
    driver: Driver,

    /// Driver handle for waking up the thread.
    driver_handle: driver::Handle,

    /// Reference to the scheduler handle.
    handle: Arc<Handle>,

    /// Shutdown signal.
    shutdown: AtomicBool,
}

impl IoThread {
    /// Creates a new I/O thread.
    pub(crate) fn new(driver: Driver, driver_handle: driver::Handle, handle: Arc<Handle>) -> Self {
        IoThread {
            driver,
            driver_handle,
            handle,
            shutdown: AtomicBool::new(false),
        }
    }

    /// Runs the I/O thread main loop.
    ///
    /// This loop:
    /// 1. Checks for shutdown
    /// 2. Calls driver.park() to wait for I/O or timer events
    /// 3. The driver internally wakes tasks via their Wakers
    /// 4. Woken tasks are scheduled to the inject queue
    /// 5. DPDK workers pick them up from the inject queue
    pub(crate) fn run(&mut self) {
        loop {
            // Check shutdown flag
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }

            // Park on the driver - this blocks until:
            // - An I/O event occurs (file ready, signal received)
            // - A timer expires
            // - The driver is unparked via driver_handle.unpark()
            //
            // The driver handles:
            // - Processing mio events and waking I/O waiters
            // - Processing timer wheel and waking expired timers
            self.driver.park(&self.driver_handle);
        }

        // Shutdown: park with timeout to process remaining events
        self.driver.shutdown(&self.driver_handle);
    }

    /// Signals the I/O thread to shut down.
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake up the driver in case it's parked
        self.driver_handle.unpark();
    }

    /// Spawns the I/O thread.
    pub(crate) fn spawn(mut self) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("dpdk-io".to_string())
            .spawn(move || {
                self.run();
            })
            .expect("failed to spawn I/O thread")
    }
}

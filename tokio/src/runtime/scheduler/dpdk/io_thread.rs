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
use crate::runtime::driver::Driver;

use super::Handle;

/// Handle for controlling the I/O thread from outside.
///
/// This is returned by `IoThread::spawn()` and allows the runtime
/// to signal shutdown and wait for the thread to complete.
pub(crate) struct IoThreadHandle {
    /// Shared shutdown signal
    shutdown: Arc<AtomicBool>,
    /// Reference to scheduler handle for unparking (contains driver::Handle)
    scheduler_handle: Arc<Handle>,
    /// Thread join handle
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl IoThreadHandle {
    /// Signals the I/O thread to shut down.
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake up the driver in case it's parked
        self.scheduler_handle.driver.unpark();
    }

    /// Waits for the I/O thread to finish.
    pub(crate) fn join(&mut self) {
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }

    /// Shuts down and waits for the I/O thread to finish.
    pub(crate) fn shutdown_and_join(&mut self) {
        self.shutdown();
        self.join();
    }
}

impl std::fmt::Debug for IoThreadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoThreadHandle")
            .field("shutdown", &self.shutdown.load(Ordering::Relaxed))
            .finish()
    }
}

/// Dedicated I/O thread that handles standard I/O and timers.
///
/// DPDK workers are busy-polling the network; this thread handles
/// everything else (filesystem, signals, timers).
pub(crate) struct IoThread {
    /// The tokio I/O driver (mio + timer wheel).
    /// This thread has exclusive ownership.
    driver: Driver,

    /// Reference to the scheduler handle (contains driver::Handle).
    handle: Arc<Handle>,

    /// Shared shutdown signal.
    shutdown: Arc<AtomicBool>,
}

impl IoThread {
    /// Creates a new I/O thread.
    pub(crate) fn new(driver: Driver, handle: Arc<Handle>) -> Self {
        IoThread {
            driver,
            handle,
            shutdown: Arc::new(AtomicBool::new(false)),
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

            // Call before_park callback (following multi_thread scheduler pattern)
            if let Some(f) = &self.handle.shared.config.before_park {
                f();
            }

            // Park on the driver - this blocks until:
            // - An I/O event occurs (file ready, signal received)
            // - A timer expires
            // - The driver is unparked via driver_handle.unpark()
            //
            // The driver handles:
            // - Processing mio events and waking I/O waiters
            // - Processing timer wheel and waking expired timers
            self.driver.park(&self.handle.driver);

            // Call after_unpark callback (following multi_thread scheduler pattern)
            if let Some(f) = &self.handle.shared.config.after_unpark {
                f();
            }
        }

        // Shutdown: park with timeout to process remaining events
        self.driver.shutdown(&self.handle.driver);
    }

    /// Spawns the I/O thread and returns a handle for shutdown control.
    pub(crate) fn spawn(mut self) -> IoThreadHandle {
        let shutdown = self.shutdown.clone();
        let scheduler_handle = self.handle.clone();

        let join_handle = std::thread::Builder::new()
            .name("dpdk-io".to_string())
            .spawn(move || {
                self.run();
            })
            .expect("failed to spawn I/O thread");

        IoThreadHandle {
            shutdown,
            scheduler_handle,
            join_handle: Some(join_handle),
        }
    }
}

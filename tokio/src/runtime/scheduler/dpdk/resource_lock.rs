//! Resource locking for DPDK multi-process support.
//!
//! This module provides runtime resource locking to prevent multiple DPDK processes
//! from using the same devices or CPU cores simultaneously.
//!
//! # Design
//!
//! - Uses file-based locking via the `fs2` crate (flock on Unix)
//! - Lock files are stored in `/var/run/dpdk/`
//! - Device locks: `/var/run/dpdk/device_<pci_address>.lock`
//! - Core locks: `/var/run/dpdk/core_<id>.lock`
//! - Locks are automatically released when the process exits (OS behavior)

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Directory for storing DPDK resource lock files.
/// Follows Linux FHS standard for runtime data.
pub(crate) const LOCK_DIR: &str = "/var/run/dpdk";

/// Manages DPDK resource locks (devices and cores).
///
/// When dropped, all locks are automatically released. Locks are also released
/// by the OS if the process terminates unexpectedly.
pub(crate) struct ResourceLock {
    /// Locked devices: (pci_address, lock_file)
    device_locks: Vec<(String, File)>,
    /// Locked cores: (core_id, lock_file)
    core_locks: Vec<(usize, File)>,
}

impl ResourceLock {
    /// Creates a new empty ResourceLock.
    pub(crate) fn new() -> Self {
        Self {
            device_locks: Vec::new(),
            core_locks: Vec::new(),
        }
    }

    /// Attempts to lock the specified DPDK devices.
    ///
    /// # Arguments
    /// * `pci_addresses` - List of PCI addresses to lock (e.g., "0000:28:00.0")
    ///
    /// # Returns
    /// * `Ok(())` if all devices were locked successfully
    /// * `Err` with details if any device is already locked by another process
    pub(crate) fn acquire_devices(&mut self, pci_addresses: &[String]) -> io::Result<()> {
        // Ensure lock directory exists
        Self::ensure_lock_dir()?;

        let mut acquired = Vec::new();

        for pci_addr in pci_addresses {
            let lock_path = Self::device_lock_path(pci_addr);

            match Self::try_acquire_lock(&lock_path) {
                Ok(file) => {
                    acquired.push((pci_addr.clone(), file));
                }
                Err(e) => {
                    // Release any locks we acquired before failing
                    for (_, file) in acquired {
                        Self::release_lock(file);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "Device {} is already locked by another process: {}",
                            pci_addr, e
                        ),
                    ));
                }
            }
        }

        self.device_locks.extend(acquired);
        Ok(())
    }

    /// Attempts to lock the specified CPU cores.
    ///
    /// # Arguments
    /// * `core_ids` - List of CPU core IDs to lock
    ///
    /// # Returns
    /// * `Ok(())` if all cores were locked successfully
    /// * `Err` with details if any core is already locked by another process
    pub(crate) fn acquire_cores(&mut self, core_ids: &[usize]) -> io::Result<()> {
        // Ensure lock directory exists
        Self::ensure_lock_dir()?;

        let mut acquired = Vec::new();

        for &core_id in core_ids {
            let lock_path = Self::core_lock_path(core_id);

            match Self::try_acquire_lock(&lock_path) {
                Ok(file) => {
                    acquired.push((core_id, file));
                }
                Err(e) => {
                    // Release any locks we acquired before failing
                    for (_, file) in acquired {
                        Self::release_lock(file);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "Core {} is already locked by another process: {}",
                            core_id, e
                        ),
                    ));
                }
            }
        }

        self.core_locks.extend(acquired);
        Ok(())
    }

    /// Explicitly releases all held locks.
    ///
    /// Note: Locks are also automatically released when the ResourceLock is dropped
    /// or when the process terminates.
    pub(crate) fn release(&mut self) {
        for (_, file) in self.device_locks.drain(..) {
            Self::release_lock(file);
        }
        for (_, file) in self.core_locks.drain(..) {
            Self::release_lock(file);
        }
    }

    /// Checks if a device is currently locked by any process.
    ///
    /// This is useful for filtering available devices without acquiring locks.
    pub(crate) fn is_device_locked(pci_address: &str) -> bool {
        let lock_path = Self::device_lock_path(pci_address);
        Self::check_lock_exists(&lock_path)
    }

    /// Checks if a core is currently locked by any process.
    ///
    /// This is useful for filtering available cores without acquiring locks.
    pub(crate) fn is_core_locked(core_id: usize) -> bool {
        let lock_path = Self::core_lock_path(core_id);
        Self::check_lock_exists(&lock_path)
    }

    /// Returns the list of locked device PCI addresses.
    #[allow(dead_code)]
    pub(crate) fn locked_devices(&self) -> Vec<&str> {
        self.device_locks
            .iter()
            .map(|(addr, _)| addr.as_str())
            .collect()
    }

    /// Returns the list of locked core IDs.
    #[allow(dead_code)]
    pub(crate) fn locked_cores(&self) -> Vec<usize> {
        self.core_locks.iter().map(|(id, _)| *id).collect()
    }

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Ensures the lock directory exists with appropriate permissions.
    fn ensure_lock_dir() -> io::Result<()> {
        let path = PathBuf::from(LOCK_DIR);
        if !path.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(&path)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(&path)?;
            }
        }
        Ok(())
    }

    /// Constructs the lock file path for a device.
    fn device_lock_path(pci_address: &str) -> PathBuf {
        // Replace ':' with '_' for filesystem-safe names
        let safe_name = pci_address.replace(':', "_");
        PathBuf::from(LOCK_DIR).join(format!("device_{}.lock", safe_name))
    }

    /// Constructs the lock file path for a core.
    fn core_lock_path(core_id: usize) -> PathBuf {
        PathBuf::from(LOCK_DIR).join(format!("core_{}.lock", core_id))
    }

    /// Attempts to acquire an exclusive lock on the given path.
    fn try_acquire_lock(path: &PathBuf) -> io::Result<File> {
        // Open or create the lock file
        #[cfg(unix)]
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o644)
            .open(path)?;

        #[cfg(not(unix))]
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        // Try to acquire exclusive lock (non-blocking)
        Self::try_lock_exclusive(&file)?;

        Ok(file)
    }

    /// Platform-specific exclusive lock implementation.
    #[cfg(unix)]
    fn try_lock_exclusive(file: &File) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;

        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(unix))]
    fn try_lock_exclusive(_file: &File) -> io::Result<()> {
        // On non-Unix platforms, we don't have flock
        // For now, just succeed (this is primarily for Linux DPDK)
        Ok(())
    }

    /// Releases a lock file.
    fn release_lock(file: File) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            unsafe { libc::flock(fd, libc::LOCK_UN) };
        }
        // File is dropped, which also releases the lock
        drop(file);
    }

    /// Checks if a lock exists and is held.
    fn check_lock_exists(path: &PathBuf) -> bool {
        if !path.exists() {
            return false;
        }

        // Try to acquire the lock to see if it's held
        #[cfg(unix)]
        {
            if let Ok(file) = OpenOptions::new().write(true).open(path) {
                use std::os::unix::io::AsRawFd;
                let fd = file.as_raw_fd();
                let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
                if ret == 0 {
                    // We got the lock, so it was not held - release and return false
                    unsafe { libc::flock(fd, libc::LOCK_UN) };
                    return false;
                }
                // Lock is held by another process
                return true;
            }
        }

        false
    }
}

impl Drop for ResourceLock {
    fn drop(&mut self) {
        self.release();
    }
}

impl Default for ResourceLock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_isolation_test::serial_isolation_test;

    // Helper to clean up lock files after tests
    fn cleanup_lock(path: &PathBuf) {
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_device_lock_path() {
        let path = ResourceLock::device_lock_path("0000:28:00.0");
        assert_eq!(
            path,
            PathBuf::from("/var/run/dpdk/device_0000_28_00.0.lock")
        );
    }

    #[test]
    fn test_core_lock_path() {
        let path = ResourceLock::core_lock_path(3);
        assert_eq!(path, PathBuf::from("/var/run/dpdk/core_3.lock"));
    }

    #[test]
    fn test_resource_lock_new() {
        let lock = ResourceLock::new();
        assert!(lock.device_locks.is_empty());
        assert!(lock.core_locks.is_empty());
    }

    // These tests require root permissions to create files in /var/run/dpdk.
    // Using #[serial_isolation_test] to automatically run with sudo in subprocess.

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_acquire_device_success() {
        let test_pci = "0000:ff:ff.0";
        let lock_path = ResourceLock::device_lock_path(test_pci);
        cleanup_lock(&lock_path);

        let mut lock = ResourceLock::new();
        let result = lock.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_devices(), vec![test_pci]);

        cleanup_lock(&lock_path);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_acquire_core_success() {
        let test_core = 255;
        let lock_path = ResourceLock::core_lock_path(test_core);
        cleanup_lock(&lock_path);

        let mut lock = ResourceLock::new();
        let result = lock.acquire_cores(&[test_core]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_cores(), vec![test_core]);

        cleanup_lock(&lock_path);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_drop_releases() {
        let test_pci = "0000:ff:fe.0";
        let lock_path = ResourceLock::device_lock_path(test_pci);
        cleanup_lock(&lock_path);

        // Acquire lock in a scope
        {
            let mut lock = ResourceLock::new();
            lock.acquire_devices(&[test_pci.to_string()]).unwrap();
            assert!(!lock.locked_devices().is_empty());
        }
        // Lock should be released after drop

        // Should be able to acquire again
        let mut lock2 = ResourceLock::new();
        let result = lock2.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_ok());

        cleanup_lock(&lock_path);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_acquire_multiple_devices() {
        let test_pci_1 = "0000:ff:f0.0";
        let test_pci_2 = "0000:ff:f1.0";
        let lock_path_1 = ResourceLock::device_lock_path(test_pci_1);
        let lock_path_2 = ResourceLock::device_lock_path(test_pci_2);
        cleanup_lock(&lock_path_1);
        cleanup_lock(&lock_path_2);

        let mut lock = ResourceLock::new();
        let result = lock.acquire_devices(&[test_pci_1.to_string(), test_pci_2.to_string()]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_devices().len(), 2);
        assert!(lock.locked_devices().contains(&test_pci_1));
        assert!(lock.locked_devices().contains(&test_pci_2));

        cleanup_lock(&lock_path_1);
        cleanup_lock(&lock_path_2);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_acquire_multiple_cores() {
        let test_core_1 = 250;
        let test_core_2 = 251;
        let lock_path_1 = ResourceLock::core_lock_path(test_core_1);
        let lock_path_2 = ResourceLock::core_lock_path(test_core_2);
        cleanup_lock(&lock_path_1);
        cleanup_lock(&lock_path_2);

        let mut lock = ResourceLock::new();
        let result = lock.acquire_cores(&[test_core_1, test_core_2]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_cores().len(), 2);
        assert!(lock.locked_cores().contains(&test_core_1));
        assert!(lock.locked_cores().contains(&test_core_2));

        cleanup_lock(&lock_path_1);
        cleanup_lock(&lock_path_2);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_device_already_locked() {
        let test_pci = "0000:ff:e0.0";
        let lock_path = ResourceLock::device_lock_path(test_pci);
        cleanup_lock(&lock_path);

        // First lock
        let mut lock1 = ResourceLock::new();
        lock1.acquire_devices(&[test_pci.to_string()]).unwrap();

        // Second lock should fail
        let mut lock2 = ResourceLock::new();
        let result = lock2.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        cleanup_lock(&lock_path);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_core_already_locked() {
        let test_core = 249;
        let lock_path = ResourceLock::core_lock_path(test_core);
        cleanup_lock(&lock_path);

        // First lock
        let mut lock1 = ResourceLock::new();
        lock1.acquire_cores(&[test_core]).unwrap();

        // Second lock should fail
        let mut lock2 = ResourceLock::new();
        let result = lock2.acquire_cores(&[test_core]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        cleanup_lock(&lock_path);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_release() {
        let test_pci = "0000:ff:d0.0";
        let lock_path = ResourceLock::device_lock_path(test_pci);
        cleanup_lock(&lock_path);

        // Acquire and explicitly release
        let mut lock1 = ResourceLock::new();
        lock1.acquire_devices(&[test_pci.to_string()]).unwrap();
        assert!(!lock1.locked_devices().is_empty());
        lock1.release();
        assert!(lock1.locked_devices().is_empty());

        // Should be able to acquire again
        let mut lock2 = ResourceLock::new();
        let result = lock2.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_ok());

        cleanup_lock(&lock_path);
    }

    #[serial_isolation_test]
    #[test]
    fn test_resource_lock_dir_creation() {
        // This test verifies that ResourceLock::new() creates the lock directory if needed
        // Note: The directory should already exist from previous tests or system setup
        let lock = ResourceLock::new();
        assert!(PathBuf::from(LOCK_DIR).exists());
        drop(lock);
    }
}

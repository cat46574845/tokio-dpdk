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
    /// Directory where lock files are stored.
    lock_dir: PathBuf,
    /// Locked devices: (pci_address, lock_file)
    device_locks: Vec<(String, File)>,
    /// Locked cores: (core_id, lock_file)
    core_locks: Vec<(usize, File)>,
}

impl ResourceLock {
    /// Creates a new empty ResourceLock using the default lock directory.
    pub(crate) fn new() -> Self {
        Self {
            lock_dir: PathBuf::from(LOCK_DIR),
            device_locks: Vec::new(),
            core_locks: Vec::new(),
        }
    }

    /// Creates a new empty ResourceLock with a custom lock directory.
    #[cfg(test)]
    fn with_lock_dir(dir: PathBuf) -> Self {
        Self {
            lock_dir: dir,
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
        self.ensure_lock_dir()?;

        let mut acquired = Vec::new();

        for pci_addr in pci_addresses {
            let lock_path = self.device_lock_path(pci_addr);

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
        self.ensure_lock_dir()?;

        let mut acquired = Vec::new();

        for &core_id in core_ids {
            let lock_path = self.core_lock_path(core_id);

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
    /// Uses the default lock directory.
    pub(crate) fn is_device_locked(pci_address: &str) -> bool {
        let safe_name = pci_address.replace(':', "_");
        let lock_path = PathBuf::from(LOCK_DIR).join(format!("device_{}.lock", safe_name));
        Self::check_lock_exists(&lock_path)
    }

    /// Checks if a core is currently locked by any process.
    ///
    /// This is useful for filtering available cores without acquiring locks.
    /// Uses the default lock directory.
    pub(crate) fn is_core_locked(core_id: usize) -> bool {
        let lock_path = PathBuf::from(LOCK_DIR).join(format!("core_{}.lock", core_id));
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
    fn ensure_lock_dir(&self) -> io::Result<()> {
        if !self.lock_dir.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(&self.lock_dir)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(&self.lock_dir)?;
            }
        }
        Ok(())
    }

    /// Constructs the lock file path for a device.
    fn device_lock_path(&self, pci_address: &str) -> PathBuf {
        // Replace ':' with '_' for filesystem-safe names
        let safe_name = pci_address.replace(':', "_");
        self.lock_dir.join(format!("device_{}.lock", safe_name))
    }

    /// Constructs the lock file path for a core.
    fn core_lock_path(&self, core_id: usize) -> PathBuf {
        self.lock_dir.join(format!("core_{}.lock", core_id))
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

// Static path helpers used by tests that verify path formatting (without needing an instance).
#[cfg(test)]
impl ResourceLock {
    fn static_device_lock_path(lock_dir: &str, pci_address: &str) -> PathBuf {
        let safe_name = pci_address.replace(':', "_");
        PathBuf::from(lock_dir).join(format!("device_{}.lock", safe_name))
    }

    fn static_core_lock_path(lock_dir: &str, core_id: usize) -> PathBuf {
        PathBuf::from(lock_dir).join(format!("core_{}.lock", core_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a unique temp directory for lock file tests.
    /// Returns (lock_dir_path, ResourceLock) — the directory is auto-cleaned on test exit.
    fn test_lock(test_name: &str) -> (PathBuf, ResourceLock) {
        let dir = std::env::temp_dir()
            .join("dpdk-resource-lock-test")
            .join(test_name);
        // Clean up any leftover from previous runs
        let _ = fs::remove_dir_all(&dir);
        let lock = ResourceLock::with_lock_dir(dir.clone());
        (dir, lock)
    }

    fn cleanup_dir(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_device_lock_path() {
        let path = ResourceLock::static_device_lock_path(LOCK_DIR, "0000:28:00.0");
        assert_eq!(
            path,
            PathBuf::from("/var/run/dpdk/device_0000_28_00.0.lock")
        );
    }

    #[test]
    fn test_core_lock_path() {
        let path = ResourceLock::static_core_lock_path(LOCK_DIR, 3);
        assert_eq!(path, PathBuf::from("/var/run/dpdk/core_3.lock"));
    }

    #[test]
    fn test_resource_lock_new() {
        let lock = ResourceLock::new();
        assert!(lock.device_locks.is_empty());
        assert!(lock.core_locks.is_empty());
    }

    #[test]
    fn test_resource_lock_acquire_device_success() {
        let (dir, mut lock) = test_lock("acquire_device");

        let test_pci = "0000:ff:ff.0";
        let result = lock.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_devices(), vec![test_pci]);

        drop(lock);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_acquire_core_success() {
        let (dir, mut lock) = test_lock("acquire_core");

        let test_core = 255;
        let result = lock.acquire_cores(&[test_core]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_cores(), vec![test_core]);

        drop(lock);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_drop_releases() {
        let (dir, _) = test_lock("drop_releases");

        let test_pci = "0000:ff:fe.0";

        // Acquire lock in a scope
        {
            let mut lock = ResourceLock::with_lock_dir(dir.clone());
            lock.acquire_devices(&[test_pci.to_string()]).unwrap();
            assert!(!lock.locked_devices().is_empty());
        }
        // Lock should be released after drop

        // Should be able to acquire again
        let mut lock2 = ResourceLock::with_lock_dir(dir.clone());
        let result = lock2.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_ok());

        drop(lock2);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_acquire_multiple_devices() {
        let (dir, mut lock) = test_lock("acquire_multi_dev");

        let test_pci_1 = "0000:ff:f0.0";
        let test_pci_2 = "0000:ff:f1.0";

        let result = lock.acquire_devices(&[test_pci_1.to_string(), test_pci_2.to_string()]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_devices().len(), 2);
        assert!(lock.locked_devices().contains(&test_pci_1));
        assert!(lock.locked_devices().contains(&test_pci_2));

        drop(lock);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_acquire_multiple_cores() {
        let (dir, mut lock) = test_lock("acquire_multi_core");

        let test_core_1 = 250;
        let test_core_2 = 251;

        let result = lock.acquire_cores(&[test_core_1, test_core_2]);
        assert!(result.is_ok());
        assert_eq!(lock.locked_cores().len(), 2);
        assert!(lock.locked_cores().contains(&test_core_1));
        assert!(lock.locked_cores().contains(&test_core_2));

        drop(lock);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_device_already_locked() {
        let (dir, mut lock1) = test_lock("dev_already_locked");

        let test_pci = "0000:ff:e0.0";
        lock1.acquire_devices(&[test_pci.to_string()]).unwrap();

        // Second lock on the same dir should fail
        let mut lock2 = ResourceLock::with_lock_dir(dir.clone());
        let result = lock2.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        drop(lock1);
        drop(lock2);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_core_already_locked() {
        let (dir, mut lock1) = test_lock("core_already_locked");

        let test_core = 249;
        lock1.acquire_cores(&[test_core]).unwrap();

        // Second lock on the same dir should fail
        let mut lock2 = ResourceLock::with_lock_dir(dir.clone());
        let result = lock2.acquire_cores(&[test_core]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        drop(lock1);
        drop(lock2);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_release() {
        let (dir, mut lock1) = test_lock("release");

        let test_pci = "0000:ff:d0.0";
        lock1.acquire_devices(&[test_pci.to_string()]).unwrap();
        assert!(!lock1.locked_devices().is_empty());
        lock1.release();
        assert!(lock1.locked_devices().is_empty());

        // Should be able to acquire again
        let mut lock2 = ResourceLock::with_lock_dir(dir.clone());
        let result = lock2.acquire_devices(&[test_pci.to_string()]);
        assert!(result.is_ok());

        drop(lock1);
        drop(lock2);
        cleanup_dir(&dir);
    }

    #[test]
    fn test_resource_lock_dir_creation() {
        let dir = std::env::temp_dir()
            .join("dpdk-resource-lock-test")
            .join("dir_creation");
        let _ = fs::remove_dir_all(&dir);

        // The directory should not exist yet
        assert!(!dir.exists());

        let mut lock = ResourceLock::with_lock_dir(dir.clone());
        // Acquiring a lock triggers dir creation
        let _ = lock.acquire_devices(&["0000:00:00.0".to_string()]);
        assert!(dir.exists());

        drop(lock);
        cleanup_dir(&dir);
    }
}

//! Multi-process and multi-queue DPDK tests.
//!
//! These tests verify:
//! 1. Multi-process isolation - different processes can use different devices
//! 2. Multi-queue mode - multiple workers sharing a single device
//! 3. End-to-end traffic routing with real network operations
//!
//! **IMPORTANT**: All tests use REAL DPDK hardware. No mocked/simulated data.
//!
//! **TEST ORGANIZATION**:
//! - DPDK EAL can only be initialized ONCE per process
//! - All tests use #[serial_isolation_test] for automatic subprocess isolation
//! - Run with: SERIAL_ISOLATION_SUDO=1 cargo test --features full

#![cfg(feature = "full")]
#![cfg(not(miri))]
#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Runtime;

// =============================================================================
// Helper Functions - Using REAL configuration
// =============================================================================

/// Get first DPDK device from env.json.
fn get_first_dpdk_device() -> String {
    const CONFIG_PATHS: &[&str] = &[
        "/etc/dpdk/env.json",
        "./config/dpdk-env.json",
        "./dpdk-env.json",
    ];

    for path in CONFIG_PATHS {
        if let Ok(content) = std::fs::read_to_string(path) {
            let devices = extract_dpdk_devices_from_json(&content);
            if let Some(first) = devices.into_iter().next() {
                return first;
            }
        }
    }

    panic!(
        "No DPDK device found in env.json. Searched: {:?}",
        CONFIG_PATHS
    )
}

/// Get all DPDK devices from env.json.
fn get_all_dpdk_devices() -> Vec<String> {
    const CONFIG_PATHS: &[&str] = &[
        "/etc/dpdk/env.json",
        "./config/dpdk-env.json",
        "./dpdk-env.json",
    ];

    for path in CONFIG_PATHS {
        if let Ok(content) = std::fs::read_to_string(path) {
            let devices = extract_dpdk_devices_from_json(&content);
            if !devices.is_empty() {
                return devices;
            }
        }
    }

    panic!(
        "No DPDK devices found in env.json. Searched: {:?}",
        CONFIG_PATHS
    )
}

/// Parse DPDK devices from env.json content using serde_json
fn extract_dpdk_devices_from_json(content: &str) -> Vec<String> {
    let json: serde_json::Value = serde_json::from_str(content).expect("Failed to parse env.json");

    let mut devices = Vec::new();

    if let Some(arr) = json.get("devices").and_then(|d| d.as_array()) {
        for device in arr {
            let role = device.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "dpdk" {
                if let Some(pci) = device.get("pci_address").and_then(|p| p.as_str()) {
                    devices.push(pci.to_string());
                }
            }
        }
    }

    devices
}

/// Create DPDK runtime with a specific real device.
/// Uses worker_threads(1) because a single device on AWS ENA cannot support
/// multi-queue mode (no rte_flow support).
fn dpdk_rt_with_device(pci_address: &str) -> Runtime {
    tokio::runtime::Builder::new_dpdk()
        .dpdk_pci_addresses(&[pci_address])
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            panic!(
                "DPDK runtime creation failed for device '{}': {:?}",
                pci_address, e
            )
        })
}

// =============================================================================
// Independent Tests - Each spawns its own DPDK runtime
// =============================================================================

/// Test: When we hold the device, the lock file exists and is locked.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_same_device_lock_held() {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let device = get_first_dpdk_device();
    let _rt = dpdk_rt_with_device(&device);

    let safe_pci = device.replace(':', "_");
    let lock_path = format!("/var/run/dpdk/device_{}.lock", safe_pci);

    assert!(
        std::path::Path::new(&lock_path).exists(),
        "Lock file should exist at {}",
        lock_path
    );

    let file = OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .expect("Should open lock file");

    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    assert_eq!(ret, -1, "Should fail to acquire lock on device in use");

    let err = std::io::Error::last_os_error();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::WouldBlock,
        "Error should be WouldBlock"
    );
}

/// Test: Other devices should still be lockable.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_different_device_lock_available() {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let all_devices = get_all_dpdk_devices();
    assert!(all_devices.len() >= 2, "need 2+ DPDK devices, found {}", all_devices.len());

    let _rt = dpdk_rt_with_device(&all_devices[0]);

    let device2 = &all_devices[1];
    let safe_pci = device2.replace(':', "_");
    let lock_path = format!("/var/run/dpdk/device_{}.lock", safe_pci);

    let _ = std::fs::remove_file(&lock_path);

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o644)
        .open(&lock_path)
        .expect("Should create lock file");

    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    assert_eq!(ret, 0, "Should acquire lock on different device");

    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);
    let _ = std::fs::remove_file(&lock_path);
}

/// Test: Single queue mode with REAL HTTP traffic.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_single_queue_real_traffic() {
    let device = get_first_dpdk_device();
    let rt = dpdk_rt_with_device(&device);

    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpDpdkStream;

        let mut stream = TcpDpdkStream::connect("1.1.1.1:80")
            .await
            .expect("Should connect to Cloudflare");

        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n")
            .await
            .expect("Should send request");

        let mut buf = [0u8; 512];
        let n = stream.read(&mut buf).await.expect("Should read response");

        assert!(n > 0, "Should receive data");
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(response.contains("HTTP"), "Should be HTTP response");
    });
}

/// Test: Multiple concurrent tasks with REAL network operations.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_multi_task_real_traffic() {
    let device = get_first_dpdk_device();
    let rt = dpdk_rt_with_device(&device);

    rt.block_on(async {
        let mut handles = Vec::new();

        for i in 0..4 {
            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpDpdkStream;

                if let Ok(mut stream) = TcpDpdkStream::connect("1.1.1.1:80").await {
                    let request = format!("GET /{} HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n", i);
                    stream.write_all(request.as_bytes()).await.ok();
                    let mut buf = [0u8; 64];
                    stream.read(&mut buf).await.ok();
                    true
                } else {
                    false
                }
            }));
        }

        let mut success = 0;
        for handle in handles {
            if handle.await.unwrap_or(false) {
                success += 1;
            }
        }

        assert!(success >= 2, "At least 2 of 4 connections should succeed");
    });
}

/// Test: Real HTTP traffic through DPDK stack.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_e2e_real_network_http() {
    let device = get_first_dpdk_device();
    let rt = dpdk_rt_with_device(&device);

    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpDpdkStream;

        for _ in 0..3 {
            let mut stream = TcpDpdkStream::connect("1.1.1.1:80")
                .await
                .expect("Should connect");

            stream
                .write_all(b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n")
                .await
                .expect("Should write");

            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).await.expect("Should read");
            assert!(n > 0);
        }
    });
}

/// Test: Multiple concurrent connections to external service.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_e2e_multiple_connections() {
    let device = get_first_dpdk_device();
    let rt = dpdk_rt_with_device(&device);

    rt.block_on(async {
        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..10 {
            let counter = success_count.clone();
            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpDpdkStream;

                if let Ok(mut stream) = TcpDpdkStream::connect("1.1.1.1:80").await {
                    if stream
                        .write_all(b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n")
                        .await
                        .is_ok()
                    {
                        let mut buf = [0u8; 64];
                        if stream.read(&mut buf).await.is_ok() {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }

        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }

        let successes = success_count.load(Ordering::Relaxed);
        assert!(
            successes >= 5,
            "At least 5 of 10 should succeed, got {}",
            successes
        );
    });
}

/// Test: Concurrent workers handling traffic.
/// Uses the default runtime (all available devices) to get multiple workers.
/// Asserts that multiple workers are available — fails if not.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_e2e_concurrent_workers_traffic() {
    use tokio::runtime::dpdk;

    // Use default runtime to get multiple workers across all NICs
    let rt = tokio::runtime::Builder::new_dpdk()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("DPDK runtime creation failed");

    rt.block_on(async {
        let workers = dpdk::workers();
        assert!(
            workers.len() >= 2,
            "Test requires at least 2 workers, got {}. \
             Ensure env.json has enough DPDK devices with IPs and cores.",
            workers.len()
        );

        use std::collections::HashSet;

        let thread_ids = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let mut handles = Vec::new();

        // Distribute tasks across workers explicitly
        for i in 0..8 {
            let thread_ids = thread_ids.clone();
            let worker = workers[i % workers.len()];
            handles.push(dpdk::spawn_on(worker, async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpDpdkStream;

                let tid = std::thread::current().id();
                thread_ids.lock().unwrap().insert(format!("{:?}", tid));

                if let Ok(mut stream) = TcpDpdkStream::connect("1.1.1.1:80").await {
                    stream
                        .write_all(b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n")
                        .await
                        .ok();
                    let mut buf = [0u8; 64];
                    stream.read(&mut buf).await.ok();
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }

        let ids = thread_ids.lock().unwrap();
        assert!(
            ids.len() >= 2,
            "Tasks should have executed on at least 2 different workers, got {} thread IDs",
            ids.len()
        );
    });
}

/// Test: Traffic distribution metrics.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_e2e_traffic_distribution() {
    let device = get_first_dpdk_device();
    let rt = dpdk_rt_with_device(&device);

    rt.block_on(async {
        let rx_count = Arc::new(AtomicUsize::new(0));
        let tx_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..6 {
            let rx = rx_count.clone();
            let tx = tx_count.clone();

            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpDpdkStream;

                if let Ok(mut stream) = TcpDpdkStream::connect("1.1.1.1:80").await {
                    if stream
                        .write_all(b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n")
                        .await
                        .is_ok()
                    {
                        tx.fetch_add(1, Ordering::Relaxed);

                        let mut buf = [0u8; 128];
                        if let Ok(n) = stream.read(&mut buf).await {
                            if n > 0 {
                                rx.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }));
        }

        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }

        let tx_result = tx_count.load(Ordering::Relaxed);
        let rx_result = rx_count.load(Ordering::Relaxed);

        assert!(
            tx_result >= 3,
            "Should have at least 3 TX, got {}",
            tx_result
        );
        assert!(
            rx_result >= 3,
            "Should have at least 3 RX, got {}",
            rx_result
        );
    });
}

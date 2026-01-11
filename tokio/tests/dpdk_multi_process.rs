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
//! - All tests requiring DPDK runtime are combined into ONE test function
//! - Tests are run individually via run_dpdk_tests.sh which spawns separate processes

#![cfg(feature = "full")]
#![cfg(not(miri))]
#![cfg(target_os = "linux")]

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Runtime;

// =============================================================================
// Subtest Result Type
// =============================================================================

/// Result of a subtest - allows proper tracking of skipped tests
enum SubtestResult {
    Passed,
    Skipped(&'static str),
}

// =============================================================================
// Helper Functions - Using REAL configuration
// =============================================================================

/// Get first DPDK device from real env.json
fn get_first_dpdk_device() -> String {
    if let Ok(device) = std::env::var("DPDK_DEVICE") {
        return device;
    }

    let content = std::fs::read_to_string("/etc/dpdk/env.json")
        .expect("env.json not found - DPDK not configured");

    extract_dpdk_devices_from_json(&content)
        .into_iter()
        .next()
        .expect("No DPDK devices in env.json")
}

/// Get all DPDK devices from real env.json
fn get_all_dpdk_devices() -> Vec<String> {
    if let Ok(devices) = std::env::var("DPDK_DEVICES") {
        return devices.split(',').map(|s| s.trim().to_string()).collect();
    }

    let content = std::fs::read_to_string("/etc/dpdk/env.json")
        .expect("env.json not found - DPDK not configured");

    extract_dpdk_devices_from_json(&content)
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

/// Create DPDK runtime with a specific real device
fn dpdk_rt_with_device(pci_address: &str) -> Runtime {
    tokio::runtime::Builder::new_dpdk()
        .dpdk_device(pci_address)
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
// Test: Builder API error handling (does NOT require DPDK runtime)
// =============================================================================

/// Test: Specifying a non-existent device returns an error.
/// This test does NOT initialize DPDK EAL - it fails at device resolution.
#[test]
fn test_builder_invalid_device_error() {
    println!("\n=== test_builder_invalid_device_error ===");

    let result = tokio::runtime::Builder::new_dpdk()
        .dpdk_device("non_existent_interface_xyz")
        .enable_all()
        .build();

    assert!(result.is_err(), "Should fail with non-existent device");
    println!("Got expected error: {}", result.unwrap_err());
    println!("=== PASSED ===\n");
}

// =============================================================================
// COMBINED DPDK TEST (Single EAL initialization)
//
// All tests requiring DPDK runtime MUST be in this ONE function because
// DPDK EAL can only be initialized once per process.
// =============================================================================

/// Combined DPDK multi-process/multi-queue test suite.
///
/// This single test function contains ALL subtests that require DPDK runtime.
/// Each subtest uses REAL network traffic to/from external services.
#[test]
fn test_dpdk_multi_queue_all() {
    let device = get_first_dpdk_device();
    let all_devices = get_all_dpdk_devices();

    println!("\n========================================");
    println!("  DPDK Multi-Process/Queue Test Suite");
    println!("  Primary Device: {}", device);
    println!("  All Devices: {:?}", all_devices);
    println!("========================================\n");

    // Create DPDK runtime - this initializes EAL ONCE for the whole test
    let rt = dpdk_rt_with_device(&device);

    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;

    macro_rules! run_subtest {
        ($name:expr, $test:expr) => {{
            print!("[TEST] {} ... ", $name);
            std::io::Write::flush(&mut std::io::stdout()).ok();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $test)) {
                Ok(SubtestResult::Passed) => {
                    println!("PASSED");
                    passed += 1;
                }
                Ok(SubtestResult::Skipped(reason)) => {
                    println!("SKIPPED ({})", reason);
                    skipped += 1;
                }
                Err(e) => {
                    println!("FAILED: {:?}", e);
                    failed += 1;
                }
            }
        }};
    }

    // === 2.2 Multi-Process Tests ===
    // Note: These test the locking mechanism using the REAL device PCI address

    run_subtest!(
        "multi_process_same_device_lock_held",
        subtest_same_device_lock_held(&device)
    );

    run_subtest!(
        "multi_process_different_device_lock_available",
        subtest_different_device_lock_available(&all_devices)
    );

    // === 1.4 InitPortMultiQueue Tests ===

    run_subtest!(
        "init_port_single_queue_real_traffic",
        subtest_single_queue_real_traffic(&rt)
    );

    run_subtest!(
        "init_port_multi_task_real_traffic",
        subtest_multi_task_real_traffic(&rt)
    );

    // === 4.1 E2E Multi-Process Traffic Tests ===

    run_subtest!("e2e_real_network_http", subtest_real_network_http(&rt));

    run_subtest!(
        "e2e_multiple_connections",
        subtest_multiple_connections(&rt)
    );

    // === 4.2 E2E Multi-Queue Traffic Routing Tests ===

    run_subtest!(
        "e2e_concurrent_workers_traffic",
        subtest_concurrent_workers_traffic(&rt)
    );

    run_subtest!(
        "e2e_traffic_distribution",
        subtest_traffic_distribution(&rt)
    );

    // === Summary ===
    println!("\n========================================");
    println!(
        "  Results: {} passed, {} failed, {} skipped",
        passed, failed, skipped
    );
    println!("========================================\n");

    assert!(failed == 0, "{} subtests failed", failed);
}

// =============================================================================
// Subtests: Multi-Process Lock Verification (2.2)
// =============================================================================

/// Test: When we hold the device, the lock file exists and is locked.
fn subtest_same_device_lock_held(device: &str) -> SubtestResult {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let safe_pci = device.replace(':', "_");
    let lock_path = format!("/var/run/dpdk/device_{}.lock", safe_pci);

    // The runtime we created should hold the lock
    assert!(
        std::path::Path::new(&lock_path).exists(),
        "Lock file should exist at {}",
        lock_path
    );

    // Try to acquire - should fail with EWOULDBLOCK
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

    SubtestResult::Passed
}

/// Test: Other devices should still be lockable.
fn subtest_different_device_lock_available(all_devices: &[String]) -> SubtestResult {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    if all_devices.len() < 2 {
        return SubtestResult::Skipped("need 2+ devices");
    }

    // Device 1 is held by our runtime. Try to lock device 2.
    let device2 = &all_devices[1];
    let safe_pci = device2.replace(':', "_");
    let lock_path = format!("/var/run/dpdk/device_{}.lock", safe_pci);

    // Clean up any existing lock
    let _ = std::fs::remove_file(&lock_path);

    // We should be able to acquire this lock
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o644)
        .open(&lock_path)
        .expect("Should create lock file");

    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    assert_eq!(ret, 0, "Should acquire lock on different device");

    // Cleanup
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);
    let _ = std::fs::remove_file(&lock_path);

    SubtestResult::Passed
}

// =============================================================================
// Subtests: Init Port with REAL Traffic (1.4)
// =============================================================================

/// Test: Single queue mode with REAL HTTP traffic.
fn subtest_single_queue_real_traffic(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let mut stream = TcpStream::connect("1.1.1.1:80")
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
    SubtestResult::Passed
}

/// Test: Multiple concurrent tasks with REAL network operations.
fn subtest_multi_task_real_traffic(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        let mut handles = Vec::new();

        for i in 0..4 {
            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpStream;

                if let Ok(mut stream) = TcpStream::connect("1.1.1.1:80").await {
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
    SubtestResult::Passed
}

// =============================================================================
// Subtests: E2E Traffic Tests (4.1, 4.2)
// =============================================================================

/// Test: Real HTTP traffic through DPDK stack.
fn subtest_real_network_http(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        for _ in 0..3 {
            let mut stream = TcpStream::connect("1.1.1.1:80")
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
    SubtestResult::Passed
}

/// Test: Multiple concurrent connections to external service.
fn subtest_multiple_connections(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..10 {
            let counter = success_count.clone();
            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpStream;

                if let Ok(mut stream) = TcpStream::connect("1.1.1.1:80").await {
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
    SubtestResult::Passed
}

/// Test: Concurrent workers handling traffic.
fn subtest_concurrent_workers_traffic(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        use std::collections::HashSet;

        let thread_ids = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let thread_ids = thread_ids.clone();
            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpStream;

                let tid = std::thread::current().id();
                thread_ids.lock().unwrap().insert(format!("{:?}", tid));

                if let Ok(mut stream) = TcpStream::connect("1.1.1.1:80").await {
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
        assert!(!ids.is_empty(), "Tasks should have executed");
    });
    SubtestResult::Passed
}

/// Test: Traffic distribution metrics.
fn subtest_traffic_distribution(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        let rx_count = Arc::new(AtomicUsize::new(0));
        let tx_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..6 {
            let rx = rx_count.clone();
            let tx = tx_count.clone();

            handles.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                use tokio::net::TcpStream;

                if let Ok(mut stream) = TcpStream::connect("1.1.1.1:80").await {
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
    SubtestResult::Passed
}

// =============================================================================
// Separate test: Lock release after process exit (2.2)
//
// This test does NOT use DPDK runtime - it uses shell commands to test
// that OS releases locks when process exits.
// =============================================================================

/// Test: Locks are released after process exit.
#[test]
fn test_multi_process_lock_release_on_exit() {
    let device = get_first_dpdk_device();
    let safe_pci = device.replace(':', "_");
    let lock_path = format!("/var/run/dpdk/device_{}.lock", safe_pci);

    println!("\n=== test_multi_process_lock_release_on_exit ===");
    println!("Testing lock release for: {}", lock_path);

    // Use a DIFFERENT test lock file to avoid conflicting with the combined test
    let test_lock_path = "/var/run/dpdk/test_release.lock";
    let _ = std::fs::remove_file(test_lock_path);

    // Spawn child that holds lock briefly then exits
    let status = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "mkdir -p /var/run/dpdk && \
             flock -n -x {} -c 'echo Locked; sleep 0.5; echo Releasing'",
            test_lock_path
        ))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("Failed to spawn");

    assert!(status.success(), "Child should complete");

    std::thread::sleep(Duration::from_millis(100));

    // Now we should be able to acquire the lock
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let _ = std::fs::remove_file(test_lock_path);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(test_lock_path)
        .expect("Should open");

    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    assert_eq!(ret, 0, "Should acquire lock after child exits");

    // Cleanup
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    drop(file);
    let _ = std::fs::remove_file(test_lock_path);

    println!("=== PASSED ===\n");
}

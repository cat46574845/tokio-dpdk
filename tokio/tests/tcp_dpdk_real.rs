//! Real network TCP tests for DPDK runtime.
//!
//! These tests verify actual network connectivity using the DPDK TCP stack
//! by connecting to public services (Cloudflare).
//!
//! **IMPORTANT**: All tests are combined into a single test function because
//! DPDK EAL (Environment Abstraction Layer) can only be initialized once
//! per process lifetime.
//!
//! Requirements:
//! - DPDK properly configured
//! - Network interface bound to DPDK
//! - IP address and gateway configured in dpdk-env.json

#![cfg(feature = "full")]
#![cfg(not(miri))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;

/// Cloudflare DNS IPv4
const CLOUDFLARE_V4: &str = "1.1.1.1:80";
/// Cloudflare DNS IPv6
const CLOUDFLARE_V6: &str = "[2606:4700:4700::1111]:80";
/// Simple HTTP GET request
const HTTP_GET: &[u8] = b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n";

/// Detect first available DPDK device from env.json configuration.
fn detect_dpdk_device() -> String {
    // Check environment variable first
    if let Ok(dev) = std::env::var("DPDK_DEVICE") {
        return dev;
    }

    // Try to read from env.json
    let config_paths = [
        "/etc/dpdk/env.json",
        "./config/dpdk-env.json",
        "./dpdk-env.json",
    ];

    for path in &config_paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Some(dpdk_device) = find_dpdk_device_in_json(&content) {
                return dpdk_device;
            }
        }
    }

    panic!("No DPDK device found. Set DPDK_DEVICE environment variable or configure /etc/dpdk/env.json")
}

/// Find first DPDK device (PCI address) from JSON config content using serde_json.
fn find_dpdk_device_in_json(content: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    if let Some(devices) = json.get("devices").and_then(|d| d.as_array()) {
        for device in devices {
            let role = device.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "dpdk" {
                // Prefer PCI address as it's always present
                if let Some(pci) = device.get("pci_address").and_then(|p| p.as_str()) {
                    return Some(pci.to_string());
                }
            }
        }
    }

    None
}

/// Create a DPDK runtime for testing.
fn dpdk_rt() -> Runtime {
    let device = detect_dpdk_device();
    tokio::runtime::Builder::new_dpdk()
        .dpdk_device(&device)
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            panic!(
                "DPDK runtime creation failed for device '{}' - ensure DPDK is properly configured: {:?}",
                device, e
            )
        })
}

// =============================================================================
// Combined Test - All subtests run in a single DPDK runtime
// =============================================================================

/// Combined DPDK network test.
///
/// This single test runs all subtests sequentially using one DPDK runtime,
/// because DPDK EAL can only be initialized once per process.
#[test]
fn test_dpdk_network_all() {
    println!("\n========================================");
    println!("  DPDK Network Test Suite");
    println!("========================================\n");

    let rt = dpdk_rt();
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;

    // Run all subtests
    macro_rules! run_subtest {
        ($name:expr, $test:expr) => {{
            print!("[TEST] {} ... ", $name);
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $test)) {
                Ok(result) => match result {
                    SubtestResult::Pass => {
                        println!("PASSED");
                        passed += 1;
                    }
                    SubtestResult::Skip(reason) => {
                        println!("SKIPPED ({})", reason);
                        skipped += 1;
                    }
                    SubtestResult::Fail(reason) => {
                        println!("FAILED: {}", reason);
                        failed += 1;
                    }
                },
                Err(_) => {
                    println!("PANICKED");
                    failed += 1;
                }
            }
        }};
    }

    run_subtest!("IPv4 Connect Cloudflare", subtest_ipv4_connect(&rt));
    run_subtest!("IPv4 Read/Write", subtest_ipv4_read_write(&rt));
    run_subtest!("IPv6 Connect Cloudflare", subtest_ipv6_connect(&rt));
    run_subtest!("Many Connections (50)", subtest_many_connections(&rt));
    run_subtest!("Multi-Worker Tasks", subtest_multi_worker(&rt));

    // Summary
    println!("\n========================================");
    println!(
        "  Results: {} passed, {} failed, {} skipped",
        passed, failed, skipped
    );
    println!("========================================\n");

    // Fail if any subtest failed
    assert!(failed == 0, "{} subtests failed", failed);
}

#[derive(Debug)]
enum SubtestResult {
    Pass,
    Skip(String),
    Fail(String),
}

// =============================================================================
// Subtests
// =============================================================================

fn subtest_ipv4_connect(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        let mut stream = match TcpStream::connect(CLOUDFLARE_V4).await {
            Ok(s) => s,
            Err(e) => return SubtestResult::Skip(format!("connect failed: {}", e)),
        };

        let local = stream.local_addr().expect("Should have local addr");
        let peer = stream.peer_addr().expect("Should have peer addr");

        if !local.ip().is_ipv4() || !peer.ip().is_ipv4() {
            return SubtestResult::Fail("Address not IPv4".to_string());
        }

        // Send HTTP request
        if let Err(e) = stream.write_all(HTTP_GET).await {
            return SubtestResult::Fail(format!("write failed: {}", e));
        }

        // Read response
        let mut buf = [0u8; 1024];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return SubtestResult::Fail(format!("read failed: {}", e)),
            Err(_) => return SubtestResult::Fail("timeout".to_string()),
        };

        if n == 0 {
            return SubtestResult::Fail("no data received".to_string());
        }

        let response = String::from_utf8_lossy(&buf[..n]);
        if !response.contains("HTTP/1.") {
            return SubtestResult::Fail(format!("not HTTP response: {}", &response[..50.min(n)]));
        }

        SubtestResult::Pass
    })
}

fn subtest_ipv4_read_write(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        let mut stream = match TcpStream::connect(CLOUDFLARE_V4).await {
            Ok(s) => s,
            Err(e) => return SubtestResult::Skip(format!("connect failed: {}", e)),
        };

        // Multiple write/read cycles
        for i in 0..3 {
            let request = format!(
                "GET /{} HTTP/1.0\r\nHost: 1.1.1.1\r\nConnection: keep-alive\r\n\r\n",
                i
            );
            if let Err(e) = stream.write_all(request.as_bytes()).await {
                return SubtestResult::Fail(format!("write {} failed: {}", i, e));
            }

            let mut buf = [0u8; 512];
            match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                Ok(Ok(_)) => return SubtestResult::Fail(format!("no data for request {}", i)),
                Ok(Err(e)) => return SubtestResult::Fail(format!("read {} failed: {}", i, e)),
                Err(_) => return SubtestResult::Fail(format!("timeout on request {}", i)),
            }
        }

        SubtestResult::Pass
    })
}

fn subtest_ipv6_connect(rt: &Runtime) -> SubtestResult {
    rt.block_on(async {
        let mut stream = match TcpStream::connect(CLOUDFLARE_V6).await {
            Ok(s) => s,
            Err(e) => return SubtestResult::Skip(format!("IPv6 not available: {}", e)),
        };

        let local = stream.local_addr().expect("Should have local addr");
        let peer = stream.peer_addr().expect("Should have peer addr");

        if !local.ip().is_ipv6() || !peer.ip().is_ipv6() {
            return SubtestResult::Fail("Address not IPv6".to_string());
        }

        // Send HTTP request
        if let Err(e) = stream
            .write_all(b"GET / HTTP/1.0\r\nHost: [2606:4700:4700::1111]\r\n\r\n")
            .await
        {
            return SubtestResult::Fail(format!("write failed: {}", e));
        }

        // Read response
        let mut buf = [0u8; 1024];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return SubtestResult::Fail(format!("read failed: {}", e)),
            Err(_) => return SubtestResult::Fail("timeout".to_string()),
        };

        if n == 0 {
            return SubtestResult::Fail("no data received".to_string());
        }

        let response = String::from_utf8_lossy(&buf[..n]);
        if !response.contains("HTTP/1.") {
            return SubtestResult::Fail("not HTTP response".to_string());
        }

        SubtestResult::Pass
    })
}

fn subtest_many_connections(rt: &Runtime) -> SubtestResult {
    const NUM_CONNECTIONS: usize = 50;

    rt.block_on(async {
        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(NUM_CONNECTIONS);

        for i in 0..NUM_CONNECTIONS {
            let success = success_count.clone();
            let handle = tokio::spawn(async move {
                // Stagger connections
                tokio::time::sleep(Duration::from_millis(i as u64 * 10)).await;

                match TcpStream::connect(CLOUDFLARE_V4).await {
                    Ok(mut stream) => {
                        if stream.write_all(HTTP_GET).await.is_ok() {
                            let mut buf = [0u8; 128];
                            if stream.read(&mut buf).await.is_ok() {
                                success.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(_) => {}
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }

        let successes = success_count.load(Ordering::Relaxed);

        // Expect at least 80% success rate
        if successes >= NUM_CONNECTIONS * 8 / 10 {
            SubtestResult::Pass
        } else {
            SubtestResult::Fail(format!("only {}/{} succeeded", successes, NUM_CONNECTIONS))
        }
    })
}

fn subtest_multi_worker(rt: &Runtime) -> SubtestResult {
    const TASKS_PER_WORKER: usize = 5;

    rt.block_on(async {
        let num_workers = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1)
            .min(4);

        let success_count = Arc::new(AtomicUsize::new(0));
        let total_tasks = num_workers * TASKS_PER_WORKER;
        let mut handles = Vec::with_capacity(total_tasks);

        for _ in 0..total_tasks {
            let success = success_count.clone();
            let handle = tokio::spawn(async move {
                match TcpStream::connect(CLOUDFLARE_V4).await {
                    Ok(mut stream) => {
                        stream.write_all(HTTP_GET).await.ok();
                        let mut buf = [0u8; 128];
                        if stream.read(&mut buf).await.is_ok() {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {}
                }
            });
            handles.push(handle);
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }

        let successes = success_count.load(Ordering::Relaxed);

        if successes >= total_tasks * 7 / 10 {
            SubtestResult::Pass
        } else {
            SubtestResult::Fail(format!("only {}/{} succeeded", successes, total_tasks))
        }
    })
}

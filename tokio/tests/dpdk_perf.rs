//! DPDK Event Loop Performance Benchmarks
//!
//! Measures tick rate under varying I/O and sync task loads.
//! Run with `--release` for accurate results.
//!
//! ```bash
//! sudo rm -rf /var/run/dpdk/*
//! sudo -E DPDK_TEST_PORT=8192 cargo test --package tokio --test dpdk_perf \
//!   --features full --release -- --nocapture
//! ```

#![cfg(all(feature = "full", feature = "rt-multi-thread"))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpDpdkStream, TcpStream};

/// Duration for each benchmark
const BENCH_DURATION: Duration = Duration::from_secs(5);

/// External server for I/O tests
const CLOUDFLARE_V4: &str = "1.1.1.1:80";

/// HTTP request for keeping connections active
const HTTP_GET: &[u8] = b"GET / HTTP/1.1\r\nHost: 1.1.1.1\r\nConnection: keep-alive\r\n\r\n";

// =============================================================================
// Helper Functions
// =============================================================================

fn detect_dpdk_device() -> String {
    std::fs::read_to_string("/home/ubuntu/tokio-dpdk/env.json")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.contains("\"dpdk_pci\""))
                .and_then(|l| l.split('"').nth(3).map(String::from))
        })
        .unwrap_or_else(|| "0000:28:00.0".to_string())
}

fn dpdk_rt() -> tokio::runtime::Runtime {
    let device = detect_dpdk_device();
    tokio::runtime::Builder::new_dpdk()
        .dpdk_device(&device)
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("Failed to create DPDK runtime")
}

/// Count ticks by yielding repeatedly for a fixed duration
async fn count_ticks(duration: Duration) -> u64 {
    let start = Instant::now();
    let mut ticks = 0u64;
    while start.elapsed() < duration {
        tokio::task::yield_now().await;
        ticks += 1;
    }
    ticks
}

/// Calculate and print tick rate
fn report_result(name: &str, ticks: u64, duration: Duration) {
    let ticks_per_sec = ticks as f64 / duration.as_secs_f64();
    println!("[PERF] {:40} {:>12.0} ticks/sec", name, ticks_per_sec);
}

// =============================================================================
// Baseline Test
// =============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_baseline() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let ticks = count_ticks(BENCH_DURATION).await;
        report_result("baseline (no load)", ticks, BENCH_DURATION);
    });
}

// =============================================================================
// DPDK I/O Load Tests
// =============================================================================

async fn run_dpdk_io_benchmark(num_connections: usize) {
    let counter = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn I/O worker tasks
    let mut handles = Vec::with_capacity(num_connections);
    for _ in 0..num_connections {
        let counter = counter.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            // Connect to external server
            let stream = match tokio::time::timeout(
                Duration::from_secs(5),
                TcpDpdkStream::connect(CLOUDFLARE_V4),
            )
            .await
            {
                Ok(Ok(s)) => s,
                _ => return, // Skip failed connections
            };

            let mut stream = stream;
            while !stop.load(Ordering::Relaxed) {
                // Send request
                if stream.write_all(HTTP_GET).await.is_err() {
                    break;
                }
                // Read response (partial)
                let mut buf = [0u8; 256];
                if stream.read(&mut buf).await.is_err() {
                    break;
                }
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }));
    }

    // Wait for connections to establish
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Run tick counter
    let ticks = count_ticks(BENCH_DURATION).await;

    // Stop I/O workers
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    let io_ops = counter.load(Ordering::Relaxed);
    let name = format!("dpdk_io_{} ({} ops)", num_connections, io_ops);
    report_result(&name, ticks, BENCH_DURATION);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_dpdk_io_10() {
    let rt = dpdk_rt();
    rt.block_on(run_dpdk_io_benchmark(10));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_dpdk_io_50() {
    let rt = dpdk_rt();
    rt.block_on(run_dpdk_io_benchmark(50));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_dpdk_io_100() {
    let rt = dpdk_rt();
    rt.block_on(run_dpdk_io_benchmark(100));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_dpdk_io_200() {
    let rt = dpdk_rt();
    rt.block_on(run_dpdk_io_benchmark(200));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_dpdk_io_500() {
    let rt = dpdk_rt();
    rt.block_on(run_dpdk_io_benchmark(500));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_dpdk_io_1000() {
    let rt = dpdk_rt();
    rt.block_on(run_dpdk_io_benchmark(1000));
}

// =============================================================================
// Kernel I/O Load Tests (interference test)
// =============================================================================

async fn run_kernel_io_benchmark(num_connections: usize) {
    let counter = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn I/O worker tasks using kernel TCP
    let mut handles = Vec::with_capacity(num_connections);
    for _ in 0..num_connections {
        let counter = counter.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            // Connect to external server via kernel stack
            let stream = match tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect(CLOUDFLARE_V4),
            )
            .await
            {
                Ok(Ok(s)) => s,
                _ => return,
            };

            let mut stream = stream;
            while !stop.load(Ordering::Relaxed) {
                if stream.write_all(HTTP_GET).await.is_err() {
                    break;
                }
                let mut buf = [0u8; 256];
                if stream.read(&mut buf).await.is_err() {
                    break;
                }
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }));
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    let ticks = count_ticks(BENCH_DURATION).await;

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    let io_ops = counter.load(Ordering::Relaxed);
    let name = format!("kernel_io_{} ({} ops)", num_connections, io_ops);
    report_result(&name, ticks, BENCH_DURATION);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_kernel_io_10() {
    let rt = dpdk_rt();
    rt.block_on(run_kernel_io_benchmark(10));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_kernel_io_50() {
    let rt = dpdk_rt();
    rt.block_on(run_kernel_io_benchmark(50));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_kernel_io_100() {
    let rt = dpdk_rt();
    rt.block_on(run_kernel_io_benchmark(100));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_kernel_io_200() {
    let rt = dpdk_rt();
    rt.block_on(run_kernel_io_benchmark(200));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_kernel_io_500() {
    let rt = dpdk_rt();
    rt.block_on(run_kernel_io_benchmark(500));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_kernel_io_1000() {
    let rt = dpdk_rt();
    rt.block_on(run_kernel_io_benchmark(1000));
}

// =============================================================================
// Sync Task Load Tests
// =============================================================================

async fn run_sync_benchmark(num_tasks: usize) {
    let counter = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn compute tasks
    let mut handles = Vec::with_capacity(num_tasks);
    for _ in 0..num_tasks {
        let counter = counter.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }));
    }

    let ticks = count_ticks(BENCH_DURATION).await;

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }

    let ops = counter.load(Ordering::Relaxed);
    let name = format!("sync_{} ({} ops)", num_tasks, ops);
    report_result(&name, ticks, BENCH_DURATION);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_sync_100() {
    let rt = dpdk_rt();
    rt.block_on(run_sync_benchmark(100));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_sync_500() {
    let rt = dpdk_rt();
    rt.block_on(run_sync_benchmark(500));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_sync_1000() {
    let rt = dpdk_rt();
    rt.block_on(run_sync_benchmark(1000));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_sync_2000() {
    let rt = dpdk_rt();
    rt.block_on(run_sync_benchmark(2000));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_sync_5000() {
    let rt = dpdk_rt();
    rt.block_on(run_sync_benchmark(5000));
}

// =============================================================================
// Mixed Load Tests (DPDK I/O + Kernel I/O + Sync)
// =============================================================================

async fn run_mixed_benchmark(dpdk_conns: usize, kernel_conns: usize, sync_tasks: usize) {
    let dpdk_counter = Arc::new(AtomicU64::new(0));
    let kernel_counter = Arc::new(AtomicU64::new(0));
    let sync_counter = Arc::new(AtomicU64::new(0));
    let dpdk_connected = Arc::new(AtomicU64::new(0));
    let dpdk_failed = Arc::new(AtomicU64::new(0));
    let kernel_connected = Arc::new(AtomicU64::new(0));
    let kernel_failed = Arc::new(AtomicU64::new(0));
    let dpdk_writes = Arc::new(AtomicU64::new(0));
    let dpdk_reads = Arc::new(AtomicU64::new(0));
    let kernel_writes = Arc::new(AtomicU64::new(0));
    let kernel_reads = Arc::new(AtomicU64::new(0));

    // Await point counters
    let dpdk_await_connect = Arc::new(AtomicU64::new(0));
    let dpdk_await_write = Arc::new(AtomicU64::new(0));
    let dpdk_await_read = Arc::new(AtomicU64::new(0));
    let dpdk_await_yield = Arc::new(AtomicU64::new(0));
    let kernel_await_connect = Arc::new(AtomicU64::new(0));
    let kernel_await_write = Arc::new(AtomicU64::new(0));
    let kernel_await_read = Arc::new(AtomicU64::new(0));
    let kernel_await_yield = Arc::new(AtomicU64::new(0));
    let sync_await_yield = Arc::new(AtomicU64::new(0));

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut handles = Vec::new();

    // DPDK I/O
    for i in 0..dpdk_conns {
        let counter = dpdk_counter.clone();
        let stop = stop.clone();
        let connected = dpdk_connected.clone();
        let failed = dpdk_failed.clone();
        let writes = dpdk_writes.clone();
        let reads = dpdk_reads.clone();
        let await_connect = dpdk_await_connect.clone();
        let await_write = dpdk_await_write.clone();
        let await_read = dpdk_await_read.clone();
        let await_yield = dpdk_await_yield.clone();
        handles.push(tokio::spawn(async move {
            let stream = match tokio::time::timeout(
                Duration::from_secs(5),
                TcpDpdkStream::connect(CLOUDFLARE_V4),
            )
            .await
            {
                Ok(Ok(s)) => {
                    await_connect.fetch_add(1, Ordering::Relaxed);
                    connected.fetch_add(1, Ordering::Relaxed);
                    s
                }
                Ok(Err(e)) => {
                    await_connect.fetch_add(1, Ordering::Relaxed);
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[DEBUG] DPDK connect {} failed: {}", i, e);
                    return;
                }
                Err(_) => {
                    await_connect.fetch_add(1, Ordering::Relaxed);
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[DEBUG] DPDK connect {} timeout", i);
                    return;
                }
            };
            let mut stream = stream;
            while !stop.load(Ordering::Relaxed) {
                if stream.write_all(HTTP_GET).await.is_err() {
                    await_write.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                await_write.fetch_add(1, Ordering::Relaxed);
                writes.fetch_add(1, Ordering::Relaxed);
                let mut buf = [0u8; 256];
                if stream.read(&mut buf).await.is_err() {
                    await_read.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                await_read.fetch_add(1, Ordering::Relaxed);
                reads.fetch_add(1, Ordering::Relaxed);
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                await_yield.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Kernel I/O
    for i in 0..kernel_conns {
        let counter = kernel_counter.clone();
        let stop = stop.clone();
        let connected = kernel_connected.clone();
        let failed = kernel_failed.clone();
        let writes = kernel_writes.clone();
        let reads = kernel_reads.clone();
        let await_connect = kernel_await_connect.clone();
        let await_write = kernel_await_write.clone();
        let await_read = kernel_await_read.clone();
        let await_yield = kernel_await_yield.clone();
        handles.push(tokio::spawn(async move {
            let stream = match tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect(CLOUDFLARE_V4),
            )
            .await
            {
                Ok(Ok(s)) => {
                    await_connect.fetch_add(1, Ordering::Relaxed);
                    connected.fetch_add(1, Ordering::Relaxed);
                    s
                }
                Ok(Err(e)) => {
                    await_connect.fetch_add(1, Ordering::Relaxed);
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[DEBUG] Kernel connect {} failed: {}", i, e);
                    return;
                }
                Err(_) => {
                    await_connect.fetch_add(1, Ordering::Relaxed);
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[DEBUG] Kernel connect {} timeout", i);
                    return;
                }
            };
            let mut stream = stream;
            while !stop.load(Ordering::Relaxed) {
                if stream.write_all(HTTP_GET).await.is_err() {
                    await_write.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                await_write.fetch_add(1, Ordering::Relaxed);
                writes.fetch_add(1, Ordering::Relaxed);
                let mut buf = [0u8; 256];
                if stream.read(&mut buf).await.is_err() {
                    await_read.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                await_read.fetch_add(1, Ordering::Relaxed);
                reads.fetch_add(1, Ordering::Relaxed);
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                await_yield.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Sync tasks
    for _ in 0..sync_tasks {
        let counter = sync_counter.clone();
        let stop = stop.clone();
        let await_yield = sync_await_yield.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                await_yield.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    let ticks = count_ticks(BENCH_DURATION).await;

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    let dpdk_ops = dpdk_counter.load(Ordering::Relaxed);
    let kernel_ops = kernel_counter.load(Ordering::Relaxed);
    let sync_ops = sync_counter.load(Ordering::Relaxed);
    let dpdk_ok = dpdk_connected.load(Ordering::Relaxed);
    let _dpdk_err = dpdk_failed.load(Ordering::Relaxed);
    let kernel_ok = kernel_connected.load(Ordering::Relaxed);
    let _kernel_err = kernel_failed.load(Ordering::Relaxed);
    let dpdk_w = dpdk_writes.load(Ordering::Relaxed);
    let dpdk_r = dpdk_reads.load(Ordering::Relaxed);
    let kernel_w = kernel_writes.load(Ordering::Relaxed);
    let kernel_r = kernel_reads.load(Ordering::Relaxed);

    eprintln!(
        "[CONN] dpdk: {}/{} ok, kernel: {}/{} ok",
        dpdk_ok, dpdk_conns, kernel_ok, kernel_conns
    );
    eprintln!(
        "[IO] dpdk: w={} r={}, kernel: w={} r={}",
        dpdk_w, dpdk_r, kernel_w, kernel_r
    );
    eprintln!(
        "[AWAIT-DPDK] connect={} write={} read={} yield={}",
        dpdk_await_connect.load(Ordering::Relaxed),
        dpdk_await_write.load(Ordering::Relaxed),
        dpdk_await_read.load(Ordering::Relaxed),
        dpdk_await_yield.load(Ordering::Relaxed)
    );
    eprintln!(
        "[AWAIT-KERN] connect={} write={} read={} yield={}",
        kernel_await_connect.load(Ordering::Relaxed),
        kernel_await_write.load(Ordering::Relaxed),
        kernel_await_read.load(Ordering::Relaxed),
        kernel_await_yield.load(Ordering::Relaxed)
    );
    eprintln!(
        "[AWAIT-SYNC] yield={}",
        sync_await_yield.load(Ordering::Relaxed)
    );

    let name = format!(
        "mixed_d{}_k{}_s{} (dpdk:{} kern:{} sync:{})",
        dpdk_conns, kernel_conns, sync_tasks, dpdk_ops, kernel_ops, sync_ops
    );
    report_result(&name, ticks, BENCH_DURATION);
}

/// Mixed benchmark with interleaved spawning - spawns tasks in round-robin order
async fn run_mixed_interleaved_benchmark(
    dpdk_conns: usize,
    kernel_conns: usize,
    sync_tasks: usize,
) {
    let dpdk_counter = Arc::new(AtomicU64::new(0));
    let kernel_counter = Arc::new(AtomicU64::new(0));
    let sync_counter = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = Vec::new();

    // Interleaved spawning: round-robin across all task types
    let max_count = dpdk_conns.max(kernel_conns).max(sync_tasks);

    for i in 0..max_count {
        // Spawn DPDK task if still have some
        if i < dpdk_conns {
            let counter = dpdk_counter.clone();
            let stop = stop.clone();
            handles.push(tokio::spawn(async move {
                let stream = match tokio::time::timeout(
                    Duration::from_secs(5),
                    TcpDpdkStream::connect(CLOUDFLARE_V4),
                )
                .await
                {
                    Ok(Ok(s)) => s,
                    _ => return,
                };
                let mut stream = stream;
                while !stop.load(Ordering::Relaxed) {
                    if stream.write_all(HTTP_GET).await.is_err() {
                        break;
                    }
                    let mut buf = [0u8; 256];
                    if stream.read(&mut buf).await.is_err() {
                        break;
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            }));
        }

        // Spawn Kernel task if still have some
        if i < kernel_conns {
            let counter = kernel_counter.clone();
            let stop = stop.clone();
            handles.push(tokio::spawn(async move {
                let stream = match tokio::time::timeout(
                    Duration::from_secs(5),
                    TcpStream::connect(CLOUDFLARE_V4),
                )
                .await
                {
                    Ok(Ok(s)) => s,
                    _ => return,
                };
                let mut stream = stream;
                while !stop.load(Ordering::Relaxed) {
                    if stream.write_all(HTTP_GET).await.is_err() {
                        break;
                    }
                    let mut buf = [0u8; 256];
                    if stream.read(&mut buf).await.is_err() {
                        break;
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            }));
        }

        // Spawn Sync task if still have some
        if i < sync_tasks {
            let counter = sync_counter.clone();
            let stop = stop.clone();
            handles.push(tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    counter.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            }));
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let ticks = count_ticks(BENCH_DURATION).await;
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    // Collect results
    let dpdk_ops = dpdk_counter.load(Ordering::Relaxed);
    let kernel_ops = kernel_counter.load(Ordering::Relaxed);
    let sync_ops = sync_counter.load(Ordering::Relaxed);

    let name = format!(
        "interleaved_d{}_k{}_s{} (dpdk:{} kern:{} sync:{})",
        dpdk_conns, kernel_conns, sync_tasks, dpdk_ops, kernel_ops, sync_ops
    );
    report_result(&name, ticks, BENCH_DURATION);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_mixed_dpdk_kernel_100() {
    let rt = dpdk_rt();
    rt.block_on(run_mixed_benchmark(50, 50, 0));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_mixed_dpdk_kernel_500() {
    let rt = dpdk_rt();
    rt.block_on(run_mixed_benchmark(250, 250, 0));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_mixed_all_light() {
    let rt = dpdk_rt();
    rt.block_on(run_mixed_benchmark(50, 50, 500));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_mixed_all_heavy() {
    let rt = dpdk_rt();
    rt.block_on(run_mixed_benchmark(200, 200, 2000));
}

// =============================================================================
// Combined Test Runner
// =============================================================================

/// Run all performance benchmarks in sequence (single process to avoid EAL issues)
#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn perf_all_benchmarks() {
    println!("\n========================================");
    println!("  DPDK Event Loop Performance Benchmarks");
    println!("  Duration: {} seconds per test", BENCH_DURATION.as_secs());
    println!("========================================\n");

    let rt = dpdk_rt();
    rt.block_on(async {
        // Baseline
        println!("\n--- Baseline ---");
        let ticks = count_ticks(BENCH_DURATION).await;
        report_result("baseline (no load)", ticks, BENCH_DURATION);

        // DPDK I/O
        println!("\n--- DPDK I/O Load ---");
        for n in [10, 50, 100, 200, 500] {
            run_dpdk_io_benchmark(n).await;
        }

        // Kernel I/O
        println!("\n--- Kernel I/O Load ---");
        for n in [10, 50, 100, 200, 500] {
            run_kernel_io_benchmark(n).await;
        }

        // Sync tasks
        println!("\n--- Sync Task Load ---");
        for n in [100, 500, 1000, 2000, 5000] {
            run_sync_benchmark(n).await;
        }

        // Mixed
        println!("\n--- Mixed Load ---");
        run_mixed_benchmark(50, 50, 50).await;
        run_mixed_benchmark(250, 250, 250).await;
        run_mixed_benchmark(50, 50, 500).await;
        run_mixed_benchmark(200, 200, 2000).await;

        // Interleaved Mixed (tasks spawned in round-robin order)
        println!("\n--- Interleaved Mixed Load ---");
        run_mixed_interleaved_benchmark(50, 50, 500).await;
        run_mixed_interleaved_benchmark(200, 200, 2000).await;

        println!("\n========================================");
        println!("  Benchmarks Complete");
        println!("========================================\n");
    });
}

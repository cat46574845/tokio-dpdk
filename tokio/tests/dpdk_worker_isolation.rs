//! Worker Data Isolation Tests for DPDK Runtime.
//!
//! These tests verify that connections on different DPDK workers receive
//! their own data correctly without cross-contamination.
//!
//! Test Architecture:
//! 1. A standard tokio TCP echo server runs in a separate thread (non-DPDK)
//! 2. Multiple DPDK workers create concurrent connections to the server
//! 3. Each connection sends a unique identifier
//! 4. The server echoes back the exact data received
//! 5. Each client verifies it received its own identifier (not another client's)
//!
//! This tests the fundamental correctness of multi-queue DPDK traffic routing.
//!
//! **IMPORTANT**: All tests are combined into a single test function because
//! DPDK EAL can only be initialized once per process.

#![cfg(feature = "full")]
#![cfg(not(miri))]
#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::sync::Barrier;

// =============================================================================
// Test Configuration
// =============================================================================

/// Server bind address (localhost, random port)
const SERVER_BIND: &str = "127.0.0.1:0";

// =============================================================================
// Echo Server Implementation
// =============================================================================

/// Starts an echo server that prefixes each line with a sequence number.
/// Returns the server's actual bound address.
fn start_echo_server() -> (SocketAddr, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    // Use a channel to get the bound address back
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        // Create a separate tokio runtime for the server (standard, not DPDK)
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create server runtime");

        rt.block_on(async {
            let listener = TcpListener::bind(SERVER_BIND)
                .await
                .expect("Failed to bind server");

            let addr = listener.local_addr().expect("Failed to get local addr");
            addr_tx.send(addr).expect("Failed to send addr");

            let connection_counter = Arc::new(AtomicUsize::new(0));

            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((socket, peer)) => {
                                let conn_id = connection_counter.fetch_add(1, Ordering::Relaxed);
                                tokio::spawn(handle_client(socket, peer, conn_id));
                            }
                            Err(e) => {
                                eprintln!("Accept error: {}", e);
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if shutdown_clone.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        });
    });

    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("Timeout waiting for server address");

    (addr, shutdown, handle)
}

/// Handle a single client connection.
/// Echoes back each line with format: "ECHO:<conn_id>:<seq>:<original_data>"
async fn handle_client(socket: tokio::net::TcpStream, _peer: SocketAddr, conn_id: usize) {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut seq = 0usize;

    loop {
        line.clear();
        match tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line)).await {
            Ok(Ok(0)) => break, // Connection closed
            Ok(Ok(_)) => {
                // Echo back with metadata
                let response = format!("ECHO:{}:{}:{}", conn_id, seq, line);
                if writer.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
                seq += 1;
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
}

// =============================================================================
// DPDK Device Detection (reused from other tests)
// =============================================================================

fn detect_dpdk_device() -> String {
    if let Ok(dev) = std::env::var("DPDK_DEVICE") {
        return dev;
    }

    let config_paths = [
        "/etc/dpdk/env.json",
        "./config/dpdk-env.json",
        "./dpdk-env.json",
    ];

    for path in &config_paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Some(device) = find_dpdk_device_in_json(&content) {
                return device;
            }
        }
    }

    panic!("No DPDK device found. Set DPDK_DEVICE or configure /etc/dpdk/env.json");
}

fn find_dpdk_device_in_json(content: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    if let Some(devices) = json.get("devices").and_then(|d| d.as_array()) {
        for device in devices {
            let role = device.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "dpdk" {
                if let Some(pci) = device.get("pci_address").and_then(|p| p.as_str()) {
                    return Some(pci.to_string());
                }
            }
        }
    }

    None
}

fn dpdk_rt() -> Runtime {
    let device = detect_dpdk_device();
    tokio::runtime::Builder::new_dpdk()
        .dpdk_device(&device)
        .enable_all()
        .build()
        .expect("DPDK runtime creation failed")
}

// =============================================================================
// Test Result Tracking
// =============================================================================

#[derive(Debug)]
struct ClientResult {
    messages_sent: usize,
    messages_received: usize,
    correct_responses: usize,
    wrong_responses: usize,
    errors: Vec<String>,
}

impl ClientResult {
    fn new() -> Self {
        Self {
            messages_sent: 0,
            messages_received: 0,
            correct_responses: 0,
            wrong_responses: 0,
            errors: Vec::new(),
        }
    }
}

// =============================================================================
// Subtest Result
// =============================================================================

#[derive(Debug)]
enum SubtestResult {
    Passed,
    Failed(String),
}

// =============================================================================
// Main Combined Test
// =============================================================================

/// Combined test that verifies worker data isolation.
///
/// Each client sends messages containing its unique ID, and verifies that
/// it receives back only its own messages, not messages from other clients.
///
/// This test contains multiple subtests that run sequentially with the same
/// DPDK runtime (since EAL can only be initialized once per process).
#[test]
fn test_dpdk_worker_data_isolation() {
    println!("\n========================================");
    println!("  DPDK Worker Data Isolation Test Suite");
    println!("========================================\n");

    // Start echo server in a separate thread
    println!("[SERVER] Starting echo server...");
    let (server_addr, shutdown, server_handle) = start_echo_server();
    println!("[SERVER] Listening on {}", server_addr);

    // Give server a moment to be ready
    std::thread::sleep(Duration::from_millis(100));

    // Create DPDK runtime (ONCE for all subtests)
    println!("[DPDK] Creating DPDK runtime...");
    let rt = dpdk_rt();

    let mut passed = 0;
    let mut failed = 0;

    // Subtest 1: Basic isolation test (20 clients × 10 messages)
    print!("[TEST] Basic Isolation (20 clients × 10 msgs) ... ");
    match run_basic_isolation_test(&rt, server_addr) {
        SubtestResult::Passed => {
            println!("PASSED");
            passed += 1;
        }
        SubtestResult::Failed(msg) => {
            println!("FAILED: {}", msg);
            failed += 1;
        }
    }

    // Subtest 2: High contention test (50 clients × 5 messages)
    print!("[TEST] High Contention (50 clients × 5 msgs) ... ");
    match run_high_contention_test(&rt, server_addr) {
        SubtestResult::Passed => {
            println!("PASSED");
            passed += 1;
        }
        SubtestResult::Failed(msg) => {
            println!("FAILED: {}", msg);
            failed += 1;
        }
    }

    // Subtest 3: Rapid reconnect test (10 clients, each reconnects 5 times)
    print!("[TEST] Rapid Reconnect (10 clients × 5 reconnects) ... ");
    match run_rapid_reconnect_test(&rt, server_addr) {
        SubtestResult::Passed => {
            println!("PASSED");
            passed += 1;
        }
        SubtestResult::Failed(msg) => {
            println!("FAILED: {}", msg);
            failed += 1;
        }
    }

    // Shutdown server
    shutdown.store(true, Ordering::Relaxed);
    let _ = server_handle.join();

    // Summary
    println!("\n========================================");
    println!("  Results: {} passed, {} failed", passed, failed);
    println!("========================================\n");

    assert_eq!(failed, 0, "{} subtests failed", failed);
}

// =============================================================================
// Subtest Implementations
// =============================================================================

fn run_basic_isolation_test(rt: &Runtime, server_addr: SocketAddr) -> SubtestResult {
    const NUM_CLIENTS: usize = 20;
    const MESSAGES_PER_CLIENT: usize = 10;

    let results = rt.block_on(async {
        run_isolation_test(server_addr, NUM_CLIENTS, MESSAGES_PER_CLIENT).await
    });

    let total_wrong: usize = results.iter().map(|r| r.wrong_responses).sum();
    let total_correct: usize = results.iter().map(|r| r.correct_responses).sum();
    let expected = NUM_CLIENTS * MESSAGES_PER_CLIENT;

    if total_wrong > 0 {
        SubtestResult::Failed(format!("{} messages received by wrong client", total_wrong))
    } else if total_correct < expected * 8 / 10 {
        SubtestResult::Failed(format!(
            "only {}/{} correct responses",
            total_correct, expected
        ))
    } else {
        SubtestResult::Passed
    }
}

fn run_high_contention_test(rt: &Runtime, server_addr: SocketAddr) -> SubtestResult {
    const NUM_CLIENTS: usize = 50;
    const MESSAGES_PER_CLIENT: usize = 5;

    let results = rt.block_on(async {
        run_isolation_test(server_addr, NUM_CLIENTS, MESSAGES_PER_CLIENT).await
    });

    let total_wrong: usize = results.iter().map(|r| r.wrong_responses).sum();
    let total_correct: usize = results.iter().map(|r| r.correct_responses).sum();
    let expected = NUM_CLIENTS * MESSAGES_PER_CLIENT;

    if total_wrong > 0 {
        SubtestResult::Failed(format!("{} messages received by wrong client", total_wrong))
    } else if total_correct < expected * 7 / 10 {
        SubtestResult::Failed(format!(
            "only {}/{} correct responses",
            total_correct, expected
        ))
    } else {
        SubtestResult::Passed
    }
}

fn run_rapid_reconnect_test(rt: &Runtime, server_addr: SocketAddr) -> SubtestResult {
    const NUM_CLIENTS: usize = 10;
    const RECONNECTS_PER_CLIENT: usize = 5;
    const MESSAGES_PER_CONNECTION: usize = 3;

    let results = rt.block_on(async {
        let results = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for client_id in 0..NUM_CLIENTS {
            let results = results.clone();
            let handle = tokio::spawn(async move {
                for reconnect in 0..RECONNECTS_PER_CLIENT {
                    let unique_id = client_id * 1000 + reconnect;
                    let result =
                        run_client_n_messages(unique_id, server_addr, MESSAGES_PER_CONNECTION)
                            .await;
                    results.lock().await.push(result);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(60), handle).await;
        }

        Arc::try_unwrap(results)
            .expect("Arc still has references")
            .into_inner()
    });

    let total_wrong: usize = results.iter().map(|r| r.wrong_responses).sum();
    let total_correct: usize = results.iter().map(|r| r.correct_responses).sum();
    let expected = NUM_CLIENTS * RECONNECTS_PER_CLIENT * MESSAGES_PER_CONNECTION;

    if total_wrong > 0 {
        SubtestResult::Failed(format!("{} messages received by wrong client", total_wrong))
    } else if total_correct < expected * 7 / 10 {
        SubtestResult::Failed(format!(
            "only {}/{} correct responses",
            total_correct, expected
        ))
    } else {
        SubtestResult::Passed
    }
}

// =============================================================================
// Test Helpers
// =============================================================================

async fn run_isolation_test(
    server_addr: SocketAddr,
    num_clients: usize,
    messages_per_client: usize,
) -> Vec<ClientResult> {
    let results = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(num_clients)));
    let barrier = Arc::new(Barrier::new(num_clients));

    let mut handles = Vec::with_capacity(num_clients);

    for client_id in 0..num_clients {
        let results = results.clone();
        let barrier = barrier.clone();

        let handle = tokio::spawn(async move {
            // Wait for all clients to be ready
            barrier.wait().await;

            let result = run_client_n_messages(client_id, server_addr, messages_per_client).await;
            results.lock().await.push(result);
        });

        handles.push(handle);
    }

    // Wait for all clients to complete
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(60), handle).await;
    }

    Arc::try_unwrap(results)
        .expect("Arc still has references")
        .into_inner()
}

async fn run_client_n_messages(
    client_id: usize,
    server_addr: SocketAddr,
    num_messages: usize,
) -> ClientResult {
    let mut result = ClientResult::new();

    // Connect to server
    let stream = match tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(server_addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            result.errors.push(format!("Connect failed: {}", e));
            return result;
        }
        Err(_) => {
            result.errors.push("Connect timeout".to_string());
            return result;
        }
    };

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send messages and verify responses
    for seq in 0..num_messages {
        // Create unique message: "CLIENT:<client_id>:<seq>:<random_data>\n"
        let random_data = format!("{:016x}", rand_u64());
        let message = format!("CLIENT:{}:{}:{}\n", client_id, seq, random_data);

        // Send message
        if writer.write_all(message.as_bytes()).await.is_err() {
            result.errors.push(format!("Write failed at seq {}", seq));
            return result;
        }
        result.messages_sent += 1;

        // Read response
        let mut response = String::new();
        match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut response)).await {
            Ok(Ok(0)) => {
                result
                    .errors
                    .push(format!("Connection closed at seq {}", seq));
                return result;
            }
            Ok(Ok(_)) => {
                result.messages_received += 1;

                // Verify response contains our data
                // Response format: "ECHO:<conn_id>:<echo_seq>:CLIENT:<client_id>:<seq>:<random_data>\n"
                if response.contains(&format!("CLIENT:{}:{}:{}", client_id, seq, random_data)) {
                    result.correct_responses += 1;
                } else {
                    result.wrong_responses += 1;
                    result.errors.push(format!(
                        "Wrong response at seq {}: expected CLIENT:{}:{}:{}, got: {}",
                        seq,
                        client_id,
                        seq,
                        random_data,
                        response.trim()
                    ));
                }
            }
            Ok(Err(e)) => {
                result
                    .errors
                    .push(format!("Read failed at seq {}: {}", seq, e));
                return result;
            }
            Err(_) => {
                result.errors.push(format!("Read timeout at seq {}", seq));
                return result;
            }
        }
    }

    result
}

/// Simple random number generator (no external dependency)
fn rand_u64() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tid = std::thread::current().id();
    let tid_hash = format!("{:?}", tid).len() as u64;
    ((nanos as u64) ^ (tid_hash << 32)).wrapping_mul(0x517cc1b727220a95)
}

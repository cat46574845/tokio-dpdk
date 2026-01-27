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
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpDpdkStream;
// Standard TCP types for the echo server (runs in separate non-DPDK runtime)
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::Barrier;

// Debug flag - set to true to enable debug output
const DEBUG_ISOLATION_TEST: bool = false;

// Debug macro with file:line and timestamp - compiles to nothing when DEBUG_ISOLATION_TEST is false
macro_rules! dbg_print {
    ($start:expr, $($arg:tt)*) => {
        if DEBUG_ISOLATION_TEST {
            let elapsed = $start.elapsed();
            eprintln!("[{:>12.3?}] {}:{} - {}", elapsed, file!(), line!(), format!($($arg)*));
        }
    };
}

// =============================================================================
// Test Configuration
// =============================================================================

/// Get the server bind address from env.json (kernel IP) and DPDK_TEST_PORT env var.
/// Panics if DPDK_TEST_PORT is not set.
fn get_server_bind_addr() -> String {
    let kernel_ip = get_kernel_ip();
    let port: u16 = std::env::var("DPDK_TEST_PORT")
        .expect("DPDK_TEST_PORT environment variable is required for tests")
        .parse()
        .expect("DPDK_TEST_PORT must be a valid port number");
    format!("{}:{}", kernel_ip, port)
}

/// Get the primary kernel IP address from env.json.
/// Selects the kernel interface with the most IPv4 addresses.
fn get_kernel_ip() -> String {
    const CONFIG_PATHS: &[&str] = &[
        "/etc/dpdk/env.json",
        "./config/dpdk-env.json",
        "./dpdk-env.json",
    ];

    for path in CONFIG_PATHS {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(devices) = json.get("devices").and_then(|d| d.as_array()) {
                    let mut best_ip: Option<String> = None;
                    let mut best_count = 0;

                    for device in devices {
                        if device.get("role").and_then(|r| r.as_str()) == Some("kernel") {
                            if let Some(addrs) = device.get("addresses").and_then(|a| a.as_array())
                            {
                                let ipv4_count = addrs
                                    .iter()
                                    .filter(|a| {
                                        a.as_str().map(|s| !s.contains(':')).unwrap_or(false)
                                    })
                                    .count();
                                if ipv4_count > best_count {
                                    if let Some(first_v4) = addrs
                                        .iter()
                                        .filter_map(|a| a.as_str())
                                        .find(|a| !a.contains(':'))
                                    {
                                        best_ip = Some(
                                            first_v4
                                                .split('/')
                                                .next()
                                                .unwrap_or(first_v4)
                                                .to_string(),
                                        );
                                        best_count = ipv4_count;
                                    }
                                }
                            }
                        }
                    }

                    if let Some(ip) = best_ip {
                        return ip;
                    }
                }
            }
        }
    }

    panic!(
        "No kernel device found in env.json. Searched: {:?}",
        CONFIG_PATHS
    )
}

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
            let bind_addr = get_server_bind_addr();
            let listener = TcpListener::bind(&bind_addr)
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
                // Flush immediately to ensure response is sent
                if writer.flush().await.is_err() {
                    break;
                }
                seq += 1;
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
}

fn dpdk_rt() -> Runtime {
    tokio::runtime::Builder::new_dpdk()
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
// Test Helpers
// =============================================================================

async fn run_isolation_test(
    server_addr: SocketAddr,
    num_clients: usize,
    messages_per_client: usize,
) -> Vec<ClientResult> {
    let test_start = Instant::now();
    dbg_print!(
        test_start,
        "run_isolation_test: spawning {} clients",
        num_clients
    );
    let results = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(num_clients)));
    let barrier = Arc::new(Barrier::new(num_clients));
    let arrived = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::with_capacity(num_clients);

    for client_id in 0..num_clients {
        let results = results.clone();
        let barrier = barrier.clone();
        let arrived = arrived.clone();

        let handle = tokio::spawn(async move {
            let task_start = Instant::now();
            dbg_print!(
                task_start,
                "Client {} spawned, waiting on barrier",
                client_id
            );
            // Wait for all clients to be ready
            let count = arrived.fetch_add(1, Ordering::SeqCst) + 1;
            dbg_print!(
                task_start,
                "Client {} at barrier, arrived={}/{}",
                client_id,
                count,
                num_clients
            );
            barrier.wait().await;
            dbg_print!(
                task_start,
                "Client {} passed barrier, starting work",
                client_id
            );

            let result = run_client_n_messages(client_id, server_addr, messages_per_client).await;
            dbg_print!(task_start, "Client {} finished work", client_id);
            results.lock().await.push(result);
        });

        handles.push(handle);
    }

    dbg_print!(
        test_start,
        "All {} clients spawned, waiting for completion",
        num_clients
    );

    // Wait for all clients to complete
    for (i, handle) in handles.into_iter().enumerate() {
        match tokio::time::timeout(Duration::from_secs(60), handle).await {
            Ok(Ok(())) => dbg_print!(test_start, "Handle {} completed", i),
            Ok(Err(e)) => dbg_print!(test_start, "Handle {} panicked: {:?}", i, e),
            Err(_) => dbg_print!(test_start, "Handle {} timed out", i),
        }
    }

    dbg_print!(test_start, "All handles processed");

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
    let msg_start = Instant::now();

    dbg_print!(
        msg_start,
        "Client {} connecting to {}",
        client_id,
        server_addr
    );
    // Connect to server
    let stream =
        match tokio::time::timeout(Duration::from_secs(10), TcpDpdkStream::connect(server_addr))
            .await
        {
            Ok(Ok(s)) => {
                dbg_print!(msg_start, "Client {} connected", client_id);
                s
            }
            Ok(Err(e)) => {
                dbg_print!(msg_start, "Client {} connect failed: {}", client_id, e);
                result.errors.push(format!("Connect failed: {}", e));
                return result;
            }
            Err(_) => {
                dbg_print!(msg_start, "Client {} connect timeout", client_id);
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

        let t0 = Instant::now();
        // Send message
        if writer.write_all(message.as_bytes()).await.is_err() {
            dbg_print!(
                msg_start,
                "Client {} write failed at seq {} after {:?}",
                client_id,
                seq,
                t0.elapsed()
            );
            result.errors.push(format!("Write failed at seq {}", seq));
            return result;
        }
        let t1 = Instant::now();
        // Flush to ensure immediate send
        if writer.flush().await.is_err() {
            dbg_print!(
                msg_start,
                "Client {} flush failed at seq {} after {:?}",
                client_id,
                seq,
                t0.elapsed()
            );
            result.errors.push(format!("Flush failed at seq {}", seq));
            return result;
        }
        let t2 = Instant::now();
        result.messages_sent += 1;

        // Read response
        let mut response = String::new();
        match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut response)).await {
            Ok(Ok(0)) => {
                dbg_print!(
                    msg_start,
                    "Client {} connection closed at seq {}",
                    client_id,
                    seq
                );
                result
                    .errors
                    .push(format!("Connection closed at seq {}", seq));
                return result;
            }
            Ok(Ok(_)) => {
                let t3 = Instant::now();
                dbg_print!(
                    msg_start,
                    "Client {} msg {} write={:?} flush={:?} read={:?} RTT={:?}",
                    client_id,
                    seq,
                    t1 - t0,
                    t2 - t1,
                    t3 - t2,
                    t3 - t0
                );
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

// =============================================================================
// Independent Tests (Each spawns its own echo server and DPDK runtime)
// =============================================================================

/// Test basic isolation: 20 clients × 10 messages
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_basic_isolation() {
    const NUM_CLIENTS: usize = 20;
    const MESSAGES_PER_CLIENT: usize = 10;

    let (server_addr, shutdown, server_handle) = start_echo_server();
    std::thread::sleep(Duration::from_millis(100));
    let rt = dpdk_rt();

    let results = rt.block_on(async {
        run_isolation_test(server_addr, NUM_CLIENTS, MESSAGES_PER_CLIENT).await
    });

    shutdown.store(true, Ordering::Relaxed);
    let _ = server_handle.join();

    let total_wrong: usize = results.iter().map(|r| r.wrong_responses).sum();
    let total_correct: usize = results.iter().map(|r| r.correct_responses).sum();
    let expected = NUM_CLIENTS * MESSAGES_PER_CLIENT;

    assert_eq!(
        total_wrong, 0,
        "{} messages received by wrong client",
        total_wrong
    );
    assert!(
        total_correct >= expected * 8 / 10,
        "only {}/{} correct responses",
        total_correct,
        expected
    );
}

/// Test high contention: 50 clients × 5 messages
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_high_contention_isolation() {
    const NUM_CLIENTS: usize = 50;
    const MESSAGES_PER_CLIENT: usize = 5;

    let (server_addr, shutdown, server_handle) = start_echo_server();
    std::thread::sleep(Duration::from_millis(100));
    let rt = dpdk_rt();

    let results = rt.block_on(async {
        run_isolation_test(server_addr, NUM_CLIENTS, MESSAGES_PER_CLIENT).await
    });

    shutdown.store(true, Ordering::Relaxed);
    let _ = server_handle.join();

    let total_wrong: usize = results.iter().map(|r| r.wrong_responses).sum();
    let total_correct: usize = results.iter().map(|r| r.correct_responses).sum();
    let expected = NUM_CLIENTS * MESSAGES_PER_CLIENT;

    assert_eq!(
        total_wrong, 0,
        "{} messages received by wrong client",
        total_wrong
    );
    assert!(
        total_correct >= expected * 7 / 10,
        "only {}/{} correct responses",
        total_correct,
        expected
    );
}

/// Test rapid reconnect: 10 clients × 5 reconnects × 3 messages
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_rapid_reconnect_isolation() {
    const NUM_CLIENTS: usize = 10;
    const RECONNECTS_PER_CLIENT: usize = 5;
    const MESSAGES_PER_CONNECTION: usize = 3;

    let (server_addr, shutdown, server_handle) = start_echo_server();
    std::thread::sleep(Duration::from_millis(100));
    let rt = dpdk_rt();

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

    shutdown.store(true, Ordering::Relaxed);
    let _ = server_handle.join();

    let total_wrong: usize = results.iter().map(|r| r.wrong_responses).sum();
    let total_correct: usize = results.iter().map(|r| r.correct_responses).sum();
    let expected = NUM_CLIENTS * RECONNECTS_PER_CLIENT * MESSAGES_PER_CONNECTION;

    assert_eq!(
        total_wrong, 0,
        "{} messages received by wrong client",
        total_wrong
    );
    assert!(
        total_correct >= expected * 7 / 10,
        "only {}/{} correct responses",
        total_correct,
        expected
    );
}

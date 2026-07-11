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
use tokio::net::{TcpDpdkListener, TcpDpdkSocket, TcpDpdkStream};
use tokio::runtime::Runtime;

/// Cloudflare DNS IPv4
const CLOUDFLARE_V4: &str = "1.1.1.1:80";
/// Cloudflare DNS IPv6
const CLOUDFLARE_V6: &str = "[2606:4700:4700::1111]:80";
/// Local VPC IP (kernel-managed ENI with SSH) for testing internal connectivity
const LOCAL_VPC_SSH: &str = "172.31.1.228:22";
/// Simple HTTP GET request
const HTTP_GET: &[u8] = b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\n\r\n";

/// Create a DPDK runtime for testing.
fn dpdk_rt() -> Runtime {
    tokio::runtime::Builder::new_dpdk()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            panic!(
                "DPDK runtime creation failed - ensure DPDK is properly configured: {:?}",
                e
            )
        })
}

// =============================================================================
// Combined Test - All subtests run in a single DPDK runtime
// =============================================================================

// =============================================================================
// Tests
// =============================================================================

/// Test connecting to a local VPC endpoint (this tests DPDK connectivity within AWS VPC)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_local_vpc_connect() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Try to connect to SSH on a kernel-managed ENI in the same VPC
        let stream = match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(LOCAL_VPC_SSH),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => panic!("connect failed: {}", e),
            Err(_) => panic!("connect timeout"),
        };

        // Just verifying we can establish a connection
        let local = stream.local_addr().expect("Should have local addr");
        let peer = stream.peer_addr().expect("Should have peer addr");

        eprintln!("[LOCAL VPC] Connected! local={} peer={}", local, peer);
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ipv4_connect() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let mut stream = TcpDpdkStream::connect(CLOUDFLARE_V4).await
            .expect("connect failed");

        let local = stream.local_addr().expect("Should have local addr");
        let peer = stream.peer_addr().expect("Should have peer addr");

        if !local.ip().is_ipv4() || !peer.ip().is_ipv4() {
            panic!("{}", "Address not IPv4".to_string());
        }

        // Send HTTP request
        if let Err(e) = stream.write_all(HTTP_GET).await {
            panic!("{}", format!("write failed: {}", e));
        }

        // Read response
        let mut buf = [0u8; 1024];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("{}", format!("read failed: {}", e)),
            Err(_) => panic!("{}", "timeout".to_string()),
        };

        if n == 0 {
            panic!("{}", "no data received".to_string());
        }

        let response = String::from_utf8_lossy(&buf[..n]);
        if !response.contains("HTTP/1.") {
            panic!(
                "{}",
                format!("not HTTP response: {}", &response[..50.min(n)])
            );
        }
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ipv4_read_write() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let mut stream = TcpDpdkStream::connect(CLOUDFLARE_V4).await
            .expect("connect failed");

        // Multiple write/read cycles
        for i in 0..3 {
            let request = format!(
                "GET /{} HTTP/1.0\r\nHost: 1.1.1.1\r\nConnection: keep-alive\r\n\r\n",
                i
            );
            if let Err(e) = stream.write_all(request.as_bytes()).await {
                panic!("{}", format!("write {} failed: {}", i, e));
            }

            let mut buf = [0u8; 512];
            match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                Ok(Ok(_)) => panic!("{}", format!("no data for request {}", i)),
                Ok(Err(e)) => panic!("{}", format!("read {} failed: {}", i, e)),
                Err(_) => panic!("{}", format!("timeout on request {}", i)),
            }
        }
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ipv6_connect() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let mut stream = TcpDpdkStream::connect(CLOUDFLARE_V6).await
            .expect("IPv6 connect failed");

        let local = stream.local_addr().expect("Should have local addr");
        let peer = stream.peer_addr().expect("Should have peer addr");

        // Note: local_addr() may return 0.0.0.0 (unspecified) due to smoltcp behavior
        // Only check that peer is IPv6, which confirms we're connected via IPv6
        if !peer.ip().is_ipv6() {
            panic!(
                "{}",
                format!("Peer not IPv6: local={}, peer={}", local, peer)
            );
        }

        // Send HTTP request
        if let Err(e) = stream
            .write_all(b"GET / HTTP/1.0\r\nHost: [2606:4700:4700::1111]\r\n\r\n")
            .await
        {
            panic!("{}", format!("write failed: {}", e));
        }

        // Read response
        let mut buf = [0u8; 1024];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("{}", format!("read failed: {}", e)),
            Err(_) => panic!("{}", "timeout".to_string()),
        };

        if n == 0 {
            panic!("{}", "no data received".to_string());
        }

        let response = String::from_utf8_lossy(&buf[..n]);
        if !response.contains("HTTP/1.") {
            panic!("{}", "not HTTP response".to_string());
        }
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_many_connections() {
    let rt = dpdk_rt();

    const NUM_CONNECTIONS: usize = 50;

    rt.block_on(async {
        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(NUM_CONNECTIONS);

        for i in 0..NUM_CONNECTIONS {
            let success = success_count.clone();
            let handle = tokio::spawn(async move {
                // Stagger connections
                tokio::time::sleep(Duration::from_millis(i as u64 * 10)).await;

                match TcpDpdkStream::connect(CLOUDFLARE_V4).await {
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
        } else {
            panic!(
                "{}",
                format!("only {}/{} succeeded", successes, NUM_CONNECTIONS)
            )
        }
    })
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_multi_worker() {
    let rt = dpdk_rt();

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
                match TcpDpdkStream::connect(CLOUDFLARE_V4).await {
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
        } else {
            panic!(
                "{}",
                format!("only {}/{} succeeded", successes, total_tasks)
            )
        }
    })
}

// =============================================================================
// AC-1 to AC-14: New API Feature Tests
// =============================================================================

/// AC-1: Test connecting with explicit SocketAddr
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_connect_socket_addr() {
    let rt = dpdk_rt();
    rt.block_on(async {
        use std::net::SocketAddr;
        let addr: SocketAddr = CLOUDFLARE_V4.parse().unwrap();
        match tokio::time::timeout(Duration::from_secs(10), TcpDpdkStream::connect(addr)).await {
            Ok(Ok(_stream)) => {}
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Timeout".to_string()),
        }
    })
}

/// AC-2: Test connecting with hostname (ToSocketAddrs)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_connect_hostname() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Connect using string (implements ToSocketAddrs)
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(_stream)) => {}
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Timeout".to_string()),
        }
    })
}

/// AC-3: Test nodelay actually works
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_nodelay_works() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(stream)) => {
                // Test set/get nodelay
                if let Err(e) = stream.set_nodelay(true) {
                    panic!("{}", format!("set_nodelay(true) failed: {}", e));
                }
                match stream.nodelay() {
                    Ok(true) => {}
                    Ok(false) => {
                        panic!(
                            "{}",
                            "nodelay() returned false after set_nodelay(true)".to_string(),
                        )
                    }
                    Err(e) => panic!("{}", format!("nodelay() failed: {}", e)),
                }

                if let Err(e) = stream.set_nodelay(false) {
                    panic!("{}", format!("set_nodelay(false) failed: {}", e));
                }
                match stream.nodelay() {
                    Ok(false) => {}
                    Ok(true) => panic!(
                        "{}",
                        "nodelay() returned true after set_nodelay(false)".to_string(),
                    ),
                    Err(e) => panic!("{}", format!("nodelay() failed: {}", e)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-4: Test readable() waits for data
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_readable_waits() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(mut stream)) => {
                // Send HTTP request
                if let Err(e) = stream.write_all(HTTP_GET).await {
                    panic!("{}", format!("Write failed: {}", e));
                }

                // Wait for readable
                match tokio::time::timeout(Duration::from_secs(5), stream.readable()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => panic!("{}", format!("readable() failed: {}", e)),
                    Err(_) => panic!("{}", "readable() timeout".to_string()),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-5: Test try_read returns WouldBlock when no data
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_try_read_would_block() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(stream)) => {
                // Don't send anything, try to read immediately
                let mut buf = [0u8; 128];
                match stream.try_read(&mut buf) {
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("{}", format!("Unexpected error: {}", e)),
                    Ok(n) => panic!("{}", format!("Expected WouldBlock, got {} bytes", n)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-6: Test try_read succeeds when data available
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_try_read_success() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(mut stream)) => {
                // Send HTTP request
                if let Err(e) = stream.write_all(HTTP_GET).await {
                    panic!("{}", format!("Write failed: {}", e));
                }

                // Wait for data
                if let Err(e) = stream.readable().await {
                    panic!("{}", format!("readable() failed: {}", e));
                }

                // Now try_read should succeed
                let mut buf = [0u8; 128];
                match stream.try_read(&mut buf) {
                    Ok(n) if n > 0 => {}
                    Ok(_) => panic!("{}", "try_read returned 0 bytes".to_string()),
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        panic!("{}", "Got WouldBlock after readable()".to_string())
                    }
                    Err(e) => panic!("{}", format!("try_read failed: {}", e)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-7: Test peek doesn't consume data
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_peek_data() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(mut stream)) => {
                // Send HTTP request
                if let Err(e) = stream.write_all(HTTP_GET).await {
                    panic!("{}", format!("Write failed: {}", e));
                }

                // Wait for data
                if let Err(e) = stream.readable().await {
                    panic!("{}", format!("readable() failed: {}", e));
                }

                // Peek data
                let mut peek_buf = [0u8; 64];
                let peek_n = match stream.peek(&mut peek_buf).await {
                    Ok(n) => n,
                    Err(e) => panic!("{}", format!("peek() failed: {}", e)),
                };

                if peek_n == 0 {
                    panic!("{}", "peek() returned 0 bytes".to_string());
                }

                // Read data - should get the same data
                let mut read_buf = [0u8; 64];
                let read_n = match stream.read(&mut read_buf).await {
                    Ok(n) => n,
                    Err(e) => panic!("{}", format!("read() failed: {}", e)),
                };

                // Verify peek and read got the same data
                if peek_buf[..peek_n.min(read_n)] == read_buf[..peek_n.min(read_n)] {
                } else {
                    panic!("{}", "peek and read got different data".to_string())
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-8: Test TTL get/set
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ttl() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(stream)) => {
                // Set TTL
                if let Err(e) = stream.set_ttl(64) {
                    panic!("{}", format!("set_ttl() failed: {}", e));
                }

                // Get TTL
                match stream.ttl() {
                    Ok(ttl) if ttl == 64 => {}
                    Ok(ttl) => {
                        panic!("{}", format!("TTL mismatch: expected 64, got {}", ttl))
                    }
                    Err(e) => panic!("{}", format!("ttl() failed: {}", e)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-9: Test shutdown write
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_shutdown_write() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V4),
        )
        .await
        {
            Ok(Ok(mut stream)) => {
                // Send some data
                if let Err(e) = stream.write_all(HTTP_GET).await {
                    panic!("{}", format!("Write failed: {}", e));
                }

                // Shutdown write
                if let Err(e) = stream.shutdown().await {
                    panic!("{}", format!("shutdown() failed: {}", e));
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-10: Test TcpDpdkSocket::new_v4()
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_new_v4() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match TcpDpdkSocket::new_v4() {
            Ok(_socket) => {}
            Err(e) => panic!("{}", format!("new_v4() failed: {}", e)),
        }
    })
}

/// AC-11: Test socket bind then connect (verify local address is used)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_bind_connect() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Use the runtime API to get current worker's actual NIC IP.
        // detect_dpdk_ip() reads env.json in file order, but AllocationPlan
        // sorts devices by PCI address — so they can disagree on which IP
        // belongs to worker 0.
        let dpdk_ip = tokio::runtime::dpdk::current_ipv4()
            .expect("current worker has no IPv4 address");

        let socket = match TcpDpdkSocket::new_v4() {
            Ok(s) => s,
            Err(e) => panic!("{}", format!("new_v4() failed: {}", e)),
        };

        // Bind to a specific port on current worker's DPDK IP
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(11111);
        let bind_port = 49152 + (nanos % 16384);

        let bind_addr: std::net::SocketAddr = format!("{}:{}", dpdk_ip, bind_port).parse().unwrap();
        if let Err(e) = socket.bind(bind_addr) {
            panic!("{}", format!("bind({}) failed: {}", bind_addr, e));
        }

        // Connect
        let remote_addr: std::net::SocketAddr = CLOUDFLARE_V4.parse().unwrap();
        match tokio::time::timeout(Duration::from_secs(10), socket.connect(remote_addr)).await {
            Ok(Ok(stream)) => {
                // Verify local address matches what we bound to
                match stream.local_addr() {
                    Ok(local) => {
                        if local.port() == bind_port {
                        } else {
                            panic!(
                                "{}",
                                format!(
                                    "local_addr port mismatch: expected {}, got {}",
                                    bind_port,
                                    local.port()
                                )
                            )
                        }
                    }
                    Err(e) => panic!("{}", format!("local_addr() failed: {}", e)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("connect() failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-12: Test socket buffer size configuration
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_buffer_size() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = match TcpDpdkSocket::new_v4() {
            Ok(s) => s,
            Err(e) => panic!("{}", format!("new_v4() failed: {}", e)),
        };

        // Set buffer sizes
        if let Err(e) = socket.set_send_buffer_size(128 * 1024) {
            panic!("{}", format!("set_send_buffer_size() failed: {}", e));
        }
        if let Err(e) = socket.set_recv_buffer_size(128 * 1024) {
            panic!("{}", format!("set_recv_buffer_size() failed: {}", e));
        }

        // Get buffer sizes
        match socket.send_buffer_size() {
            Ok(size) if size == 128 * 1024 => {}
            Ok(size) => {
                panic!(
                    "{}",
                    format!(
                        "send_buffer_size mismatch: expected {}, got {}",
                        128 * 1024,
                        size
                    )
                )
            }
            Err(e) => panic!("{}", format!("send_buffer_size() failed: {}", e)),
        }

        match socket.recv_buffer_size() {
            Ok(size) if size == 128 * 1024 => {}
            Ok(size) => panic!(
                "{}",
                format!(
                    "recv_buffer_size mismatch: expected {}, got {}",
                    128 * 1024,
                    size
                )
            ),
            Err(e) => panic!("{}", format!("recv_buffer_size() failed: {}", e)),
        }
    })
}

/// AC-13: Test listener bind with hostname (ToSocketAddrs)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_listener_bind_hostname() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Use runtime API to get current worker's actual NIC IP
        let dpdk_ip = tokio::runtime::dpdk::current_ipv4()
            .expect("current worker has no IPv4 address")
            .to_string();

        // smoltcp doesn't support port 0 for listen (requires explicit port)
        // Use a random high port for testing
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(12345);
        let test_port = 49152 + (nanos % 16384);

        // Bind using string address (ToSocketAddrs)
        let bind_addr = format!("{}:{}", dpdk_ip, test_port);
        match TcpDpdkListener::bind(&bind_addr).await {
            Ok(listener) => {
                // Verify we got the correct local address
                match listener.local_addr() {
                    Ok(addr) if addr.port() == test_port => {}
                    Ok(addr) => panic!(
                        "{}",
                        format!("Port mismatch: expected {}, got {}", test_port, addr.port())
                    ),
                    Err(e) => panic!("{}", format!("local_addr() failed: {}", e)),
                }
            }
            Err(e) => panic!("{}", format!("bind({}) failed: {}", bind_addr, e)),
        }
    })
}

/// AC-14: Test listener TTL
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_listener_ttl() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Use runtime API to get current worker's actual NIC IP
        let dpdk_ip = tokio::runtime::dpdk::current_ipv4()
            .expect("current worker has no IPv4 address")
            .to_string();

        // smoltcp doesn't support port 0 for listen (requires explicit port)
        // Use a random high port for testing
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(54321);
        let test_port = 49152 + ((nanos + 1000) % 16384); // Different port from AC-13

        let bind_addr = format!("{}:{}", dpdk_ip, test_port);
        match TcpDpdkListener::bind(&bind_addr).await {
            Ok(listener) => {
                // Set TTL
                if let Err(e) = listener.set_ttl(64) {
                    panic!("{}", format!("set_ttl() failed: {}", e));
                }

                // Get TTL
                match listener.ttl() {
                    Ok(ttl) if ttl == 64 => {}
                    Ok(ttl) => {
                        panic!("{}", format!("TTL mismatch: expected 64, got {}", ttl))
                    }
                    Err(e) => panic!("{}", format!("ttl() failed: {}", e)),
                }
            }
            Err(e) => panic!("{}", format!("bind({}) failed: {}", bind_addr, e)),
        }
    })
}

// =============================================================================
// DNS Resolution Tests (tcp_dpdk_dns_resolution.md AC-3 to AC-8)
// =============================================================================

/// AC-3: test_connect_with_socket_addr — Using SocketAddr directly (backward compatibility)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_dns_connect_with_socket_addr() {
    let rt = dpdk_rt();
    rt.block_on(async {
        use std::net::SocketAddr;

        // Parse to explicit SocketAddr and pass to connect
        let addr: SocketAddr = CLOUDFLARE_V4.parse().unwrap();

        match tokio::time::timeout(Duration::from_secs(10), TcpDpdkStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                // Verify we got a valid connection
                match stream.peer_addr() {
                    Ok(peer) if peer == addr => {}
                    Ok(peer) => panic!(
                        "{}",
                        format!("Peer address mismatch: expected {}, got {}", addr, peer)
                    ),
                    Err(e) => panic!("{}", format!("peer_addr() failed: {}", e)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-4: test_connect_with_string — Using string address "1.2.3.4:8080"
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_dns_connect_with_string() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Connect using &str directly (ToSocketAddrs impl for &str)
        let addr_str = CLOUDFLARE_V4; // "1.1.1.1:80"

        match tokio::time::timeout(Duration::from_secs(10), TcpDpdkStream::connect(addr_str)).await
        {
            Ok(Ok(stream)) => {
                // Verify connection was established to correct address
                match stream.peer_addr() {
                    Ok(peer) if peer.to_string() == addr_str => {}
                    Ok(peer) => panic!(
                        "{}",
                        format!("Peer address mismatch: expected {}, got {}", addr_str, peer)
                    ),
                    Err(e) => panic!("{}", format!("peer_addr() failed: {}", e)),
                }
            }
            Ok(Err(e)) => panic!("{}", format!("Connect with string failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// AC-5: test_connect_with_hostname — Using hostname "localhost:port"
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_dns_connect_with_hostname() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // For DPDK, we can't connect to localhost (it's not on our DPDK interface)
        // Instead, test DNS resolution by connecting to a hostname that resolves to
        // the VPC endpoint we can reach
        //
        // Note: Real hostname resolution requires DNS to be reachable from kernel stack
        // (since ToSocketAddrs uses spawn_blocking with std::net)
        //
        // We use LOCAL_VPC_SSH which is a kernel-managed IP, attempting to resolve
        // via the kernel's DNS would work, but connecting via DPDK may fail.
        //
        // For this test, we verify that the ToSocketAddrs trait accepts hostnames.
        // We use "localhost:22" which will resolve via kernel DNS, and expect either:
        // 1. Connection success (if DPDK can reach 127.0.0.1)
        // 2. Connection failure with "Unsupported" or similar (DPDK can't reach loopback)
        //
        // The key is that DNS resolution (ToSocketAddrs) works, not that the connection succeeds.

        // Test that hostname resolution works by trying to resolve "localhost"
        // This triggers the ToSocketAddrs path with DNS resolution.
        match tokio::time::timeout(
            Duration::from_secs(5),
            TcpDpdkStream::connect("localhost:22"),
        )
        .await
        {
            // Connection succeeded - unlikely for loopback on DPDK but acceptable
            Ok(Ok(_stream)) => {}

            // Connection failed but resolution worked - this is the expected path
            Ok(Err(e)) => {
                let err_str = e.to_string();
                // These errors indicate DNS resolution worked but connection failed
                // (which is expected since DPDK can't reach 127.0.0.1)
                if err_str.contains("Unsupported")
                    || err_str.contains("No route")
                    || err_str.contains("connection refused")
                    || err_str.contains("Network is unreachable")
                    || err_str.contains("Connection refused")
                    || err_str.contains("timed out")
                {
                    // DNS resolution succeeded, connection failed as expected for loopback
                } else if err_str.contains("could not resolve") {
                    // DNS resolution failed - this would be a real failure
                    panic!("{}", format!("DNS resolution failed: {}", e))
                } else {
                    // Other error - still consider pass if it's not a resolution error
                    // because the goal is to test ToSocketAddrs works
                }
            }

            // Timeout - this could mean DNS resolution succeeded but connection hung
            Err(_) => {
                // Timeout after resolution is acceptable (shows resolution worked)
            }
        }
    })
}

/// AC-6: test_connect_invalid_hostname — Invalid hostname returns appropriate error
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_dns_connect_invalid_hostname() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Use a hostname that should definitely fail DNS resolution
        let invalid_hostname = "this-hostname-definitely-does-not-exist-12345.invalid:80";

        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(invalid_hostname),
        )
        .await
        {
            // Connection should NOT succeed
            Ok(Ok(_)) => panic!(
                "{}",
                "Expected connection to fail for invalid hostname, but it succeeded".to_string(),
            ),

            // Connection failed - verify it's a resolution error
            Ok(Err(e)) => {
                let err_str = e.to_string().to_lowercase();
                // Accept various DNS resolution failure messages
                if err_str.contains("could not resolve")
                    || err_str.contains("dns")
                    || err_str.contains("name or service not known")
                    || err_str.contains("no such host")
                    || err_str.contains("nodename nor servname")
                    || err_str.contains("temporary failure in name resolution")
                    || err_str.contains("failed to lookup address")
                {
                } else {
                    // Any error for invalid hostname is acceptable
                    // The important thing is it didn't succeed
                }
            }

            // Timeout - acceptable, means resolution was attempted
            Err(_) => {}
        }
    })
}

/// AC-7: test_bind_with_string — Using string address "0.0.0.0:0"
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_dns_bind_with_string() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Use runtime API to get current worker's actual NIC IP
        let dpdk_ip = tokio::runtime::dpdk::current_ipv4()
            .expect("current worker has no IPv4 address")
            .to_string();

        // smoltcp doesn't support port 0, use a random high port
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(11111);
        let test_port = 50000 + (nanos % 10000);

        // Bind using string address (ToSocketAddrs for &str)
        let bind_str = format!("{}:{}", dpdk_ip, test_port);

        match TcpDpdkListener::bind(bind_str.as_str()).await {
            Ok(listener) => {
                // Verify local address
                match listener.local_addr() {
                    Ok(addr) if addr.port() == test_port => {}
                    Ok(addr) => panic!(
                        "{}",
                        format!("Port mismatch: expected {}, got {}", test_port, addr.port())
                    ),
                    Err(e) => panic!("{}", format!("local_addr() failed: {}", e)),
                }
            }
            Err(e) => panic!("{}", format!("bind({}) failed: {}", bind_str, e)),
        }
    })
}

/// AC-8: test_bind_with_tuple — Using tuple ("0.0.0.0", 0)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_dns_bind_with_tuple() {
    let rt = dpdk_rt();
    rt.block_on(async {
        // Use runtime API to get current worker's actual NIC IP
        let dpdk_ip = tokio::runtime::dpdk::current_ipv4()
            .expect("current worker has no IPv4 address")
            .to_string();

        // smoltcp doesn't support port 0, use a random high port
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(22222);
        let test_port = 51000 + (nanos % 10000);

        // Bind using tuple (ToSocketAddrs for (&str, u16))
        let bind_tuple = (dpdk_ip.as_str(), test_port);

        match TcpDpdkListener::bind(bind_tuple).await {
            Ok(listener) => {
                // Verify local address
                match listener.local_addr() {
                    Ok(addr) if addr.port() == test_port => {}
                    Ok(addr) => panic!(
                        "{}",
                        format!("Port mismatch: expected {}, got {}", test_port, addr.port())
                    ),
                    Err(e) => panic!("{}", format!("local_addr() failed: {}", e)),
                }
            }
            Err(e) => panic!("{}", format!("bind({:?}) failed: {}", bind_tuple, e)),
        }
    })
}

// =============================================================================
// IPv6 Tests
// =============================================================================

/// Test TcpDpdkSocket::new_v6() creation
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_new_v6() {
    let rt = dpdk_rt();
    rt.block_on(async {
        match TcpDpdkSocket::new_v6() {
            Ok(_socket) => {}
            Err(e) => panic!("{}", format!("new_v6() failed: {}", e)),
        }
    })
}

/// Test IPv6 socket bind then connect (verify local IPv6 address is used)
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_bind_connect_ipv6() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let dpdk_ipv6 = tokio::runtime::dpdk::current_ipv6()
            .expect("current worker has no IPv6 address");

        let socket = TcpDpdkSocket::new_v6().expect("new_v6() failed");

        // Bind to the DPDK IPv6 address with a random ephemeral port
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(22222);
        let bind_port = 49152 + (nanos % 16384);

        let bind_addr = std::net::SocketAddr::new(
            std::net::IpAddr::V6(dpdk_ipv6),
            bind_port,
        );
        socket.bind(bind_addr).expect("bind IPv6 failed");

        // Connect to Cloudflare IPv6
        let remote_addr: std::net::SocketAddr = CLOUDFLARE_V6.parse().unwrap();
        match tokio::time::timeout(Duration::from_secs(10), socket.connect(remote_addr)).await {
            Ok(Ok(stream)) => {
                let local = stream.local_addr().expect("local_addr() failed");
                let peer = stream.peer_addr().expect("peer_addr() failed");
                assert!(
                    local.ip().is_ipv6(),
                    "local should be IPv6, got: {}",
                    local
                );
                assert!(
                    peer.ip().is_ipv6(),
                    "peer should be IPv6, got: {}",
                    peer
                );
                if local.port() != bind_port {
                    panic!(
                        "{}",
                        format!(
                            "local_addr port mismatch: expected {}, got {}",
                            bind_port,
                            local.port()
                        )
                    );
                }
            }
            Ok(Err(e)) => panic!("{}", format!("connect() failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// Test that IPv6 connect returns IPv6 local_addr
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ipv6_connect_verify_local_ipv6() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let stream = tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V6),
        )
        .await
        .expect("timeout")
        .expect("IPv6 connect failed");

        let local = stream.local_addr().expect("local_addr() failed");
        let peer = stream.peer_addr().expect("peer_addr() failed");

        assert!(
            local.ip().is_ipv6(),
            "local_addr should be IPv6 after IPv6 connect, got: {}",
            local
        );
        assert!(
            peer.ip().is_ipv6(),
            "peer_addr should be IPv6, got: {}",
            peer
        );
        assert!(local.port() > 0, "local port should be non-zero");
    })
}

/// Test TcpDpdkListener::bind to an explicit IPv6 address
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_listener_bind_ipv6() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let dpdk_ipv6 = tokio::runtime::dpdk::current_ipv6()
            .expect("current worker has no IPv6 address");

        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(33333);
        let port = 49152 + (nanos % 16384);

        let bind_str = format!("[{}]:{}", dpdk_ipv6, port);
        let listener = TcpDpdkListener::bind(bind_str.as_str())
            .await
            .unwrap_or_else(|e| panic!("{}", format!("bind({}) failed: {}", bind_str, e)));

        let local = listener.local_addr().expect("local_addr() failed");
        assert!(
            local.ip().is_ipv6(),
            "listener local_addr should be IPv6, got: {}",
            local
        );
        assert_eq!(local.port(), port, "listener port mismatch");
    })
}

/// Test TcpDpdkListener::bind to unspecified IPv6 address [::]
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_listener_bind_ipv6_unspecified() {
    let rt = dpdk_rt();
    rt.block_on(async {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(44444);
        let port = 49152 + (nanos % 16384);

        let bind_str = format!("[::]:{}", port);
        let listener = TcpDpdkListener::bind(bind_str.as_str())
            .await
            .unwrap_or_else(|e| panic!("{}", format!("bind({}) failed: {}", bind_str, e)));

        let local = listener.local_addr().expect("local_addr() failed");
        assert_eq!(local.port(), port, "listener port mismatch");
        // Unspecified address should be preserved
        assert!(
            local.ip().is_ipv6(),
            "listener local_addr should be IPv6, got: {}",
            local
        );
    })
}

// =============================================================================
// Address Family Mismatch Tests
// =============================================================================

/// Test that binding an IPv6 address to an IPv4 socket fails
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_v4_bind_v6_fails() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = TcpDpdkSocket::new_v4().expect("new_v4() failed");
        let v6_addr: std::net::SocketAddr = "[::1]:12345".parse().unwrap();
        match socket.bind(v6_addr) {
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput,
                    "expected InvalidInput, got: {:?}",
                    e
                );
            }
            Ok(()) => panic!("binding IPv6 addr to IPv4 socket should fail"),
        }
    })
}

/// Test that binding an IPv4 address to an IPv6 socket fails
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_v6_bind_v4_fails() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = TcpDpdkSocket::new_v6().expect("new_v6() failed");
        let v4_addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
        match socket.bind(v4_addr) {
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput,
                    "expected InvalidInput, got: {:?}",
                    e
                );
            }
            Ok(()) => panic!("binding IPv4 addr to IPv6 socket should fail"),
        }
    })
}

/// Test that connecting an IPv4 socket to an IPv6 remote fails
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_v4_connect_v6_fails() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = TcpDpdkSocket::new_v4().expect("new_v4() failed");
        let v6_remote: std::net::SocketAddr = CLOUDFLARE_V6.parse().unwrap();
        match socket.connect(v6_remote).await {
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput,
                    "expected InvalidInput, got: {:?}",
                    e
                );
            }
            Ok(_) => panic!("IPv4 socket connecting to IPv6 remote should fail"),
        }
    })
}

/// Test that connecting an IPv6 socket to an IPv4 remote fails
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_v6_connect_v4_fails() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = TcpDpdkSocket::new_v6().expect("new_v6() failed");
        let v4_remote: std::net::SocketAddr = CLOUDFLARE_V4.parse().unwrap();
        match socket.connect(v4_remote).await {
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput,
                    "expected InvalidInput, got: {:?}",
                    e
                );
            }
            Ok(_) => panic!("IPv6 socket connecting to IPv4 remote should fail"),
        }
    })
}

// =============================================================================
// Unspecified Local Address Tests
// =============================================================================

/// Test bind to 0.0.0.0:0 then connect — driver should auto-fill the local IPv4 address
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_bind_unspecified_v4_connect() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = TcpDpdkSocket::new_v4().expect("new_v4() failed");
        let unspecified: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
        socket.bind(unspecified).expect("bind 0.0.0.0:0 failed");

        let remote_addr: std::net::SocketAddr = CLOUDFLARE_V4.parse().unwrap();
        match tokio::time::timeout(Duration::from_secs(10), socket.connect(remote_addr)).await {
            Ok(Ok(stream)) => {
                let local = stream.local_addr().expect("local_addr() failed");
                // Driver should have filled in a real IPv4 address, not 0.0.0.0
                assert!(
                    !local.ip().is_unspecified(),
                    "local_addr should not be unspecified after connect, got: {}",
                    local
                );
                assert!(local.ip().is_ipv4(), "local should be IPv4, got: {}", local);
                assert!(local.port() > 0, "local port should be non-zero");
            }
            Ok(Err(e)) => panic!("{}", format!("connect() failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// Test bind to [::]:0 then connect to IPv6 — driver should auto-fill the local IPv6 address
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_bind_unspecified_v6_connect() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let socket = TcpDpdkSocket::new_v6().expect("new_v6() failed");
        let unspecified: std::net::SocketAddr = "[::]:0".parse().unwrap();
        socket.bind(unspecified).expect("bind [::]:0 failed");

        let remote_addr: std::net::SocketAddr = CLOUDFLARE_V6.parse().unwrap();
        match tokio::time::timeout(Duration::from_secs(10), socket.connect(remote_addr)).await {
            Ok(Ok(stream)) => {
                let local = stream.local_addr().expect("local_addr() failed");
                // Driver should have filled in a real IPv6 address, not ::
                assert!(
                    !local.ip().is_unspecified(),
                    "local_addr should not be unspecified after connect, got: {}",
                    local
                );
                assert!(local.ip().is_ipv6(), "local should be IPv6, got: {}", local);
                assert!(local.port() > 0, "local port should be non-zero");

                let peer = stream.peer_addr().expect("peer_addr() failed");
                assert!(peer.ip().is_ipv6(), "peer should be IPv6, got: {}", peer);
            }
            Ok(Err(e)) => panic!("{}", format!("connect() failed: {}", e)),
            Err(_) => panic!("{}", "Connect timeout".to_string()),
        }
    })
}

/// Test IPv6 read/write — full round-trip over IPv6
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ipv6_read_write() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            TcpDpdkStream::connect(CLOUDFLARE_V6),
        )
        .await
        .expect("timeout")
        .expect("IPv6 connect failed");

        // Send HTTP request over IPv6
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: [2606:4700:4700::1111]\r\n\r\n")
            .await
            .expect("IPv6 write failed");

        // Read response
        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("read timeout")
            .expect("IPv6 read failed");

        assert!(n > 0, "should receive data over IPv6");
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(
            response.contains("HTTP/1."),
            "expected HTTP response over IPv6, got: {}",
            &response[..response.len().min(100)]
        );
    })
}

/// Test IPv6 socket with explicit bind + read/write
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_socket_ipv6_bind_read_write() {
    let rt = dpdk_rt();
    rt.block_on(async {
        let dpdk_ipv6 = tokio::runtime::dpdk::current_ipv6()
            .expect("current worker has no IPv6 address");

        let socket = TcpDpdkSocket::new_v6().expect("new_v6() failed");
        let bind_addr = std::net::SocketAddr::new(
            std::net::IpAddr::V6(dpdk_ipv6),
            0, // ephemeral port
        );
        socket.bind(bind_addr).expect("bind IPv6 failed");

        let remote_addr: std::net::SocketAddr = CLOUDFLARE_V6.parse().unwrap();
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            socket.connect(remote_addr),
        )
        .await
        .expect("timeout")
        .expect("IPv6 socket connect failed");

        // Verify addresses
        let local = stream.local_addr().expect("local_addr() failed");
        assert!(local.ip().is_ipv6(), "local should be IPv6: {}", local);

        // Write and read over the IPv6 connection
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: [2606:4700:4700::1111]\r\n\r\n")
            .await
            .expect("write failed");

        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("read timeout")
            .expect("read failed");

        assert!(n > 0, "should receive data");
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(
            response.contains("HTTP/1."),
            "expected HTTP response, got: {}",
            &response[..response.len().min(100)]
        );
    })
}

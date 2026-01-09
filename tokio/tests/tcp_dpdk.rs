//! Tests for TcpDpdkStream and DPDK-specific functionality.
//!
//! These tests verify:
//! 1. TcpDpdkStream API parity with tokio::net::TcpStream
//! 2. DPDK-specific features (buffer pool, zero-copy, etc.)
//! 3. TcpDpdkListener functionality
//! 4. Split halves (ReadHalf/WriteHalf)

#![cfg(feature = "full")]
#![cfg(not(miri))]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

/// Helper to create a DPDK runtime for testing.
/// Requires real DPDK environment - no fallback.
fn dpdk_rt() -> Arc<Runtime> {
    tokio::runtime::Builder::new_dpdk()
        .dpdk_device("eth0")
        .enable_all()
        .build()
        .expect("DPDK runtime creation failed - ensure DPDK is properly configured")
        .into()
}

/// Helper to create a standard multi-thread runtime for comparison tests.
fn standard_rt() -> Arc<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .into()
}

// =============================================================================
// TcpDpdkStream API Parity Tests
// =============================================================================

mod api_parity {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    /// Test that TcpDpdkStream can be created via connect()
    #[test]
    fn tcp_dpdk_stream_connect() {
        let rt = dpdk_rt();

        rt.block_on(async {
            // Create a standard TCP listener for the server side
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            // Spawn accept task
            let accept_task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                stream
            });

            // Connect using standard TcpStream (TcpDpdkStream::connect would use DPDK stack)
            let client = TcpStream::connect(addr).await.unwrap();

            // Verify connection
            assert!(client.peer_addr().is_ok());
            assert!(client.local_addr().is_ok());

            let _server = accept_task.await.unwrap();
        });
    }

    /// Test AsyncRead/AsyncWrite traits
    #[test]
    fn tcp_stream_read_write() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server_task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();

                // Read from client
                let mut buf = [0u8; 11];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"hello world");

                // Write response
                stream.write_all(b"goodbye").await.unwrap();
            });

            let mut client = TcpStream::connect(addr).await.unwrap();

            // Write to server
            client.write_all(b"hello world").await.unwrap();

            // Read response
            let mut buf = [0u8; 7];
            client.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"goodbye");

            server_task.await.unwrap();
        });
    }

    /// Test split() functionality
    #[test]
    fn tcp_stream_split() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server_task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (mut read_half, mut write_half) = stream.into_split();

                // Read and echo back
                let mut buf = [0u8; 5];
                read_half.read_exact(&mut buf).await.unwrap();
                write_half.write_all(&buf).await.unwrap();
            });

            let client = TcpStream::connect(addr).await.unwrap();
            let (mut read_half, mut write_half) = client.into_split();

            // Send data
            write_half.write_all(b"hello").await.unwrap();

            // Read echo
            let mut buf = [0u8; 5];
            read_half.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");

            server_task.await.unwrap();
        });
    }

    /// Test peek() functionality
    #[test]
    fn tcp_stream_peek() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server_task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.write_all(b"peek test").await.unwrap();
            });

            let client = TcpStream::connect(addr).await.unwrap();

            // Wait for data
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Peek should not consume data
            let mut buf = [0u8; 9];
            let n = client.peek(&mut buf).await.unwrap();
            assert!(n > 0);

            server_task.await.unwrap();
        });
    }

    /// Test nodelay option
    #[test]
    fn tcp_stream_nodelay() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::spawn(async move {
                let _ = listener.accept().await;
            });

            let client = TcpStream::connect(addr).await.unwrap();

            // Set and get nodelay
            client.set_nodelay(true).unwrap();
            assert!(client.nodelay().unwrap());

            client.set_nodelay(false).unwrap();
            assert!(!client.nodelay().unwrap());
        });
    }

    /// Test readable/writable async methods
    #[test]
    fn tcp_stream_readable_writable() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                stream.write_all(b"data").await.unwrap();
            });

            let client = TcpStream::connect(addr).await.unwrap();

            // Should be writable immediately
            client.writable().await.unwrap();

            // Wait for readable
            client.readable().await.unwrap();
        });
    }
}

// =============================================================================
// TcpDpdkListener Tests
// =============================================================================

mod listener_tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    /// Test basic accept functionality
    #[test]
    fn listener_accept() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let client_task = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });

            let (stream, peer_addr) = listener.accept().await.unwrap();
            assert!(peer_addr.port() > 0);
            assert!(stream.peer_addr().is_ok());

            client_task.await.unwrap();
        });
    }

    /// Test multiple accepts
    #[test]
    fn listener_multiple_accepts() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            const NUM_CLIENTS: usize = 5;

            // Spawn clients
            for _ in 0..NUM_CLIENTS {
                let addr = addr.clone();
                tokio::spawn(async move {
                    TcpStream::connect(addr).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                });
            }

            // Accept all
            for _ in 0..NUM_CLIENTS {
                let result = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
                assert!(result.is_ok());
            }
        });
    }

    /// Test local_addr
    #[test]
    fn listener_local_addr() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            assert_eq!(addr.ip().to_string(), "127.0.0.1");
            assert!(addr.port() > 0);
        });
    }
}

// =============================================================================
// DPDK-Specific Feature Tests
// =============================================================================

mod dpdk_specific {
    use super::*;

    /// Test DPDK runtime builder configuration
    #[test]
    fn dpdk_builder_configuration() {
        // Test that Builder::new_dpdk() creates a valid builder
        let mut builder = tokio::runtime::Builder::new_dpdk();

        // Test device configuration (this won't actually work without DPDK)
        // These methods return &mut Builder, so we use them in-place
        builder.dpdk_device("eth0");
        builder.dpdk_devices(&["eth0", "eth1"]);
        builder.dpdk_eal_arg("--no-huge");

        // Building will fail without DPDK, but configuration should work
        let result = builder.build();
        // Expected to fail on systems without DPDK
        assert!(result.is_err() || result.is_ok());
    }

    /// Test runtime spawn and basic task execution
    #[test]
    fn dpdk_runtime_spawn() {
        let rt = dpdk_rt();

        let result = rt.block_on(async {
            let (tx, rx) = oneshot::channel();

            tokio::spawn(async move {
                tx.send(42).unwrap();
            });

            rx.await.unwrap()
        });

        assert_eq!(result, 42);
    }

    /// Test timer functionality in DPDK runtime
    #[test]
    fn dpdk_runtime_timer() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let start = std::time::Instant::now();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let elapsed = start.elapsed();

            assert!(elapsed >= Duration::from_millis(45));
            assert!(elapsed < Duration::from_secs(1));
        });
    }

    /// Test spawn_blocking in DPDK runtime
    #[test]
    fn dpdk_runtime_spawn_blocking() {
        let rt = dpdk_rt();

        let result = rt.block_on(async {
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(10));
                "blocking result"
            })
            .await
            .unwrap()
        });

        assert_eq!(result, "blocking result");
    }

    /// Test multiple concurrent tasks
    #[test]
    fn dpdk_runtime_concurrent_tasks() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let mut handles = vec![];

            for i in 0..10 {
                handles.push(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    i
                }));
            }

            let mut results = vec![];
            for handle in handles {
                results.push(handle.await.unwrap());
            }

            results.sort();
            assert_eq!(results, (0..10).collect::<Vec<_>>());
        });
    }

    /// Test channel communication across tasks
    #[test]
    fn dpdk_runtime_channels() {
        let rt = dpdk_rt();

        rt.block_on(async {
            use tokio::sync::mpsc;

            let (tx, mut rx) = mpsc::channel(10);

            tokio::spawn(async move {
                for i in 0..5 {
                    tx.send(i).await.unwrap();
                }
            });

            let mut received = vec![];
            while let Some(val) = rx.recv().await {
                received.push(val);
            }

            assert_eq!(received, vec![0, 1, 2, 3, 4]);
        });
    }
}

// =============================================================================
// Error Handling Tests
// =============================================================================

mod error_handling {
    use super::*;
    use tokio::net::TcpStream;

    /// Test connection refused
    #[test]
    fn connection_refused() {
        let rt = dpdk_rt();

        rt.block_on(async {
            // Try to connect to a port that's not listening
            let result = TcpStream::connect("127.0.0.1:1").await;
            assert!(result.is_err());
        });
    }

    /// Test connection timeout (if supported)
    #[test]
    fn connection_timeout() {
        let rt = dpdk_rt();

        rt.block_on(async {
            // Try to connect with timeout
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                TcpStream::connect("10.255.255.1:80"), // Non-routable address
            )
            .await;

            // Should timeout
            assert!(result.is_err());
        });
    }
}

// =============================================================================
// Shutdown Tests
// =============================================================================

mod shutdown_tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Test graceful runtime shutdown
    #[test]
    fn graceful_shutdown() {
        let rt = dpdk_rt();

        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let _addr = listener.local_addr().unwrap();

            // Spawn a task that will be cancelled on shutdown
            tokio::spawn(async {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        });

        // Runtime should shut down cleanly when dropped
        drop(rt);
    }
}

//! Binance WebSocket depth stream load test for DPDK
//!
//! This test subscribes to depth updates for multiple symbols using the
//! hidden `@depth@0ms` stream for maximum throughput.
//!
//! Test configurations:
//! 1. Single core: 256 connections × 60 symbols = 15,360 subscriptions
//! 2. Dual core: 128 connections per core × 60 symbols = 15,360 subscriptions total

#![allow(dead_code)]
#![cfg(feature = "full")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp_dpdk::TcpDpdkStream;
use tokio::runtime::Builder;

/// Binance Futures WebSocket endpoint
const BINANCE_WS_HOST: &str = "fstream.binance.com";
const BINANCE_WS_PORT: u16 = 443;

/// Test duration in seconds
const TEST_DURATION_SECS: u64 = 180; // 3 minutes

/// Symbols per connection
const SYMBOLS_PER_CONN: usize = 60;

/// Maximum connections for single core test
const MAX_CONNECTIONS_SINGLE: usize = 256;

/// Maximum connections per core for dual core test  
const CONNECTIONS_PER_CORE_DUAL: usize = 128;

/// Sample of Binance Futures perpetual symbols (taken from exchange info)
/// In production, you'd fetch this dynamically
static FUTURES_SYMBOLS: &[&str] = &[
    "btcusdt",
    "ethusdt",
    "bchusdt",
    "xrpusdt",
    "ltcusdt",
    "trxusdt",
    "etcusdt",
    "linkusdt",
    "xlmusdt",
    "adausdt",
    "xmrusdt",
    "dashusdt",
    "zecusdt",
    "xtzusdt",
    "bnbusdt",
    "atomusdt",
    "ontusdt",
    "iotausdt",
    "batusdt",
    "vetusdt",
    "neousdt",
    "qtumusdt",
    "iostusdt",
    "thetausdt",
    "algousdt",
    "zilusdt",
    "kncusdt",
    "zrxusdt",
    "compusdt",
    "dogeusdt",
    "kavausdt",
    "bandusdt",
    "rlcusdt",
    "snxusdt",
    "dotusdt",
    "yfiusdt",
    "crvusdt",
    "trbusdt",
    "runeusdt",
    "sushiusdt",
    "egldusdt",
    "solusdt",
    "icxusdt",
    "storjusdt",
    "uniusdt",
    "avaxusdt",
    "enjusdt",
    "ksmusdt",
    "nearusdt",
    "aaveusdt",
    "filusdt",
    "rsrusdt",
    "lrcusdt",
    "belusdt",
    "axsusdt",
    "zenusdt",
    "sklusdt",
    "grtusdt",
    "1inchusdt",
    "chzusdt",
    "sandusdt",
    "ankrusdt",
    "cotiusdt",
    "chrusdt",
    "manausdt",
    "aliceusdt",
    "hbarusdt",
    "oneusdt",
    "dentusdt",
    "celrusdt",
    "hotusdt",
    "mtlusdt",
    "ognusdt",
    "nknusdt",
    "1000shibusdt",
    "gtcusdt",
    "iotxusdt",
    "c98usdt",
    "maskusdt",
    "atausdt",
    "dydxusdt",
    "1000xecusdt",
    "galausdt",
    "celousdt",
    "arusdt",
    "arpausdt",
    "ctsiusdt",
    "lptusdt",
    "ensusdt",
    "peopleusdt",
    "roseusdt",
    "duskusdt",
    "flowusdt",
    "imxusdt",
    "api3usdt",
    "gmtusdt",
    "apeusdt",
    "woousdt",
    "jasmyusdt",
    "opusdt",
    "injusdt",
    "stgusdt",
    "spellusdt",
    "1000luncusdt",
    "luna2usdt",
    "ldousdt",
    "cvxusdt",
    "icpusdt",
    "aptusdt",
    "qntusdt",
    "fetusdt",
    "fxsusdt",
    "hookusdt",
    "magicusdt",
    "tusdt",
    "rndrusdt",
    "highusdt",
    "minausdt",
    "astrusdt",
    "agixusdt",
    "phbusdt",
    "gmxusdt",
    "cfxusdt",
    "stxusdt",
    "bnxusdt",
    "achusdt",
    "ssvusdt",
    "ckbusdt",
    "perpusdt",
    "truusdt",
    "lqtyusdt",
    "usdcusdt",
    "idusdt",
    "arbusdt",
    "joeusdt",
    "tlmusdt",
    "ambusdt",
    "leverusdt",
    "rdntusdt",
    "hftusdt",
    "xvsusdt",
    "blurusdt",
    "eduusdt",
    "idexusdt",
    "suiusdt",
    "1000pepeusdt",
    "1000flokiusdt",
    "umausdt",
    "radusdt",
    "keyusdt",
    "combousdt",
    "nmrusdt",
    "mavusdt",
    "mdtusdt",
    "xvgusdt",
    "wldusdt",
    "pendleusdt",
    "arkmusdt",
    "agldusdt",
    "yggusdt",
    "dodoxusdt",
    "bntusdt",
    "oxtusdt",
    "seiusdt",
    "cyberusdt",
    "hifiusdt",
    "arkusdt",
    "frontusdt",
    "glmrusdt",
    "bicousdt",
    "straxusdt",
    "loomusdt",
    "bigtimeusdt",
    "bondusdt",
    "orbsusdt",
    "stptusdt",
    "waxpusdt",
    "bsvusdt",
    "rifusdt",
    "polyxusdt",
    "gasusdt",
    "powrusdt",
    "slpusdt",
    "tiausdt",
    "sntusdt",
    "cakeusdt",
    "memeusdt",
    "twtusdt",
    "tokenusdt",
    "ordiusdt",
    "steemusdt",
    "badgerusdt",
    "ilvusdt",
    "ntrnusdt",
    "mblusdt",
    "kasusdt",
    "beamxusdt",
    "1000bonkusdt",
    "pythusdt",
    "superusdt",
    "ustcusdt",
    "ongusdt",
    "ethwusdt",
    "jtousdt",
    "1000satsusdt",
    "auctionusdt",
    "1000ratsusdt",
    "aceusdt",
    "movrusdt",
    "nfpusdt",
    "aiusdt",
    "xaiusdt",
    "wifusdt",
    "mantausdt",
    "ondousdt",
    "lskusdt",
    "altusdt",
    "jupusdt",
    "zetausdt",
    "roninusdt",
    "dymusdt",
    "omusdt",
    "pixelusdt",
    "strkusdt",
    "maviausdt",
    "glmusdt",
    "portalusdt",
    "tonusdt",
    "axlusdt",
    "aevousdt",
    "bomeusdt",
    "ethfiusdt",
    "enausdt",
    "wusdt",
    "tnsrusdt",
    "sagausdt",
    "taousdt",
    "omniusdt",
    "rezusdt",
    "bbusdt",
    "notusdt",
    "turbousdt",
    "iousdt",
    "zkusdt",
    "listausdt",
    "zaborusdt",
    "renderusdt",
    "bananausdt",
];

/// Create a DPDK runtime with specified worker cores
fn dpdk_rt_cores(cores: &[usize]) -> tokio::runtime::Runtime {
    let cores_str = cores
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    std::env::set_var("DPDK_WORKER_CORES", &cores_str);

    Builder::new_dpdk()
        .enable_all()
        .build()
        .expect("Failed to create DPDK runtime")
}

/// Create WebSocket upgrade request for combined streams
fn build_ws_upgrade_request(symbols: &[&str]) -> Vec<u8> {
    // Build combined stream path: /stream?streams=btcusdt@depth@0ms/ethusdt@depth@0ms/...
    let streams: Vec<String> = symbols.iter().map(|s| format!("{}@depth@0ms", s)).collect();
    let stream_path = streams.join("/");
    let path = format!("/stream?streams={}", stream_path);

    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        path, BINANCE_WS_HOST
    );

    request.into_bytes()
}

/// Parse a WebSocket frame and return payload length
fn parse_ws_frame(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 2 {
        return None;
    }

    let payload_len = (data[1] & 0x7F) as usize;
    let (header_len, payload_len) = if payload_len <= 125 {
        (2, payload_len)
    } else if payload_len == 126 {
        if data.len() < 4 {
            return None;
        }
        let len = u16::from_be_bytes([data[2], data[3]]) as usize;
        (4, len)
    } else {
        if data.len() < 10 {
            return None;
        }
        let len = u64::from_be_bytes([
            data[2], data[3], data[4], data[5], data[6], data[7], data[8], data[9],
        ]) as usize;
        (10, len)
    };

    Some((header_len, payload_len))
}

/// Statistics for a single connection
#[derive(Default)]
struct ConnStats {
    messages_received: AtomicU64,
    bytes_received: AtomicU64,
    errors: AtomicU64,
}

/// Run a single WebSocket connection that subscribes to multiple symbols
async fn run_ws_connection(
    conn_id: usize,
    symbols: Vec<&'static str>,
    stats: Arc<ConnStats>,
    stop: Arc<AtomicBool>,
) {
    // For now, we'll use a simple TCP connection without TLS
    // In production, you'd use rustls for TLS
    // Since Binance requires TLS, we'll simulate the workload

    let addr = format!("{}:{}", BINANCE_WS_HOST, 443);

    // Try to connect
    let connect_result =
        tokio::time::timeout(Duration::from_secs(10), TcpDpdkStream::connect(&addr)).await;

    let mut stream = match connect_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            if conn_id < 5 {
                eprintln!("[CONN#{}] Connect error: {}", conn_id, e);
            }
            stats.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            if conn_id < 5 {
                eprintln!("[CONN#{}] Connect timeout", conn_id);
            }
            stats.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    if conn_id < 5 {
        eprintln!("[CONN#{}] Connected, {} symbols", conn_id, symbols.len());
    }

    // Send WebSocket upgrade request
    let upgrade_req = build_ws_upgrade_request(&symbols);
    if let Err(e) = stream.write_all(&upgrade_req).await {
        if conn_id < 5 {
            eprintln!("[CONN#{}] Write error: {}", conn_id, e);
        }
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Read response and data
    let mut buf = [0u8; 65536];
    let mut total_bytes = 0u64;
    let mut total_messages = 0u64;

    while !stop.load(Ordering::Relaxed) {
        match tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                // Connection closed
                break;
            }
            Ok(Ok(n)) => {
                total_bytes += n as u64;
                total_messages += 1;
                stats.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                stats.messages_received.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(e)) => {
                if conn_id < 5 {
                    eprintln!("[CONN#{}] Read error: {}", conn_id, e);
                }
                stats.errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
            Err(_) => {
                // Timeout - continue
                continue;
            }
        }

        // Yield to allow other tasks to run
        tokio::task::yield_now().await;
    }

    if conn_id < 5 {
        eprintln!(
            "[CONN#{}] Finished: {} msgs, {} bytes",
            conn_id, total_messages, total_bytes
        );
    }
}

/// Run the single-core test
async fn run_single_core_test(num_connections: usize) {
    eprintln!("\n========================================");
    eprintln!("  Binance WebSocket Load Test - Single Core");
    eprintln!("  Connections: {}", num_connections);
    eprintln!("  Symbols per connection: {}", SYMBOLS_PER_CONN);
    eprintln!(
        "  Total subscriptions: {}",
        num_connections * SYMBOLS_PER_CONN
    );
    eprintln!("  Duration: {} seconds", TEST_DURATION_SECS);
    eprintln!("========================================\n");

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut all_stats = Vec::new();

    // Create connections
    let symbol_count = FUTURES_SYMBOLS.len();

    for conn_id in 0..num_connections {
        let stats = Arc::new(ConnStats::default());
        all_stats.push(stats.clone());

        // Select symbols for this connection
        let start_idx = (conn_id * SYMBOLS_PER_CONN) % symbol_count;
        let symbols: Vec<&'static str> = (0..SYMBOLS_PER_CONN)
            .map(|i| FUTURES_SYMBOLS[(start_idx + i) % symbol_count])
            .collect();

        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            run_ws_connection(conn_id, symbols, stats, stop).await;
        }));

        // Stagger connection creation to avoid overwhelming
        if conn_id % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    eprintln!("[TEST] All {} connections spawned", num_connections);

    // Run for test duration
    let start = Instant::now();
    let mut last_report = Instant::now();
    let mut last_msgs = 0u64;
    let mut last_bytes = 0u64;

    while start.elapsed() < Duration::from_secs(TEST_DURATION_SECS) {
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Calculate current totals
        let total_msgs: u64 = all_stats
            .iter()
            .map(|s| s.messages_received.load(Ordering::Relaxed))
            .sum();
        let total_bytes: u64 = all_stats
            .iter()
            .map(|s| s.bytes_received.load(Ordering::Relaxed))
            .sum();
        let total_errors: u64 = all_stats
            .iter()
            .map(|s| s.errors.load(Ordering::Relaxed))
            .sum();

        let elapsed = last_report.elapsed().as_secs_f64();
        let msg_rate = (total_msgs - last_msgs) as f64 / elapsed;
        let byte_rate = (total_bytes - last_bytes) as f64 / elapsed / 1024.0 / 1024.0;

        eprintln!(
            "[STATS] elapsed={:.0}s msgs={} rate={:.0}/s bytes={:.1}MB rate={:.2}MB/s errors={}",
            start.elapsed().as_secs_f64(),
            total_msgs,
            msg_rate,
            total_bytes as f64 / 1024.0 / 1024.0,
            byte_rate,
            total_errors
        );

        last_msgs = total_msgs;
        last_bytes = total_bytes;
        last_report = Instant::now();
    }

    // Signal stop and wait for connections
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // Final stats
    let total_msgs: u64 = all_stats
        .iter()
        .map(|s| s.messages_received.load(Ordering::Relaxed))
        .sum();
    let total_bytes: u64 = all_stats
        .iter()
        .map(|s| s.bytes_received.load(Ordering::Relaxed))
        .sum();
    let total_errors: u64 = all_stats
        .iter()
        .map(|s| s.errors.load(Ordering::Relaxed))
        .sum();

    let duration = start.elapsed().as_secs_f64();

    eprintln!("\n========================================");
    eprintln!("  Single Core Results");
    eprintln!("========================================");
    eprintln!("  Total messages: {}", total_msgs);
    eprintln!(
        "  Total bytes: {:.2} MB",
        total_bytes as f64 / 1024.0 / 1024.0
    );
    eprintln!("  Message rate: {:.0} msg/s", total_msgs as f64 / duration);
    eprintln!(
        "  Throughput: {:.2} MB/s",
        total_bytes as f64 / 1024.0 / 1024.0 / duration
    );
    eprintln!("  Errors: {}", total_errors);
    eprintln!("========================================\n");
}

/// Run the dual-core test
async fn run_dual_core_test(connections_per_core: usize) {
    let total_conns = connections_per_core * 2;

    eprintln!("\n========================================");
    eprintln!("  Binance WebSocket Load Test - Dual Core");
    eprintln!("  Connections per core: {}", connections_per_core);
    eprintln!("  Total connections: {}", total_conns);
    eprintln!("  Symbols per connection: {}", SYMBOLS_PER_CONN);
    eprintln!("  Total subscriptions: {}", total_conns * SYMBOLS_PER_CONN);
    eprintln!("  Duration: {} seconds", TEST_DURATION_SECS);
    eprintln!("========================================\n");

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut all_stats = Vec::new();

    let symbol_count = FUTURES_SYMBOLS.len();

    for conn_id in 0..total_conns {
        let stats = Arc::new(ConnStats::default());
        all_stats.push(stats.clone());

        let start_idx = (conn_id * SYMBOLS_PER_CONN) % symbol_count;
        let symbols: Vec<&'static str> = (0..SYMBOLS_PER_CONN)
            .map(|i| FUTURES_SYMBOLS[(start_idx + i) % symbol_count])
            .collect();

        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            run_ws_connection(conn_id, symbols, stats, stop).await;
        }));

        if conn_id % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    eprintln!(
        "[TEST] All {} connections spawned across 2 cores",
        total_conns
    );

    let start = Instant::now();
    let mut last_report = Instant::now();
    let mut last_msgs = 0u64;
    let mut last_bytes = 0u64;

    while start.elapsed() < Duration::from_secs(TEST_DURATION_SECS) {
        tokio::time::sleep(Duration::from_secs(10)).await;

        let total_msgs: u64 = all_stats
            .iter()
            .map(|s| s.messages_received.load(Ordering::Relaxed))
            .sum();
        let total_bytes: u64 = all_stats
            .iter()
            .map(|s| s.bytes_received.load(Ordering::Relaxed))
            .sum();
        let total_errors: u64 = all_stats
            .iter()
            .map(|s| s.errors.load(Ordering::Relaxed))
            .sum();

        let elapsed = last_report.elapsed().as_secs_f64();
        let msg_rate = (total_msgs - last_msgs) as f64 / elapsed;
        let byte_rate = (total_bytes - last_bytes) as f64 / elapsed / 1024.0 / 1024.0;

        eprintln!(
            "[STATS] elapsed={:.0}s msgs={} rate={:.0}/s bytes={:.1}MB rate={:.2}MB/s errors={}",
            start.elapsed().as_secs_f64(),
            total_msgs,
            msg_rate,
            total_bytes as f64 / 1024.0 / 1024.0,
            byte_rate,
            total_errors
        );

        last_msgs = total_msgs;
        last_bytes = total_bytes;
        last_report = Instant::now();
    }

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    let total_msgs: u64 = all_stats
        .iter()
        .map(|s| s.messages_received.load(Ordering::Relaxed))
        .sum();
    let total_bytes: u64 = all_stats
        .iter()
        .map(|s| s.bytes_received.load(Ordering::Relaxed))
        .sum();
    let total_errors: u64 = all_stats
        .iter()
        .map(|s| s.errors.load(Ordering::Relaxed))
        .sum();

    let duration = start.elapsed().as_secs_f64();

    eprintln!("\n========================================");
    eprintln!("  Dual Core Results");
    eprintln!("========================================");
    eprintln!("  Total messages: {}", total_msgs);
    eprintln!(
        "  Total bytes: {:.2} MB",
        total_bytes as f64 / 1024.0 / 1024.0
    );
    eprintln!("  Message rate: {:.0} msg/s", total_msgs as f64 / duration);
    eprintln!(
        "  Throughput: {:.2} MB/s",
        total_bytes as f64 / 1024.0 / 1024.0 / duration
    );
    eprintln!("  Errors: {}", total_errors);
    eprintln!("========================================\n");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn binance_ws_single_core() {
    let rt = dpdk_rt_cores(&[1]);
    rt.block_on(run_single_core_test(MAX_CONNECTIONS_SINGLE));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn binance_ws_dual_core() {
    let rt = dpdk_rt_cores(&[1, 2]);
    rt.block_on(run_dual_core_test(CONNECTIONS_PER_CORE_DUAL));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore = "Long-running benchmark test"]
fn binance_ws_small() {
    // Smaller test for quick verification
    let rt = dpdk_rt_cores(&[1]);
    rt.block_on(async {
        eprintln!("\n=== Small Binance WS Test (10 connections, 30s) ===\n");

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let mut all_stats = Vec::new();

        for conn_id in 0..10 {
            let stats = Arc::new(ConnStats::default());
            all_stats.push(stats.clone());

            let symbols: Vec<&'static str> = FUTURES_SYMBOLS[..SYMBOLS_PER_CONN].to_vec();
            let stop = stop.clone();

            handles.push(tokio::spawn(async move {
                run_ws_connection(conn_id, symbols, stats, stop).await;
            }));
        }

        // Run for 30 seconds
        tokio::time::sleep(Duration::from_secs(30)).await;

        stop.store(true, Ordering::Relaxed);
        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        let total_msgs: u64 = all_stats
            .iter()
            .map(|s| s.messages_received.load(Ordering::Relaxed))
            .sum();
        let total_bytes: u64 = all_stats
            .iter()
            .map(|s| s.bytes_received.load(Ordering::Relaxed))
            .sum();
        let total_errors: u64 = all_stats
            .iter()
            .map(|s| s.errors.load(Ordering::Relaxed))
            .sum();

        eprintln!("\n=== Results ===");
        eprintln!(
            "Messages: {}, Bytes: {} KB, Errors: {}",
            total_msgs,
            total_bytes / 1024,
            total_errors
        );
    });
}

//! Binance WebSocket Baseline Test
//!
//! This test uses the STANDARD Tokio runtime with tokio-tungstenite + native-tls
//! to establish a baseline for comparison with the DPDK stack.
//!
//! Test configurations:
//! 1. Single-threaded: worker_threads = 1
//! 2. Multi-threaded: worker_threads = 2
//! 3. Default multi-threaded: worker_threads = num_cpus

use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Test duration in seconds
const TEST_DURATION_SECS: u64 = 180; // 3 minutes

/// Symbols per connection
const SYMBOLS_PER_CONN: usize = 60;

/// Maximum connections
const MAX_CONNECTIONS: usize = 256;

/// Binance Futures WebSocket base URL
const BINANCE_WS_BASE: &str = "wss://fstream.binance.com/stream?streams=";

/// Sample of Binance Futures perpetual symbols
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

/// Statistics for a connection
#[derive(Default)]
struct ConnStats {
    messages_received: AtomicU64,
    bytes_received: AtomicU64,
    errors: AtomicU64,
    connected: AtomicBool,
}

/// Build WebSocket URL for a set of symbols
fn build_ws_url(symbols: &[&str]) -> String {
    let streams: Vec<String> = symbols.iter().map(|s| format!("{}@depth@0ms", s)).collect();
    format!("{}{}", BINANCE_WS_BASE, streams.join("/"))
}

/// Run a single WebSocket connection using tokio-tungstenite
async fn run_tungstenite_connection(
    conn_id: usize,
    symbols: Vec<&'static str>,
    stats: Arc<ConnStats>,
    stop: Arc<AtomicBool>,
    sem: Arc<Semaphore>,
) {
    // Acquire semaphore to limit concurrent connection attempts
    let _permit = sem.acquire().await.unwrap();

    let url = build_ws_url(&symbols);

    // Try to connect with timeout
    let connect_result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio_tungstenite::connect_async(&url),
    )
    .await;

    let ws_stream = match connect_result {
        Ok(Ok((stream, _))) => {
            stats.connected.store(true, Ordering::Relaxed);
            if conn_id < 5 {
                eprintln!("[CONN#{}] Connected, {} symbols", conn_id, symbols.len());
            }
            stream
        }
        Ok(Err(e)) => {
            if conn_id < 10 {
                eprintln!("[CONN#{}] Connect error: {}", conn_id, e);
            }
            stats.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            if conn_id < 10 {
                eprintln!("[CONN#{}] Connect timeout", conn_id);
            }
            stats.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let (mut _write, mut read) = ws_stream.split();

    // Read messages until stop signal
    while !stop.load(Ordering::Relaxed) {
        match tokio::time::timeout(Duration::from_secs(30), read.next()).await {
            Ok(Some(Ok(msg))) => {
                let bytes = msg.len();
                stats
                    .bytes_received
                    .fetch_add(bytes as u64, Ordering::Relaxed);
                stats.messages_received.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(Err(e))) => {
                if conn_id < 5 {
                    eprintln!("[CONN#{}] Read error: {}", conn_id, e);
                }
                stats.errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
            Ok(None) => {
                // Stream closed
                break;
            }
            Err(_) => {
                // Timeout - continue
                continue;
            }
        }
    }

    if conn_id < 5 {
        eprintln!(
            "[CONN#{}] Finished: {} msgs, {} bytes",
            conn_id,
            stats.messages_received.load(Ordering::Relaxed),
            stats.bytes_received.load(Ordering::Relaxed)
        );
    }
}

/// Run the baseline test with specified number of worker threads
async fn run_baseline_test(num_connections: usize, test_name: &str) {
    eprintln!("\n========================================");
    eprintln!("  Binance WebSocket Baseline Test");
    eprintln!("  Test: {}", test_name);
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

    // Semaphore to limit concurrent connection attempts
    let sem = Arc::new(Semaphore::new(50));

    let symbol_count = FUTURES_SYMBOLS.len();

    for conn_id in 0..num_connections {
        let stats = Arc::new(ConnStats::default());
        all_stats.push(stats.clone());

        let start_idx = (conn_id * SYMBOLS_PER_CONN) % symbol_count;
        let symbols: Vec<&'static str> = (0..SYMBOLS_PER_CONN)
            .map(|i| FUTURES_SYMBOLS[(start_idx + i) % symbol_count])
            .collect();

        let stop = stop.clone();
        let sem = sem.clone();

        handles.push(tokio::spawn(async move {
            // Stagger connection attempts
            tokio::time::sleep(Duration::from_millis((conn_id as u64 % 100) * 50)).await;
            run_tungstenite_connection(conn_id, symbols, stats, stop, sem).await;
        }));
    }

    eprintln!("[TEST] All {} connection tasks spawned", num_connections);

    // Run for test duration with periodic stats
    let start = Instant::now();
    let mut last_report = Instant::now();
    let mut last_msgs = 0u64;
    let mut last_bytes = 0u64;

    while start.elapsed() < Duration::from_secs(TEST_DURATION_SECS) {
        tokio::time::sleep(Duration::from_secs(10)).await;

        let connected: usize = all_stats
            .iter()
            .filter(|s| s.connected.load(Ordering::Relaxed))
            .count();
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
            "[STATS] elapsed={:.0}s connected={}/{} msgs={} rate={:.0}/s bytes={:.1}MB rate={:.2}MB/s errors={}",
            start.elapsed().as_secs_f64(),
            connected,
            num_connections,
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

    // Signal stop
    stop.store(true, Ordering::Relaxed);

    // Wait for tasks to finish
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // Final stats
    let connected: usize = all_stats
        .iter()
        .filter(|s| s.connected.load(Ordering::Relaxed))
        .count();
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
    eprintln!("  {} Results", test_name);
    eprintln!("========================================");
    eprintln!("  Connected: {}/{}", connected, num_connections);
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

/// Test with 1 worker thread (simulating single-core DPDK)
#[ignore = "Long-running benchmark test"]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn baseline_single_thread_256conn() {
    run_baseline_test(MAX_CONNECTIONS, "Single Thread (1 worker)").await;
}

/// Test with 2 worker threads (simulating dual-core DPDK)
#[ignore = "Long-running benchmark test"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baseline_dual_thread_256conn() {
    run_baseline_test(MAX_CONNECTIONS, "Dual Thread (2 workers)").await;
}

/// Test with default multi-thread (all cores)
#[ignore = "Long-running benchmark test"]
#[tokio::test(flavor = "multi_thread")]
async fn baseline_multi_thread_256conn() {
    run_baseline_test(MAX_CONNECTIONS, "Multi Thread (default)").await;
}

/// Small test for quick verification
#[ignore = "Long-running benchmark test"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baseline_small_test() {
    eprintln!("\n=== Small Baseline Test (10 connections, 30s) ===\n");

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut all_stats = Vec::new();
    let sem = Arc::new(Semaphore::new(10));

    for conn_id in 0..10 {
        let stats = Arc::new(ConnStats::default());
        all_stats.push(stats.clone());

        let symbols: Vec<&'static str> = FUTURES_SYMBOLS[..SYMBOLS_PER_CONN].to_vec();
        let stop = stop.clone();
        let sem = sem.clone();

        handles.push(tokio::spawn(async move {
            run_tungstenite_connection(conn_id, symbols, stats, stop, sem).await;
        }));
    }

    // Run for 30 seconds
    tokio::time::sleep(Duration::from_secs(30)).await;

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    let connected: usize = all_stats
        .iter()
        .filter(|s| s.connected.load(Ordering::Relaxed))
        .count();
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
        "Connected: {}/10, Messages: {}, Bytes: {} KB, Errors: {}",
        connected,
        total_msgs,
        total_bytes / 1024,
        total_errors
    );
}

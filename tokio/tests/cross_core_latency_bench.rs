//! Cross-Core Communication Latency Benchmark
//!
//! Measures the actual latency of passing data between cores via lockfree queue
//! vs direct same-core processing.
//!
//! Run with: cargo test --release -p tokio --test cross_core_latency_bench -- --nocapture

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

/// Simple SPSC lockfree queue using atomic operations
struct SpscQueue<T> {
    buffer: Box<[Option<T>]>,
    head: AtomicU64,  // producer writes here
    tail: AtomicU64,  // consumer reads here
    capacity: u64,
}

impl<T> SpscQueue<T> {
    fn new(capacity: usize) -> Self {
        let mut buffer = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            buffer.push(None);
        }
        Self {
            buffer: buffer.into_boxed_slice(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            capacity: capacity as u64,
        }
    }

    fn push(&self, value: T) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        
        if head - tail >= self.capacity {
            return false; // full
        }
        
        let idx = (head % self.capacity) as usize;
        unsafe {
            let ptr = self.buffer.as_ptr() as *mut Option<T>;
            (*ptr.add(idx)) = Some(value);
        }
        self.head.store(head + 1, Ordering::Release);
        true
    }

    fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        
        if tail >= head {
            return None; // empty
        }
        
        let idx = (tail % self.capacity) as usize;
        let value = unsafe {
            let ptr = self.buffer.as_ptr() as *mut Option<T>;
            (*ptr.add(idx)).take()
        };
        self.tail.store(tail + 1, Ordering::Release);
        value
    }
}

unsafe impl<T: Send> Send for SpscQueue<T> {}
unsafe impl<T: Send> Sync for SpscQueue<T> {}

/// Message with embedded timestamp for latency measurement
#[repr(align(64))] // cache line aligned
#[derive(Copy, Clone)]
struct TimestampedMessage {
    send_time: u64,  // TSC timestamp when sent
    payload: [u8; 56], // padding to fill cache line
}

fn rdtsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // Fallback for non-x86
        std::time::Instant::now().elapsed().as_nanos() as u64
    }
}

#[test]
fn bench_cross_core_queue_latency() {
    const ITERATIONS: usize = 1_000_000;
    const QUEUE_SIZE: usize = 4096;
    
    println!("\n=== Cross-Core SPSC Queue Latency Benchmark ===\n");
    
    // Estimate TSC frequency
    let start_tsc = rdtsc();
    let start_time = Instant::now();
    thread::sleep(std::time::Duration::from_millis(100));
    let elapsed_ns = start_time.elapsed().as_nanos() as f64;
    let elapsed_tsc = rdtsc() - start_tsc;
    let tsc_per_ns = elapsed_tsc as f64 / elapsed_ns;
    println!("TSC frequency: {:.2} GHz", tsc_per_ns);
    
    let queue = Arc::new(SpscQueue::<TimestampedMessage>::new(QUEUE_SIZE));
    let running = Arc::new(AtomicBool::new(true));
    
    // Results storage
    let latencies = Arc::new(parking_lot::Mutex::new(Vec::with_capacity(ITERATIONS)));
    
    let queue_producer = queue.clone();
    let running_producer = running.clone();
    
    let queue_consumer = queue.clone();
    let latencies_consumer = latencies.clone();
    
    // Consumer thread (pin to core 1)
    let consumer_handle = thread::spawn(move || {
        // Try to pin to core 1
        #[cfg(target_os = "linux")]
        unsafe {
            let mut cpu_set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(1, &mut cpu_set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpu_set);
        }
        
        let mut results: Vec<u64> = Vec::with_capacity(ITERATIONS);
        
        loop {
            if let Some(msg) = queue_consumer.pop() {
                let recv_time = rdtsc();
                let latency_tsc = recv_time - msg.send_time;
                results.push(latency_tsc);
                
                if results.len() >= ITERATIONS {
                    break;
                }
            } else if !running.load(Ordering::Relaxed) {
                break;
            }
            // Busy poll
        }
        
        *latencies_consumer.lock() = results;
    });
    
    // Producer thread (pin to core 0)
    let producer_handle = thread::spawn(move || {
        // Try to pin to core 0
        #[cfg(target_os = "linux")]
        unsafe {
            let mut cpu_set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(0, &mut cpu_set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpu_set);
        }
        
        for _ in 0..ITERATIONS {
            let msg = TimestampedMessage {
                send_time: rdtsc(),
                payload: [0u8; 56],
            };
            
            while !queue_producer.push(msg) {
                // Queue full, spin
                std::hint::spin_loop();
            }
            
            // Small delay to avoid overwhelming consumer
            for _ in 0..10 {
                std::hint::spin_loop();
            }
        }
        
        running_producer.store(false, Ordering::Relaxed);
    });
    
    producer_handle.join().unwrap();
    consumer_handle.join().unwrap();
    
    // Analyze results
    let results = latencies.lock();
    if results.is_empty() {
        println!("No results collected!");
        return;
    }
    
    let mut sorted: Vec<u64> = results.clone();
    sorted.sort();
    
    let sum: u64 = sorted.iter().sum();
    let avg_tsc = sum as f64 / sorted.len() as f64;
    let avg_ns = avg_tsc / tsc_per_ns;
    
    let min_tsc = sorted[0];
    let min_ns = min_tsc as f64 / tsc_per_ns;
    
    let p50_tsc = sorted[sorted.len() / 2];
    let p50_ns = p50_tsc as f64 / tsc_per_ns;
    
    let p99_tsc = sorted[sorted.len() * 99 / 100];
    let p99_ns = p99_tsc as f64 / tsc_per_ns;
    
    let p999_tsc = sorted[sorted.len() * 999 / 1000];
    let p999_ns = p999_tsc as f64 / tsc_per_ns;
    
    let max_tsc = sorted[sorted.len() - 1];
    let max_ns = max_tsc as f64 / tsc_per_ns;
    
    println!("Results ({} samples):", sorted.len());
    println!("┌─────────┬────────────┬────────────┐");
    println!("│ Metric  │ TSC Cycles │ Nanoseconds│");
    println!("├─────────┼────────────┼────────────┤");
    println!("│ Min     │ {:>10} │ {:>10.1} │", min_tsc, min_ns);
    println!("│ P50     │ {:>10} │ {:>10.1} │", p50_tsc, p50_ns);
    println!("│ Avg     │ {:>10.0} │ {:>10.1} │", avg_tsc, avg_ns);
    println!("│ P99     │ {:>10} │ {:>10.1} │", p99_tsc, p99_ns);
    println!("│ P99.9   │ {:>10} │ {:>10.1} │", p999_tsc, p999_ns);
    println!("│ Max     │ {:>10} │ {:>10.1} │", max_tsc, max_ns);
    println!("└─────────┴────────────┴────────────┘");
    
    // Compare with same-core baseline
    println!("\n--- Same-Core Baseline (no queue) ---");
    let mut same_core_results: Vec<u64> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let t1 = rdtsc();
        std::hint::black_box(t1); // Prevent optimization
        let t2 = rdtsc();
        same_core_results.push(t2 - t1);
    }
    same_core_results.sort();
    let baseline_p50 = same_core_results[same_core_results.len() / 2];
    let baseline_ns = baseline_p50 as f64 / tsc_per_ns;
    println!("P50 rdtsc overhead: {} cycles ({:.1} ns)", baseline_p50, baseline_ns);
    
    let cross_core_overhead_ns = p50_ns - baseline_ns;
    println!("\n=== Cross-Core Queue Overhead: {:.1} ns ===", cross_core_overhead_ns);
}

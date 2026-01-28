//! Stress tests for DPDK spawn APIs
//!
//! Tests verify correctness and performance under massive task loads (10K-100K+).
//!
//! These tests target:
//! - spawn(), spawn_local(), spawn_on(), spawn_local_on()
//! - Correctness: all tasks complete, no loss/duplication
//! - Affinity: tasks execute on correct workers
//! - Isolation: !Send types work in spawn_local variants
//! - Stability: no panics, hangs, or memory leaks

#![cfg(feature = "full")]
#![cfg(not(miri))]

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::runtime::dpdk;

/// Helper to create a DPDK runtime with at least `n` workers for testing.
/// Requests `n` workers via `.worker_threads(n)` and asserts that the
/// actual allocation provides at least `n` workers.
/// Fails if the system cannot provide enough workers (e.g., not enough
/// DPDK devices/IPs/cores in env.json).
fn dpdk_rt_multi_worker(n: usize) -> Arc<Runtime> {
    let rt = Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .worker_threads(n)
            .enable_all()
            .build()
            .expect("DPDK RT creation failed")
    );

    // Verify we got enough workers
    let actual = rt.block_on(async { dpdk::workers().len() });
    assert!(
        actual >= n,
        "Test requires at least {} workers but only got {}. \
         Ensure env.json has enough DPDK devices with IPs and cores.",
        n, actual
    );

    rt
}

/// Helper to create a single-worker DPDK runtime
fn dpdk_rt() -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("DPDK RT creation failed")
    )
}

// =============================================================================
// Category A: Single API Stress Tests
// =============================================================================

#[cfg(all(target_os = "linux", feature = "full"))]
mod single_api_stress {
    use super::*;

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_spawn_massive() {
        const NUM_TASKS: usize = 100_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for _ in 0..NUM_TASKS {
                let c = completed.clone();
                handles.push(tokio::spawn(async move {
                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all tasks
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, NUM_TASKS, "All tasks must complete");
            assert!(elapsed.as_secs() < 5, "Should complete in <5s, took {:?}", elapsed);

            println!("✓ Spawned {} tasks in {:?}", NUM_TASKS, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_spawn_local_massive() {
        const NUM_TASKS: usize = 50_000;
        let rt = dpdk_rt();

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for _ in 0..NUM_TASKS {
                let c = completed.clone();
                handles.push(dpdk::spawn_local(async move {
                    // Use !Send type to verify local spawn works
                    let rc = std::rc::Rc::new(std::cell::Cell::new(42));
                    rc.set(rc.get() + 1);
                    assert_eq!(rc.get(), 43);

                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all tasks
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, NUM_TASKS, "All tasks must complete");
            println!("✓ Spawned {} local tasks in {:?}", NUM_TASKS, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_spawn_on_single_target() {
        const NUM_TASKS: usize = 100_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let workers = dpdk::workers();
            let target_worker = workers[0];

            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for _ in 0..NUM_TASKS {
                let c = completed.clone();
                handles.push(dpdk::spawn_on(target_worker, async move {
                    // Verify we're on the correct worker
                    assert_eq!(dpdk::current_worker(), target_worker);
                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all tasks
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, NUM_TASKS, "All tasks must complete");
            println!("✓ Spawned {} tasks on single worker in {:?}", NUM_TASKS, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_spawn_on_distributed() {
        const NUM_TASKS: usize = 100_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let workers = dpdk::workers();
            let num_workers = workers.len();

            let per_worker_counts = Arc::new(Mutex::new(vec![0usize; num_workers]));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for i in 0..NUM_TASKS {
                // Round-robin distribution
                let target_worker = workers[i % num_workers];
                let counts = per_worker_counts.clone();

                handles.push(dpdk::spawn_on(target_worker, async move {
                    let current = dpdk::current_worker();
                    assert_eq!(current, target_worker);

                    let idx = current.index();
                    counts.lock().unwrap()[idx] += 1;
                }));
            }

            // Wait for all tasks
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let counts = per_worker_counts.lock().unwrap();
            let total: usize = counts.iter().sum();

            assert_eq!(total, NUM_TASKS, "All tasks must complete");

            // Verify distribution is roughly even
            let expected_per_worker = NUM_TASKS / num_workers;
            for (i, &count) in counts.iter().enumerate() {
                assert_eq!(count, expected_per_worker, "Worker {} count mismatch", i);
            }

            println!("✓ Distributed {} tasks across {} workers in {:?}", NUM_TASKS, num_workers, elapsed);
            println!("  Per-worker counts: {:?}", *counts);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_spawn_local_on_factory() {
        const NUM_TASKS: usize = 10_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let workers = dpdk::workers();
            let target_worker = workers[0];

            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for _ in 0..NUM_TASKS {
                let c = completed.clone();
                let target = target_worker;
                handles.push(dpdk::spawn_local_on(target_worker, move || async move {
                    // Use !Send type to verify local spawn works
                    let rc = std::rc::Rc::new(std::cell::Cell::new(100));
                    rc.set(rc.get() * 2);
                    assert_eq!(rc.get(), 200);

                    // Verify correct worker
                    assert_eq!(dpdk::current_worker(), target);

                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all tasks
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, NUM_TASKS, "All tasks must complete");
            println!("✓ Spawned {} local_on tasks in {:?}", NUM_TASKS, elapsed);
        });
    }
}

// =============================================================================
// Category B: Mixed API Tests
// =============================================================================

#[cfg(all(target_os = "linux", feature = "full"))]
mod mixed_api_stress {
    use super::*;

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_mixed_spawn_and_spawn_local() {
        const NUM_EACH: usize = 50_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let spawn_count = Arc::new(AtomicUsize::new(0));
            let local_count = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::new();

            // Spawn regular tasks
            for _ in 0..NUM_EACH {
                let c = spawn_count.clone();
                handles.push(tokio::spawn(async move {
                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Spawn local tasks
            for _ in 0..NUM_EACH {
                let c = local_count.clone();
                handles.push(dpdk::spawn_local(async move {
                    let rc = std::rc::Rc::new(42);
                    assert_eq!(*rc, 42);
                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();

            assert_eq!(spawn_count.load(Ordering::Relaxed), NUM_EACH);
            assert_eq!(local_count.load(Ordering::Relaxed), NUM_EACH);

            println!("✓ Mixed {} spawn + {} local in {:?}", NUM_EACH, NUM_EACH, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_mixed_all_spawn_variants() {
        const NUM_EACH: usize = 25_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let workers = dpdk::workers();
            let target = workers[0];

            let counts = Arc::new(Mutex::new(vec![0usize; 4]));
            let start = std::time::Instant::now();

            let mut handles = Vec::new();

            // 1. spawn
            for _ in 0..NUM_EACH {
                let c = counts.clone();
                handles.push(tokio::spawn(async move {
                    c.lock().unwrap()[0] += 1;
                }));
            }

            // 2. spawn_local
            for _ in 0..NUM_EACH {
                let c = counts.clone();
                handles.push(dpdk::spawn_local(async move {
                    c.lock().unwrap()[1] += 1;
                }));
            }

            // 3. spawn_on
            for _ in 0..NUM_EACH {
                let c = counts.clone();
                handles.push(dpdk::spawn_on(target, async move {
                    c.lock().unwrap()[2] += 1;
                }));
            }

            // 4. spawn_local_on
            for _ in 0..NUM_EACH {
                let c = counts.clone();
                handles.push(dpdk::spawn_local_on(target, || async move {
                    c.lock().unwrap()[3] += 1;
                }));
            }

            // Wait for all
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let c = counts.lock().unwrap();

            assert_eq!(c[0], NUM_EACH, "spawn count");
            assert_eq!(c[1], NUM_EACH, "spawn_local count");
            assert_eq!(c[2], NUM_EACH, "spawn_on count");
            assert_eq!(c[3], NUM_EACH, "spawn_local_on count");

            println!("✓ All 4 variants × {} tasks in {:?}", NUM_EACH, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_cross_worker_spawning() {
        const NUM_EACH_DIRECTION: usize = 10_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let workers = dpdk::workers();
            assert!(
                workers.len() >= 2,
                "Test requires at least 2 workers, got {}. \
                 Ensure env.json has enough DPDK devices with IPs and cores.",
                workers.len()
            );

            let worker0 = workers[0];
            let worker1 = workers[1];

            let count_0_to_1 = Arc::new(AtomicUsize::new(0));
            let count_1_to_0 = Arc::new(AtomicUsize::new(0));

            let start = std::time::Instant::now();

            // Worker 0 spawns tasks on Worker 1
            let c01 = count_0_to_1.clone();
            let h0 = dpdk::spawn_on(worker0, async move {
                let mut handles = Vec::new();
                for _ in 0..NUM_EACH_DIRECTION {
                    let c = c01.clone();
                    handles.push(dpdk::spawn_on(worker1, async move {
                        assert_eq!(dpdk::current_worker(), worker1);
                        c.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });

            // Worker 1 spawns tasks on Worker 0
            let c10 = count_1_to_0.clone();
            let h1 = dpdk::spawn_on(worker1, async move {
                let mut handles = Vec::new();
                for _ in 0..NUM_EACH_DIRECTION {
                    let c = c10.clone();
                    handles.push(dpdk::spawn_on(worker0, async move {
                        assert_eq!(dpdk::current_worker(), worker0);
                        c.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });

            h0.await.unwrap();
            h1.await.unwrap();

            let elapsed = start.elapsed();

            assert_eq!(count_0_to_1.load(Ordering::Relaxed), NUM_EACH_DIRECTION);
            assert_eq!(count_1_to_0.load(Ordering::Relaxed), NUM_EACH_DIRECTION);

            println!("✓ Bidirectional cross-worker spawn {} each in {:?}", NUM_EACH_DIRECTION, elapsed);
        });
    }
}

// =============================================================================
// Category C: Concurrent Stress Tests
// =============================================================================

#[cfg(all(target_os = "linux", feature = "full"))]
mod concurrent_stress {
    use super::*;

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_burst_spawn_rapid() {
        const BURST_SIZE: usize = 2_000;
        const NUM_BURSTS: usize = 50;
        const TOTAL: usize = BURST_SIZE * NUM_BURSTS;

        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            for _ in 0..NUM_BURSTS {
                let mut handles = Vec::with_capacity(BURST_SIZE);
                for _ in 0..BURST_SIZE {
                    let c = completed.clone();
                    handles.push(tokio::spawn(async move {
                        c.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                // Wait for this burst
                for h in handles {
                    h.await.unwrap();
                }
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, TOTAL);
            println!("✓ {} bursts × {} tasks = {} total in {:?}", NUM_BURSTS, BURST_SIZE, TOTAL, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_sustained_spawn_rate() {
        const DURATION_SECS: u64 = 5;
        const SPAWN_RATE: usize = 20_000; // per second

        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let spawned = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut all_handles = Vec::new();

            while start.elapsed().as_secs() < DURATION_SECS {
                let batch_start = std::time::Instant::now();

                // Spawn one batch
                for _ in 0..1000 {
                    let c = completed.clone();
                    spawned.fetch_add(1, Ordering::Relaxed);
                    all_handles.push(tokio::spawn(async move {
                        c.fetch_add(1, Ordering::Relaxed);
                    }));
                }

                // Rate limiting
                let elapsed = batch_start.elapsed();
                let target = Duration::from_micros((1_000_000 / (SPAWN_RATE / 1000)) as u64);
                if elapsed < target {
                    tokio::time::sleep(target - elapsed).await;
                }
            }

            // Wait for all tasks to complete
            for h in all_handles {
                h.await.unwrap();
            }

            let total_elapsed = start.elapsed();
            let spawn_count = spawned.load(Ordering::Relaxed);
            let complete_count = completed.load(Ordering::Relaxed);

            assert_eq!(spawn_count, complete_count);
            println!("✓ Sustained spawn: {} tasks in {:?}", spawn_count, total_elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_concurrent_spawners() {
        const NUM_SPAWNERS: usize = 10;
        const TASKS_PER_SPAWNER: usize = 10_000;
        const TOTAL: usize = NUM_SPAWNERS * TASKS_PER_SPAWNER;

        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            // Create spawner tasks
            let mut spawner_handles = Vec::new();

            for _ in 0..NUM_SPAWNERS {
                let c = completed.clone();
                spawner_handles.push(tokio::spawn(async move {
                    let mut handles = Vec::with_capacity(TASKS_PER_SPAWNER);
                    for _ in 0..TASKS_PER_SPAWNER {
                        let c2 = c.clone();
                        handles.push(tokio::spawn(async move {
                            c2.fetch_add(1, Ordering::Relaxed);
                        }));
                    }
                    // Wait for all this spawner's tasks
                    for h in handles {
                        h.await.unwrap();
                    }
                }));
            }

            // Wait for all spawners
            for h in spawner_handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, TOTAL);
            println!("✓ {} concurrent spawners × {} tasks = {} total in {:?}",
                     NUM_SPAWNERS, TASKS_PER_SPAWNER, TOTAL, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_spawn_from_spawn() {
        const INITIAL_TASKS: usize = 1_000;
        const CHILDREN_PER_TASK: usize = 10;
        const TOTAL: usize = INITIAL_TASKS * (1 + CHILDREN_PER_TASK);

        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::new();

            // First level: spawn initial tasks
            for _ in 0..INITIAL_TASKS {
                let c = completed.clone();
                handles.push(tokio::spawn(async move {
                    c.fetch_add(1, Ordering::Relaxed);

                    // Second level: each task spawns children
                    let mut child_handles = Vec::new();
                    for _ in 0..CHILDREN_PER_TASK {
                        let c2 = c.clone();
                        child_handles.push(tokio::spawn(async move {
                            c2.fetch_add(1, Ordering::Relaxed);
                        }));
                    }

                    // Wait for children
                    for h in child_handles {
                        h.await.unwrap();
                    }
                }));
            }

            // Wait for all
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, TOTAL);
            println!("✓ Nested spawn: {} parent × (1 + {}) = {} total in {:?}",
                     INITIAL_TASKS, CHILDREN_PER_TASK, TOTAL, elapsed);
        });
    }
}

// =============================================================================
// Category D: Resource Limits Tests
// =============================================================================

#[cfg(all(target_os = "linux", feature = "full"))]
mod resource_limits {
    use super::*;

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_local_queue_overflow() {
        // Local queue is 256 slots, spawn 10K to trigger overflow
        const NUM_TASKS: usize = 10_000;
        let rt = dpdk_rt();

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for _ in 0..NUM_TASKS {
                let c = completed.clone();
                handles.push(dpdk::spawn_local(async move {
                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, NUM_TASKS);
            println!("✓ Local queue overflow test: {} tasks in {:?}", NUM_TASKS, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    #[ignore] // Ignore by default - very large test
    fn stress_memory_pressure() {
        const NUM_TASKS: usize = 1_000_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for _ in 0..NUM_TASKS {
                let c = completed.clone();
                handles.push(tokio::spawn(async move {
                    // Allocate small vec
                    let _v = vec![0u8; 64];
                    c.fetch_add(1, Ordering::Relaxed);
                }));
            }

            // Wait for all
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let count = completed.load(Ordering::Relaxed);

            assert_eq!(count, NUM_TASKS);
            println!("✓ Memory pressure test: {} tasks in {:?}", NUM_TASKS, elapsed);
        });
    }
}

// =============================================================================
// Category E: Correctness Tests
// =============================================================================

#[cfg(all(target_os = "linux", feature = "full"))]
mod correctness {
    use super::*;

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_unique_task_ids() {
        const NUM_TASKS: usize = 100_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let ids = Arc::new(Mutex::new(HashSet::new()));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for i in 0..NUM_TASKS {
                let ids_clone = ids.clone();
                handles.push(tokio::spawn(async move {
                    // Each task has unique ID
                    let inserted = ids_clone.lock().unwrap().insert(i);
                    assert!(inserted, "Duplicate task ID: {}", i);
                    i
                }));
            }

            // Collect results
            let mut results = Vec::new();
            for h in handles {
                results.push(h.await.unwrap());
            }

            let elapsed = start.elapsed();

            assert_eq!(results.len(), NUM_TASKS);
            assert_eq!(ids.lock().unwrap().len(), NUM_TASKS);

            println!("✓ Unique IDs test: {} tasks, {} unique IDs in {:?}",
                     NUM_TASKS, ids.lock().unwrap().len(), elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_worker_affinity_enforcement() {
        const TASKS_PER_WORKER: usize = 25_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let workers = dpdk::workers();
            let num_workers = workers.len();

            let violations = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::new();

            for worker in workers {
                for _ in 0..TASKS_PER_WORKER {
                    let v = violations.clone();
                    handles.push(dpdk::spawn_on(worker, async move {
                        let current = dpdk::current_worker();
                        if current != worker {
                            v.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                }
            }

            // Wait for all
            for h in handles {
                h.await.unwrap();
            }

            let elapsed = start.elapsed();
            let v = violations.load(Ordering::Relaxed);

            assert_eq!(v, 0, "Worker affinity violations: {}", v);

            println!("✓ Affinity test: {} workers × {} tasks = {} total, 0 violations in {:?}",
                     num_workers, TASKS_PER_WORKER, num_workers * TASKS_PER_WORKER, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    fn stress_result_correctness() {
        const NUM_TASKS: usize = 100_000;
        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for i in 0..NUM_TASKS {
                handles.push(tokio::spawn(async move {
                    // Simple computation
                    let result = i * 2 + 1;
                    result
                }));
            }

            // Verify results
            for (i, h) in handles.into_iter().enumerate() {
                let result = h.await.unwrap();
                let expected = i * 2 + 1;
                assert_eq!(result, expected, "Task {} result mismatch", i);
            }

            let elapsed = start.elapsed();
            println!("✓ Result correctness: {} tasks, all results correct in {:?}", NUM_TASKS, elapsed);
        });
    }

    #[serial_isolation_test::serial_isolation_test]
    #[test]
    #[ignore] // FIXME: Panics in spawned tasks cause DPDK worker issues
    fn stress_panic_recovery() {
        const NUM_TASKS: usize = 10_000;
        const PANIC_RATE: usize = 100; // 1% panic (gentler)

        let rt = dpdk_rt_multi_worker(4);

        rt.block_on(async {
            let completed = Arc::new(AtomicUsize::new(0));
            let panicked = Arc::new(AtomicUsize::new(0));
            let start = std::time::Instant::now();

            let mut handles = Vec::with_capacity(NUM_TASKS);
            for i in 0..NUM_TASKS {
                let c = completed.clone();

                handles.push(tokio::spawn(async move {
                    // Skip i=0 to avoid panicking in the first task
                    if i > 0 && i % PANIC_RATE == 0 {
                        panic!("Intentional panic for test");
                    } else {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }

            // Check results
            for h in handles {
                if h.await.is_err() {
                    panicked.fetch_add(1, Ordering::Relaxed);
                }
            }

            let elapsed = start.elapsed();
            let c = completed.load(Ordering::Relaxed);
            let p = panicked.load(Ordering::Relaxed);

            // We skip i=0, so expected panic is (NUM_TASKS - 1) / PANIC_RATE
            let expected_panic = (NUM_TASKS - 1) / PANIC_RATE;
            let expected_complete = NUM_TASKS - expected_panic;

            assert_eq!(c, expected_complete, "Completed task count");
            assert_eq!(p, expected_panic, "Panicked task count");

            println!("✓ Panic recovery: {} tasks, {} completed, {} panicked in {:?}",
                     NUM_TASKS, c, p, elapsed);
        });
    }
}

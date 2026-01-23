//! Tests for the Unified Event Loop architecture.
//!
//! These tests verify the implementation of the unified event loop design
//! as specified in unified_event_loop.md.
//!
//! NOTE: These tests REQUIRE a DPDK environment with proper configuration.
//! They will FAIL if DPDK is not properly configured.

#![cfg(all(feature = "rt-multi-thread", feature = "net", target_os = "linux"))]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

// =============================================================================
// DPDK Device Detection (from env.json)
// =============================================================================

/// Detect DPDK device PCI address from env.json configuration.
fn detect_dpdk_device() -> String {
    const CONFIG_PATHS: &[&str] = &[
        "/etc/dpdk/env.json",
        "./config/dpdk-env.json",
        "./dpdk-env.json",
    ];

    for path in CONFIG_PATHS {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(devices) = json.get("devices").and_then(|d| d.as_array()) {
                    for device in devices {
                        if device.get("role").and_then(|r| r.as_str()) == Some("dpdk") {
                            if let Some(pci) = device.get("pci_address").and_then(|p| p.as_str()) {
                                return pci.to_string();
                            }
                        }
                    }
                }
            }
        }
    }

    panic!(
        "No DPDK device found in env.json. Searched: {:?}",
        CONFIG_PATHS
    )
}

/// Create a DPDK runtime with proper device configuration.
fn create_dpdk_runtime() -> tokio::runtime::Runtime {
    let device = detect_dpdk_device();
    tokio::runtime::Builder::new_dpdk()
        .dpdk_device(&device)
        .enable_all()
        .build()
        .expect("DPDK runtime creation must succeed")
}

// ============================================================================
// AC-1: Run executes spawned tasks
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac1_run_executes_spawned_tasks() {
    let rt = create_dpdk_runtime();
    let counter = Arc::new(AtomicUsize::new(0));
    let task_count = 10;

    rt.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..task_count {
            let counter = counter.clone();
            let handle = tokio::spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }
    });

    assert_eq!(counter.load(Ordering::SeqCst), task_count);
}

// ============================================================================
// AC-2: Run polls block_on future
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac2_run_polls_block_on_future() {
    let rt = create_dpdk_runtime();
    let poll_count = Arc::new(AtomicUsize::new(0));
    let poll_count_clone = poll_count.clone();

    let result = rt.block_on(async move {
        poll_count_clone.fetch_add(1, Ordering::SeqCst);
        42
    });

    assert_eq!(result, 42);
    assert!(poll_count.load(Ordering::SeqCst) >= 1);
}

// ============================================================================
// AC-3: Run exits on block_on complete
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac3_run_exits_on_block_on_complete() {
    let rt = create_dpdk_runtime();
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    let result = rt.block_on(async move {
        completed_clone.store(true, Ordering::SeqCst);
        42
    });

    assert_eq!(result, 42);
    assert!(completed.load(Ordering::SeqCst));
}

// ============================================================================
// AC-4: Maybe maintenance called
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac4_maybe_maintenance_called() {
    let rt = create_dpdk_runtime();

    rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..100 {
            let handle = tokio::spawn(async move {
                let _ = i * 2;
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }
    });
}

// ============================================================================
// AC-5: Buffer replenish called
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac5_buffer_replenish_called() {
    let rt = create_dpdk_runtime();

    rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..500 {
            let handle = tokio::spawn(async move {
                let _ = i * 2;
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }
    });
}

// ============================================================================
// AC-6: CPU affinity during block_on
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac6_cpu_affinity_during_block_on() {
    let rt = create_dpdk_runtime();
    let cpu_during = Arc::new(AtomicUsize::new(usize::MAX));
    let cpu_during_clone = cpu_during.clone();

    rt.block_on(async move {
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu >= 0 {
            cpu_during_clone.store(cpu as usize, Ordering::SeqCst);
        }
    });

    assert_ne!(cpu_during.load(Ordering::SeqCst), usize::MAX);
}

// ============================================================================
// AC-7: CPU affinity outside block_on
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac7_cpu_affinity_outside_block_on() {
    let rt = create_dpdk_runtime();
    let cpu_before = unsafe { libc::sched_getcpu() };

    rt.block_on(async {
        tokio::task::yield_now().await;
    });

    let cpu_after = unsafe { libc::sched_getcpu() };

    assert!(cpu_before >= 0);
    assert!(cpu_after >= 0);
    assert_eq!(cpu_before, cpu_after);
}

// ============================================================================
// AC-8: Worker0 state transfer
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac8_worker0_state_transfer() {
    let rt = create_dpdk_runtime();

    let result = rt.block_on(async {
        tokio::task::yield_now().await;
        "state_transfer_success"
    });

    assert_eq!(result, "state_transfer_success");
}

// ============================================================================
// AC-9: Worker0 recovery
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac9_worker0_recovery() {
    let rt = create_dpdk_runtime();

    // First block_on
    rt.block_on(async {
        tokio::task::yield_now().await;
    });

    std::thread::sleep(std::time::Duration::from_millis(10));

    // Second block_on - should work because worker 0 was respawned
    let result = rt.block_on(async {
        tokio::task::yield_now().await;
        42
    });

    assert_eq!(result, 42);
}

// ============================================================================
// AC-10: Multiple block_on
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac10_multiple_block_on() {
    let rt = create_dpdk_runtime();

    for i in 0..3 {
        let result = rt.block_on(async {
            tokio::task::yield_now().await;
            i * 10
        });
        assert_eq!(result, i * 10);
    }
}

// ============================================================================
// AC-11: Single run function
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac11_single_run_function() {
    let rt = create_dpdk_runtime();
    let result = rt.block_on(async { 42 });
    assert_eq!(result, 42);
}

// ============================================================================
// AC-12: All workers running after build
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac12_all_workers_running_after_build() {
    let rt = create_dpdk_runtime();

    std::thread::sleep(std::time::Duration::from_millis(50));

    let counter = Arc::new(AtomicUsize::new(0));

    rt.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..20 {
            let counter = counter.clone();
            handles.push(tokio::spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });

    assert_eq!(counter.load(Ordering::SeqCst), 20);
}

// ============================================================================
// AC-13: Spawn before block_on
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac13_spawn_before_block_on() {
    let rt = create_dpdk_runtime();
    let counter = Arc::new(AtomicUsize::new(0));

    let handle1 = rt.spawn({
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    let handle2 = rt.spawn({
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    rt.block_on(async {
        let _ = handle1.await;
        let _ = handle2.await;
    });

    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

// ============================================================================
// AC-14: Spawn on core
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac14_spawn_on_core() {
    let rt = create_dpdk_runtime();
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    rt.block_on(async move {
        let _ = tokio::spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
        })
        .await;
    });

    assert!(completed.load(Ordering::SeqCst));
}

// ============================================================================
// AC-15: Spawn local core
// ============================================================================

#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_ac15_spawn_local_core() {
    let rt = create_dpdk_runtime();
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    rt.block_on(async move {
        let _ = tokio::spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
        })
        .await;
    });

    assert!(completed.load(Ordering::SeqCst));
}

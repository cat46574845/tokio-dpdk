//! rte_flow Support Verification Test
//!
//! This test verifies whether the current DPDK device (e.g., AWS ENA) supports
//! rte_flow rules for multi-queue traffic steering.
//!
//! The implementation now panics if flow rules fail in multi-queue mode,
//! so this test will fail with a panic if rte_flow is not supported.
//!
//! Run with: SERIAL_ISOLATION_SUDO=1 cargo test -p tokio --test dpdk_rte_flow_verify -- --nocapture

#![cfg(all(feature = "full", feature = "test-util"))]

#[path = "support/dpdk_config.rs"]
mod dpdk_config;

use dpdk_config::*;
use serial_isolation_test::serial_isolation_test;
use tokio::runtime::Builder;

/// Test that creates a multi-queue DPDK runtime on a SINGLE device.
///
/// This test uses `dpdk_pci_addresses()` + `dpdk_num_workers()` to force:
/// 1. Using only ONE device (by PCI address)
/// 2. Creating TWO workers on that single device
/// 3. This requires 2 queues on the same NIC = multi-queue mode
/// 4. Flow rules are created for IP-based queue steering
/// 5. If rte_flow is NOT supported, runtime creation PANICS
///
/// Run with:
///   SERIAL_ISOLATION_SUDO=1 cargo test --manifest-path tokio/Cargo.toml \
///     --test dpdk_rte_flow_verify --features "full,test-util" -- --nocapture
#[test]
#[serial_isolation_test]
fn test_multi_queue_requires_rte_flow() {
    let env = DpdkEnv::get();

    // Get the DPDK device info
    let device = env.dpdk_device().expect("No DPDK device configured");
    let pci_address = &device.pci_address;

    println!("=== Multi-Queue rte_flow Verification (Single Device) ===");
    println!("Device: {} ({})", pci_address, device.original_name.as_deref().unwrap_or("N/A"));

    // Count available IPs on this device - we need at least 2 for multi-queue
    let ipv4_count = device.ipv4_addresses().len();
    let ipv6_count = device.ipv6_addresses().len();
    let total_ips = ipv4_count + ipv6_count;

    println!("IPv4 addresses: {:?}", device.ipv4_addresses());
    println!("IPv6 addresses: {} total", ipv6_count);
    println!("Total IPs: {}", total_ips);

    if total_ips < 2 {
        println!("\n⚠️  WARNING: Device only has {} IP(s), need 2+ for multi-queue test", total_ips);
        println!("   Skipping multi-queue test - add more IPs to test rte_flow.");
        println!("   You can add secondary IPs via AWS console or CLI.");
        return;
    }

    println!("\n[1] Creating DPDK runtime with 2 workers on SINGLE NIC...");
    println!("    Using dpdk_pci_addresses([\"{}\"]) - only this device", pci_address);
    println!("    Using dpdk_num_workers(2) - force 2 queues on 1 device");
    println!("    This requires rte_flow for IP-based queue steering.");
    println!("    If rte_flow is NOT supported, this will PANIC.\n");

    // Create runtime using new API:
    // - dpdk_pci_addresses() sets PCI addresses (triggers AllocationPlan path)
    // - dpdk_num_workers() sets worker count
    // This forces all workers onto the single device = multi-queue mode
    let result = Builder::new_dpdk()
        .dpdk_pci_addresses(&[pci_address.as_str()])  // Single device
        .dpdk_num_workers(2)  // 2 workers = 2 queues on same device
        .enable_all()
        .build();

    match result {
        Ok(rt) => {
            println!("\n✅ Runtime created successfully!");
            println!("   rte_flow is supported on this device.");

            // Run a simple async task to verify runtime is functional
            rt.block_on(async {
                println!("   Async task running on DPDK runtime");
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                println!("   ✅ Runtime is functional");
            });

            drop(rt);
            println!("\n✅ TEST PASSED - rte_flow is working!");
        }
        Err(e) => {
            // This could be configuration error or rte_flow panic
            eprintln!("\n❌ Runtime creation FAILED: {}", e);
            panic!("Failed to create DPDK runtime: {}", e);
        }
    }
}

/// Simple test to show device configuration info (no runtime creation)
#[test]
#[serial_isolation_test]
fn test_device_configuration_info() {
    let env = DpdkEnv::get();

    println!("=== DPDK Device Configuration ===\n");

    if let Some(device) = env.dpdk_device() {
        println!("DPDK Device:");
        println!("  PCI Address: {}", device.pci_address);
        println!("  Original Name: {:?}", device.original_name);
        println!("  MAC: {}", device.mac);
        println!("  MTU: {}", device.mtu);
        println!("  Role: {}", device.role);
        println!("  IPv4 Addresses: {:?}", device.ipv4_addresses());
        println!("  IPv6 Addresses: {:?}", device.ipv6_addresses());
        println!("  Total IPs: {} (need 2+ for multi-queue)", device.addresses.len());
    } else {
        println!("No DPDK device found in configuration!");
    }

    println!("\n=== Multi-Queue Readiness ===");
    if let Some(device) = env.dpdk_device() {
        let ip_count = device.addresses.len();

        if ip_count >= 2 {
            println!("✅ Ready for multi-queue: {} IPs available", ip_count);
            println!("   Run test_multi_queue_requires_rte_flow to verify rte_flow support.");
        } else {
            println!("❌ NOT ready for multi-queue:");
            println!("   - Need 2+ IPs, only have {}", ip_count);
            println!("   - Add secondary IPs via AWS console");
        }
    }

    println!("\n=== End of Configuration Info ===");
}

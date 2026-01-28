//! Test using device with multiple IPs

#![cfg(feature = "full")]
#![cfg(not(miri))]

#[path = "support/dpdk_config.rs"]
mod dpdk_config;

use dpdk_config::*;
use tokio::runtime::dpdk;

#[serial_isolation_test::serial_isolation_test]
#[test]
fn check_multi_ip_device() {
    // Dynamically find the DPDK device with the most IP addresses
    let device_info = get_dpdk_device_most_ips();
    let device = &device_info.pci_address;
    println!("Using device: {} ({} IPs)", device, device_info.addresses.len());

    // Use worker_threads(1) to avoid multi-queue mode which requires rte_flow
    // (AWS ENA does not support rte_flow). This test focuses on IP distribution info.
    let rt = tokio::runtime::Builder::new_dpdk()
        .dpdk_pci_addresses(&[device.as_str()])
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("Failed to create DPDK runtime");

    rt.block_on(async {
        let workers = dpdk::workers();
        println!("Number of workers: {}", workers.len());

        for (i, worker) in workers.iter().enumerate() {
            println!("\nWorker {}: {:?}", i, worker);

            let ipv4s = dpdk::worker_ipv4s(*worker);
            let ipv6s = dpdk::worker_ipv6s(*worker);

            println!("  IPv4s ({}): {:?}", ipv4s.len(), ipv4s);
            println!("  IPv6s ({}): {} addresses", ipv6s.len(), ipv6s.len());
            if !ipv6s.is_empty() {
                println!("    First: {:?}", ipv6s[0]);
                if ipv6s.len() > 1 {
                    println!("    Last: {:?}", ipv6s[ipv6s.len() - 1]);
                }
            }
        }
    });
}

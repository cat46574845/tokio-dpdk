//! Test using device with multiple IPs

#![cfg(feature = "full")]
#![cfg(not(miri))]

use tokio::runtime::dpdk;

#[serial_isolation_test::serial_isolation_test]
#[test]
fn check_multi_ip_device() {
    // Use device 0000:28:00.0 which has many IPv6 addresses in env.json
    let device = "0000:28:00.0";
    println!("Using device: {}", device);

    let rt = tokio::runtime::Builder::new_dpdk()
        .dpdk_pci_addresses(&[device])
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

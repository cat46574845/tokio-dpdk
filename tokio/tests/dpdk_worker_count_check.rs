//! Simple test to verify actual worker count and allocation behavior

#![cfg(feature = "full")]
#![cfg(not(miri))]

use tokio::runtime::dpdk;

#[serial_isolation_test::serial_isolation_test]
#[test]
fn check_default_worker_count() {
    let rt = tokio::runtime::Builder::new_dpdk()
        .enable_all()
        .build()
        .expect("Failed to create DPDK runtime");

    rt.block_on(async {
        let workers = dpdk::workers();
        println!("Number of workers: {}", workers.len());

        for (i, worker) in workers.iter().enumerate() {
            println!("Worker {}: {:?}", i, worker);

            let ipv4 = dpdk::worker_ipv4(*worker);
            let ipv6 = dpdk::worker_ipv6(*worker);
            let ipv4s = dpdk::worker_ipv4s(*worker);
            let ipv6s = dpdk::worker_ipv6s(*worker);

            println!("  IPv4: {:?}", ipv4);
            println!("  IPv6: {:?}", ipv6);
            println!("  All IPv4s: {:?}", ipv4s);
            println!("  All IPv6s: {:?}", ipv6s);
        }
    });
}

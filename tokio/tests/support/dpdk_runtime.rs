//! DPDK Runtime Quick Initialization Module
//!
//! Provides convenient functions to create DPDK runtimes for testing.
//! All functions read configuration from /etc/dpdk/env.json.
//!
//! This file is meant to be included in test files via:
//! ```rust,ignore
//! #[path = "support/dpdk_runtime.rs"]
//! mod dpdk_runtime;
//! use dpdk_runtime::*;
//! ```

// Re-export dpdk_config types
#[path = "dpdk_config.rs"]
mod dpdk_config;
pub use dpdk_config::*;

use std::sync::Arc;
use tokio::runtime::Runtime;

/// Create a DPDK runtime using all cores from configuration.
/// This is the default mode used by `#[tokio::test(flavor = "dpdk")]`.
pub fn dpdk_rt_all_cores() -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .enable_all()
            .build()
            .expect("DPDK runtime creation failed"),
    )
}

/// Create a DPDK runtime using a single core.
pub fn dpdk_rt_single_core() -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("DPDK runtime creation failed"),
    )
}

/// Create a DPDK runtime using specified number of cores.
pub fn dpdk_rt_n_cores(n: usize) -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .worker_threads(n)
            .enable_all()
            .build()
            .expect("DPDK runtime creation failed"),
    )
}

/// Create a DPDK runtime with a specific device PCI address.
pub fn dpdk_rt_with_device(pci_address: &str) -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .dpdk_pci_addresses(&[pci_address])
            .enable_all()
            .build()
            .expect(&format!(
                "DPDK runtime creation failed for device '{}'",
                pci_address
            )),
    )
}

/// Get the DPDK device PCI address from configuration.
/// Kept for informational purposes in tests that need to log device info.
pub fn detect_dpdk_device() -> String {
    let env = DpdkEnv::get();
    env.dpdk_device()
        .expect("No DPDK device found in configuration")
        .pci_address
        .clone()
}

/// Get the kernel IP address for test server binding.
pub fn get_kernel_ip() -> String {
    let env = DpdkEnv::get();
    env.primary_kernel_device()
        .and_then(|d| d.first_ipv4())
        .expect("No kernel device with IPv4 found")
}

/// Get the DPDK IP address.
pub fn get_dpdk_ip() -> String {
    let env = DpdkEnv::get();
    env.dpdk_device()
        .and_then(|d| d.first_ipv4())
        .expect("No DPDK device with IPv4 found")
}

/// Get test port from environment variable.
pub fn get_test_port() -> u16 {
    std::env::var("DPDK_TEST_PORT")
        .expect("DPDK_TEST_PORT environment variable is required")
        .parse()
        .expect("DPDK_TEST_PORT must be a valid port number")
}

/// Get test server address (kernel_ip:test_port).
pub fn get_test_server_addr() -> String {
    format!("{}:{}", get_kernel_ip(), get_test_port())
}

//! DPDK Test Configuration Module
//!
//! Shared configuration for all DPDK tests. Reads configuration from /etc/dpdk/env.json.
//! No default values - all required configuration must be explicitly provided.

use serde::Deserialize;
use std::fs;
use std::net::SocketAddr;
use std::sync::OnceLock;

/// Configuration file paths to search (in order)
const CONFIG_PATHS: &[&str] = &["/etc/dpdk/env.json", "./config/dpdk-env.json", "./dpdk-env.json"];

/// Environment variable for test port (required)
const TEST_PORT_ENV: &str = "DPDK_TEST_PORT";

/// Cached configuration
static CONFIG: OnceLock<DpdkEnv> = OnceLock::new();

/// Device configuration from env.json
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub pci_address: String,
    pub mac: String,
    pub addresses: Vec<String>,
    pub gateway_v4: Option<String>,
    pub gateway_v6: Option<String>,
    pub mtu: u32,
    pub role: String,
    pub original_name: Option<String>,
}

/// Hugepage configuration
#[derive(Debug, Clone, Deserialize)]
pub struct Hugepages {
    pub size_kb: u32,
    pub count: u32,
    pub mount: String,
}

/// Root configuration structure
#[derive(Debug, Clone, Deserialize)]
pub struct DpdkEnv {
    pub devices: Vec<Device>,
    pub hugepages: Hugepages,
    #[serde(default)]
    pub eal_args: Vec<String>,
}

impl DpdkEnv {
    /// Load configuration from file system
    pub fn load() -> Result<Self, String> {
        for path in CONFIG_PATHS {
            if let Ok(content) = fs::read_to_string(path) {
                return serde_json::from_str(&content)
                    .map_err(|e| format!("Failed to parse {}: {}", path, e));
            }
        }
        Err(format!(
            "No DPDK configuration found. Searched: {:?}",
            CONFIG_PATHS
        ))
    }

    /// Get cached configuration or load it
    pub fn get() -> &'static Self {
        CONFIG.get_or_init(|| Self::load().expect("DPDK configuration is required for tests"))
    }

    /// Get DPDK device (role = "dpdk")
    pub fn dpdk_device(&self) -> Option<&Device> {
        self.devices.iter().find(|d| d.role == "dpdk")
    }

    /// Get all DPDK devices
    pub fn dpdk_devices(&self) -> Vec<&Device> {
        self.devices.iter().filter(|d| d.role == "dpdk").collect()
    }

    /// Get kernel devices (role = "kernel")
    pub fn kernel_devices(&self) -> Vec<&Device> {
        self.devices.iter().filter(|d| d.role == "kernel").collect()
    }

    /// Get the primary kernel device (the one with most IPv4 addresses)
    /// This is typically the one that can communicate with DPDK via VPC routing.
    pub fn primary_kernel_device(&self) -> Option<&Device> {
        self.kernel_devices()
            .into_iter()
            .max_by_key(|d| d.ipv4_addresses().len())
    }
}

impl Device {
    /// Get the first IPv4 address (without CIDR suffix)
    pub fn first_ipv4(&self) -> Option<String> {
        self.addresses
            .iter()
            .find(|a| !a.contains(':')) // Filter out IPv6
            .map(|a| a.split('/').next().unwrap_or(a).to_string())
    }

    /// Get all IPv4 addresses (without CIDR suffix)
    pub fn ipv4_addresses(&self) -> Vec<String> {
        self.addresses
            .iter()
            .filter(|a| !a.contains(':'))
            .map(|a| a.split('/').next().unwrap_or(a).to_string())
            .collect()
    }

    /// Get the first IPv6 address (without CIDR suffix)
    pub fn first_ipv6(&self) -> Option<String> {
        self.addresses
            .iter()
            .find(|a| a.contains(':'))
            .map(|a| a.split('/').next().unwrap_or(a).to_string())
    }

    /// Get all IPv6 addresses (without CIDR suffix)
    pub fn ipv6_addresses(&self) -> Vec<String> {
        self.addresses
            .iter()
            .filter(|a| a.contains(':'))
            .map(|a| a.split('/').next().unwrap_or(a).to_string())
            .collect()
    }
}

/// Get the DPDK device PCI address for tests
/// Panics if not configured.
pub fn get_dpdk_device() -> String {
    DpdkEnv::get()
        .dpdk_device()
        .expect("No DPDK device found in env.json")
        .pci_address
        .clone()
}

/// Get the DPDK device with the most IP addresses (for multi-IP tests).
/// Panics if not configured.
pub fn get_dpdk_device_most_ips() -> &'static Device {
    DpdkEnv::get()
        .dpdk_devices()
        .into_iter()
        .max_by_key(|d| d.addresses.len())
        .expect("No DPDK device found in env.json")
}

/// Get the primary kernel IP address for tests
/// This is the kernel interface that can communicate with DPDK via VPC routing.
/// Panics if not configured.
pub fn get_kernel_ip() -> String {
    DpdkEnv::get()
        .primary_kernel_device()
        .expect("No kernel device found in env.json")
        .first_ipv4()
        .expect("Kernel device has no IPv4 address")
}

/// Get the test server bind address.
/// Uses kernel IP from env.json and port from DPDK_TEST_PORT environment variable.
/// Panics if DPDK_TEST_PORT is not set.
pub fn get_test_server_addr() -> SocketAddr {
    let ip = get_kernel_ip();
    let port: u16 = std::env::var(TEST_PORT_ENV)
        .unwrap_or_else(|_| panic!("{} environment variable is required for tests", TEST_PORT_ENV))
        .parse()
        .unwrap_or_else(|_| panic!("{} must be a valid port number", TEST_PORT_ENV));
    format!("{}:{}", ip, port)
        .parse()
        .expect("Invalid socket address")
}

/// Get the test port from environment variable.
/// Panics if DPDK_TEST_PORT is not set.
pub fn get_test_port() -> u16 {
    std::env::var(TEST_PORT_ENV)
        .unwrap_or_else(|_| panic!("{} environment variable is required for tests", TEST_PORT_ENV))
        .parse()
        .unwrap_or_else(|_| panic!("{} must be a valid port number", TEST_PORT_ENV))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_config() {
        // This will only pass if env.json exists
        if let Ok(config) = DpdkEnv::load() {
            assert!(!config.devices.is_empty());
            assert!(config.dpdk_device().is_some());
        }
    }
}

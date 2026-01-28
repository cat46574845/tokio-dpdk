//! DPDK Environment Configuration - Platform-Agnostic Intermediate Representation.
//!
//! This module defines the configuration format that platform-specific scripts
//! (e.g., AWS EC2, bare metal) generate. The tokio-dpdk runtime reads this
//! configuration at startup to initialize DPDK devices without needing to
//! access platform-specific APIs.
//!
//! # Configuration File Locations
//!
//! The runtime searches for configuration in the following order:
//! 1. `DPDK_ENV_CONFIG` environment variable (if set)
//! 2. `./config/dpdk-env.json` (project-local)
//! 3. `/etc/dpdk/env.json` (system-wide)
//!
//! # Example Configuration
//!
//! ```json
//! {
//!   "dpdk_cores": [1, 2, 3, 4],
//!   "devices": [
//!     {
//!       "pci_address": "0000:28:00.0",
//!       "mac": "02:04:3d:ba:a3:2f",
//!       "addresses": ["172.31.1.40/20", "172.31.1.41/20"],
//!       "gateway_v4": "172.31.0.1",
//!       "mtu": 9001,
//!       "role": "dpdk"
//!     }
//!   ]
//! }
//! ```

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv6Address};

/// Default configuration file paths (searched in order).
pub(crate) const DEFAULT_CONFIG_PATHS: &[&str] = &["./config/dpdk-env.json", "/etc/dpdk/env.json"];

/// DPDK environment configuration.
///
/// Contains device and core configuration loaded from env.json.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DpdkEnvConfig {
    /// Configured DPDK devices.
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,

    /// CPU cores available for DPDK workers.
    #[serde(default)]
    pub dpdk_cores: Vec<usize>,

    /// Extra EAL arguments to pass to rte_eal_init.
    ///
    /// These are merged with any EAL arguments specified via the Builder API.
    /// Environment config args are applied first, then builder args (allowing overrides).
    #[serde(default)]
    pub eal_args: Vec<String>,
}

/// Configuration for a single DPDK device.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DeviceConfig {
    /// PCI address (e.g., "0000:28:00.0").
    #[serde(default)]
    pub pci_address: String,

    /// MAC address as 6 bytes.
    #[serde(default, deserialize_with = "deserialize_mac")]
    pub mac: [u8; 6],

    /// IP addresses with prefix length (as strings, parsed at runtime).
    #[serde(default, deserialize_with = "deserialize_ip_cidrs")]
    pub addresses: Vec<IpCidr>,

    /// IPv4 default gateway.
    #[serde(default, deserialize_with = "deserialize_ipv4_gateway")]
    pub gateway_v4: Option<Ipv4Address>,

    /// IPv6 default gateway.
    #[serde(default, deserialize_with = "deserialize_ipv6_gateway")]
    pub gateway_v6: Option<Ipv6Address>,

    /// MTU (default: 1500, AWS ENA supports 9001).
    #[serde(default = "default_mtu")]
    pub mtu: u16,

    /// Device role: "dpdk" or "kernel".
    #[serde(default)]
    pub role: DeviceRole,

    /// Original interface name (before DPDK binding).
    #[serde(default)]
    pub original_name: Option<String>,
}

fn default_mtu() -> u16 {
    1500
}

/// Device role - whether bound to DPDK or left to kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DeviceRole {
    /// Bound to DPDK (vfio-pci or igb_uio).
    Dpdk,
    /// Left to kernel driver.
    #[default]
    Kernel,
}

impl DpdkEnvConfig {
    /// Validate that the configuration is usable for DPDK runtime.
    ///
    /// Checks:
    /// - At least one device with role "dpdk"
    /// - Each DPDK device has a non-empty PCI address
    /// - Each DPDK device has at least one IP address
    /// - Each DPDK device has a non-zero MAC address
    /// - dpdk_cores is non-empty
    pub(crate) fn validate(&self) -> io::Result<()> {
        let dpdk_devices: Vec<_> = self.devices.iter()
            .filter(|d| d.role == DeviceRole::Dpdk)
            .collect();

        if dpdk_devices.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DPDK env config has no device with role 'dpdk'. \
                 Check /etc/dpdk/env.json and ensure at least one device has \"role\": \"dpdk\".",
            ));
        }

        for dev in &dpdk_devices {
            if dev.pci_address.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "DPDK device has empty pci_address. \
                         Each DPDK device must have a valid PCI address (e.g., \"0000:28:00.0\")."
                    ),
                ));
            }
            if dev.addresses.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "DPDK device '{}' has no IP addresses configured. \
                         Each DPDK device must have at least one IP address.",
                        dev.pci_address
                    ),
                ));
            }
            if dev.mac == [0u8; 6] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "DPDK device '{}' has all-zero MAC address. \
                         Each DPDK device must have a valid MAC address.",
                        dev.pci_address
                    ),
                ));
            }
        }

        if self.dpdk_cores.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DPDK env config has no dpdk_cores. \
                 At least one CPU core must be configured for DPDK workers.",
            ));
        }

        Ok(())
    }

    /// Load configuration from the default search paths.
    pub(crate) fn load() -> io::Result<Self> {
        // Check environment variable first
        if let Ok(path) = std::env::var("DPDK_ENV_CONFIG") {
            return Self::load_from_file(&path);
        }

        // Search default paths
        for path in DEFAULT_CONFIG_PATHS {
            if Path::new(path).exists() {
                return Self::load_from_file(path);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "DPDK environment config not found. Searched: {:?}. \
                 Set DPDK_ENV_CONFIG or run the environment setup script.",
                DEFAULT_CONFIG_PATHS
            ),
        ))
    }

    /// Load configuration from a specific file.
    pub(crate) fn load_from_file(path: &str) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        Self::parse_json(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to parse {}: {}", path, e),
            )
        })
    }

    /// Parse configuration from JSON string.
    pub(crate) fn parse_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Invalid JSON: {}", e))
    }

    /// Find device by PCI address.
    pub(crate) fn find_by_pci(&self, pci: &str) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| d.pci_address == pci)
    }

    /// Find device by original interface name.
    pub(crate) fn find_by_name(&self, name: &str) -> Option<&DeviceConfig> {
        self.devices
            .iter()
            .find(|d| d.original_name.as_deref() == Some(name))
    }
}

impl Default for DpdkEnvConfig {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            dpdk_cores: Vec::new(),
            eal_args: Vec::new(),
        }
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            pci_address: String::new(),
            mac: [0u8; 6],
            addresses: Vec::new(),
            gateway_v4: None,
            gateway_v6: None,
            mtu: 1500,
            role: DeviceRole::Kernel,
            original_name: None,
        }
    }
}

// ============================================================================
// Custom Deserializers for smoltcp types
// ============================================================================

fn deserialize_mac<'de, D>(deserializer: D) -> Result<[u8; 6], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_mac_string(&s).map_err(serde::de::Error::custom)
}

fn deserialize_ip_cidrs<'de, D>(deserializer: D) -> Result<Vec<IpCidr>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let strings: Vec<String> = Vec::deserialize(deserializer)?;
    let mut cidrs = Vec::new();
    for s in strings {
        if let Ok(cidr) = s.parse::<IpCidr>() {
            cidrs.push(cidr);
        }
        // Skip invalid addresses silently
    }
    Ok(cidrs)
}

fn deserialize_ipv4_gateway<'de, D>(deserializer: D) -> Result<Option<Ipv4Address>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        Some(s) => {
            if let Ok(addr) = s.parse::<std::net::Ipv4Addr>() {
                Ok(Some(Ipv4Address::from_octets(addr.octets())))
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

fn deserialize_ipv6_gateway<'de, D>(deserializer: D) -> Result<Option<Ipv6Address>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        Some(s) => {
            let addr: std::net::Ipv6Addr = s.parse().unwrap_or_else(|e| {
                panic!(
                    "gateway_v6 '{}' is not a valid IPv6 address: {}. \
                     Fix the address in /etc/dpdk/env.json or re-run the config generator.",
                    s, e
                )
            });
            Ok(Some(Ipv6Address::from_octets(addr.octets())))
        }
        None => Ok(None),
    }
}

fn parse_mac_string(mac: &str) -> Result<[u8; 6], String> {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return Err(format!("Invalid MAC format: {}", mac));
    }

    let mut result = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        result[i] =
            u8::from_str_radix(part, 16).map_err(|_| format!("Invalid MAC byte: {}", part))?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mac_string() {
        let mac = parse_mac_string("02:04:3d:ba:a3:2f").unwrap();
        assert_eq!(mac, [0x02, 0x04, 0x3d, 0xba, 0xa3, 0x2f]);
    }

    #[test]
    fn test_parse_config() {
        let json = r#"{
            "dpdk_cores": [1, 2, 3],
            "devices": [
                {
                    "pci_address": "0000:28:00.0",
                    "mac": "02:04:3d:ba:a3:2f",
                    "addresses": ["172.31.1.40/20"],
                    "gateway_v4": "172.31.0.1",
                    "mtu": 9001,
                    "role": "dpdk"
                }
            ]
        }"#;

        let config = DpdkEnvConfig::parse_json(json).unwrap();
        assert_eq!(config.dpdk_cores, vec![1, 2, 3]);
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.devices[0].pci_address, "0000:28:00.0");
        assert_eq!(config.devices[0].mtu, 9001);
        assert_eq!(config.devices[0].role, DeviceRole::Dpdk);
        assert_eq!(config.devices[0].mac, [0x02, 0x04, 0x3d, 0xba, 0xa3, 0x2f]);
    }

    #[test]
    fn test_parse_config_field_order_independent() {
        // Test that parsing works regardless of field order in JSON
        let json = r#"{
            "devices": [
                {
                    "role": "dpdk",
                    "pci_address": "0000:29:00.0",
                    "mac": "aa:bb:cc:dd:ee:ff",
                    "addresses": ["10.0.0.1/24"],
                    "mtu": 1500
                }
            ],
            "dpdk_cores": [4, 5]
        }"#;

        let config = DpdkEnvConfig::parse_json(json).unwrap();
        assert_eq!(config.dpdk_cores, vec![4, 5]);
        assert_eq!(config.devices[0].pci_address, "0000:29:00.0");
        assert_eq!(config.devices[0].role, DeviceRole::Dpdk);
        assert_eq!(config.devices[0].mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{}"#;
        let config = DpdkEnvConfig::parse_json(json).unwrap();
        assert!(config.devices.is_empty());
        assert!(config.dpdk_cores.is_empty());
    }

    #[test]
    fn test_parse_config_with_extra_fields() {
        // Test that extra fields in JSON (like version, platform) are ignored,
        // and eal_args is correctly parsed
        let json = r#"{
            "version": 2,
            "platform": "aws-ec2",
            "generated_at": "2026-01-10T00:00:00Z",
            "dpdk_cores": [1],
            "devices": [],
            "eal_args": ["--iova-mode=pa"]
        }"#;

        let config = DpdkEnvConfig::parse_json(json).unwrap();
        assert_eq!(config.dpdk_cores, vec![1]);
        assert!(config.devices.is_empty());
        assert_eq!(config.eal_args, vec!["--iova-mode=pa".to_string()]);
    }

    #[test]
    fn test_parse_config_without_eal_args() {
        // eal_args should default to empty when not present
        let json = r#"{
            "dpdk_cores": [1],
            "devices": []
        }"#;

        let config = DpdkEnvConfig::parse_json(json).unwrap();
        assert!(config.eal_args.is_empty());
    }
}

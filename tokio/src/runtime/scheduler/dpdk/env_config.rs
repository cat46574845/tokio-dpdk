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
//!   "version": 1,
//!   "generated_at": "2026-01-09T08:00:00Z",
//!   "platform": "aws-ec2",
//!   "devices": [
//!     {
//!       "pci_address": "0000:28:00.0",
//!       "mac": "02:04:3d:ba:a3:2f",
//!       "addresses": ["172.31.1.40/20"],
//!       "gateway_v4": "172.31.0.1",
//!       "mtu": 9001,
//!       "role": "dpdk",
//!       "core_affinity": 1
//!     }
//!   ],
//!   "hugepages": {
//!     "size_kb": 2048,
//!     "count": 512,
//!     "mount": "/mnt/huge"
//!   },
//!   "eal_args": ["--iova-mode=pa"]
//! }
//! ```

use std::fs;
use std::io;
use std::path::Path;

use smoltcp::wire::{IpCidr, Ipv4Address, Ipv6Address};

/// Current configuration schema version.
pub(crate) const ENV_CONFIG_VERSION: u32 = 1;

/// Default configuration file paths (searched in order).
pub(crate) const DEFAULT_CONFIG_PATHS: &[&str] = &["./config/dpdk-env.json", "/etc/dpdk/env.json"];

#[derive(Debug, Clone)]
pub(crate) struct DpdkEnvConfig {
    /// Schema version for forward compatibility.
    pub version: u32,

    /// Timestamp when this config was generated.
    pub generated_at: Option<String>,

    /// Platform identifier (e.g., "aws-ec2", "bare-metal").
    pub platform: String,

    /// Configured DPDK devices.
    pub devices: Vec<DeviceConfig>,

    /// Additional EAL arguments.
    pub eal_args: Vec<String>,
}

/// Configuration for a single DPDK device.
#[derive(Debug, Clone)]
pub(crate) struct DeviceConfig {
    /// PCI address (e.g., "0000:28:00.0").
    pub pci_address: String,

    /// MAC address as 6 bytes.
    pub mac: [u8; 6],

    /// IP addresses with prefix length.
    pub addresses: Vec<IpCidr>,

    /// IPv4 default gateway.
    pub gateway_v4: Option<Ipv4Address>,

    /// IPv6 default gateway.
    pub gateway_v6: Option<Ipv6Address>,

    /// MTU (default: 1500, AWS ENA supports 9001).
    pub mtu: u16,

    /// Device role: "dpdk" or "kernel".
    pub role: DeviceRole,

    /// CPU core affinity for this device's worker.
    pub core_affinity: Option<usize>,

    /// Original interface name (before DPDK binding).
    pub original_name: Option<String>,
}

/// Device role - whether bound to DPDK or left to kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceRole {
    /// Bound to DPDK (vfio-pci or igb_uio).
    Dpdk,
    /// Left to kernel driver.
    Kernel,
}

impl DpdkEnvConfig {
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
        // Manual JSON parsing to avoid serde dependency
        // This is a simplified parser for the specific schema
        parse_env_config_json(json)
    }

    /// Get devices configured for DPDK.
    #[allow(dead_code)]
    pub(crate) fn dpdk_devices(&self) -> impl Iterator<Item = &DeviceConfig> {
        self.devices.iter().filter(|d| d.role == DeviceRole::Dpdk)
    }

    /// Find device by PCI address.
    #[allow(dead_code)]
    pub(crate) fn find_by_pci(&self, pci: &str) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| d.pci_address == pci)
    }

    /// Find device by MAC address.
    #[allow(dead_code)]
    pub(crate) fn find_by_mac(&self, mac: &[u8; 6]) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| &d.mac == mac)
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
            version: ENV_CONFIG_VERSION,
            generated_at: None,
            platform: "unknown".to_string(),
            devices: Vec::new(),
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
            core_affinity: None,
            original_name: None,
        }
    }
}

// ============================================================================
// JSON Parsing (without serde)
// ============================================================================

/// Parse the environment config JSON manually.
fn parse_env_config_json(json: &str) -> Result<DpdkEnvConfig, String> {
    // Very basic JSON parsing - production code should use a proper parser
    let json = json.trim();
    if !json.starts_with('{') || !json.ends_with('}') {
        return Err("Invalid JSON: expected object".to_string());
    }

    let mut config = DpdkEnvConfig::default();

    // Extract version
    if let Some(version) = extract_json_number(json, "version") {
        config.version = version as u32;
    }

    // Extract platform
    if let Some(platform) = extract_json_string(json, "platform") {
        config.platform = platform;
    }

    // Extract generated_at
    if let Some(generated_at) = extract_json_string(json, "generated_at") {
        config.generated_at = Some(generated_at);
    }

    // Extract devices array
    if let Some(devices_json) = extract_json_array(json, "devices") {
        config.devices = parse_devices_array(&devices_json)?;
    }

    // Extract eal_args
    if let Some(args_json) = extract_json_array(json, "eal_args") {
        config.eal_args = parse_string_array(&args_json);
    }

    Ok(config)
}

fn parse_devices_array(json: &str) -> Result<Vec<DeviceConfig>, String> {
    let mut devices = Vec::new();

    // Split by },{ pattern (simplified)
    let objects = split_json_array_objects(json);

    for obj in objects {
        devices.push(parse_device_config(&obj)?);
    }

    Ok(devices)
}

fn parse_device_config(json: &str) -> Result<DeviceConfig, String> {
    let mut device = DeviceConfig::default();

    if let Some(pci) = extract_json_string(json, "pci_address") {
        device.pci_address = pci;
    }

    if let Some(mac_str) = extract_json_string(json, "mac") {
        device.mac = parse_mac_string(&mac_str)?;
    }

    if let Some(addresses_json) = extract_json_array(json, "addresses") {
        device.addresses = parse_address_array(&addresses_json)?;
    }

    if let Some(gw) = extract_json_string(json, "gateway_v4") {
        if let Ok(addr) = gw.parse::<std::net::Ipv4Addr>() {
            device.gateway_v4 = Some(Ipv4Address::from_bytes(&addr.octets()));
        }
    }

    if let Some(gw) = extract_json_string(json, "gateway_v6") {
        if let Ok(addr) = gw.parse::<std::net::Ipv6Addr>() {
            device.gateway_v6 = Some(Ipv6Address::from_bytes(&addr.octets()));
        }
    }

    if let Some(mtu) = extract_json_number(json, "mtu") {
        device.mtu = mtu as u16;
    }

    if let Some(role) = extract_json_string(json, "role") {
        device.role = match role.as_str() {
            "dpdk" => DeviceRole::Dpdk,
            _ => DeviceRole::Kernel,
        };
    }

    if let Some(core) = extract_json_number(json, "core_affinity") {
        device.core_affinity = Some(core as usize);
    }

    if let Some(name) = extract_json_string(json, "original_name") {
        device.original_name = Some(name);
    }

    Ok(device)
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

fn parse_address_array(json: &str) -> Result<Vec<IpCidr>, String> {
    let strings = parse_string_array(json);
    let mut addrs = Vec::new();

    for s in strings {
        if let Ok(cidr) = s.parse::<IpCidr>() {
            addrs.push(cidr);
        }
    }

    Ok(addrs)
}

fn parse_string_array(json: &str) -> Vec<String> {
    let mut result = Vec::new();
    let json = json.trim();

    // Remove brackets
    let inner = json.trim_start_matches('[').trim_end_matches(']');

    for part in inner.split(',') {
        let s = part.trim().trim_matches('"');
        if !s.is_empty() {
            result.push(s.to_string());
        }
    }

    result
}

// Helper functions for basic JSON extraction

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let start = json.find(&pattern)?;
    let after_key = &json[start + pattern.len()..];

    // Find the colon and opening quote
    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start();

    if !after_colon.starts_with('"') {
        return None;
    }

    let value_start = 1;
    let value_end = after_colon[value_start..].find('"')?;

    Some(after_colon[value_start..value_start + value_end].to_string())
}

fn extract_json_number(json: &str, key: &str) -> Option<i64> {
    let pattern = format!("\"{}\"", key);
    let start = json.find(&pattern)?;
    let after_key = &json[start + pattern.len()..];

    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start();

    // Read digits
    let end = after_colon.find(|c: char| !c.is_ascii_digit() && c != '-')?;
    after_colon[..end].parse().ok()
}

fn extract_json_array(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let start = json.find(&pattern)?;
    let after_key = &json[start + pattern.len()..];

    let bracket_start = after_key.find('[')?;
    let remaining = &after_key[bracket_start..];

    // Find matching closing bracket
    let mut depth = 0;
    let mut end = 0;
    for (i, c) in remaining.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }

    Some(remaining[..=end].to_string())
}

fn split_json_array_objects(json: &str) -> Vec<String> {
    let mut result = Vec::new();
    let json = json.trim().trim_start_matches('[').trim_end_matches(']');

    let mut depth = 0;
    let mut start = 0;

    for (i, c) in json.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    result.push(json[start..=i].to_string());
                }
            }
            _ => {}
        }
    }

    result
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
    fn test_extract_json_string() {
        let json = r#"{ "platform": "aws-ec2", "version": 1 }"#;
        assert_eq!(
            extract_json_string(json, "platform"),
            Some("aws-ec2".to_string())
        );
    }

    #[test]
    fn test_extract_json_number() {
        let json = r#"{ "version": 123, "name": "test" }"#;
        assert_eq!(extract_json_number(json, "version"), Some(123));
    }

    #[test]
    fn test_parse_config() {
        let json = r#"{
            "version": 1,
            "platform": "aws-ec2",
            "devices": [
                {
                    "pci_address": "0000:28:00.0",
                    "mac": "02:04:3d:ba:a3:2f",
                    "addresses": ["172.31.1.40/20"],
                    "gateway_v4": "172.31.0.1",
                    "mtu": 9001,
                    "role": "dpdk",
                    "core_affinity": 1
                }
            ],
            "hugepages": {
                "size_kb": 2048,
                "count": 512
            }
        }"#;

        let config = DpdkEnvConfig::parse_json(json).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.platform, "aws-ec2");
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.devices[0].pci_address, "0000:28:00.0");
        assert_eq!(config.devices[0].mtu, 9001);
    }
}

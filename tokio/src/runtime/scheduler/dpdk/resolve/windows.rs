//! Windows-specific device resolution.
//!
//! Uses netdev crate (GetAdaptersAddresses) and PowerShell for network device information.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::process::Command;

use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

use super::ResolvedDevice;

/// Resolve a device by name on Windows.
pub(super) fn resolve_device(name: &str) -> io::Result<ResolvedDevice> {
    let mac = resolve_mac(name)?;
    let addresses = resolve_addresses(name)?;
    let gateway_v4 = resolve_gateway_v4(name)?;
    let gateway_v6 = resolve_gateway_v6(name)?;
    let core = resolve_irq_core(name)?;

    Ok(ResolvedDevice {
        name: name.to_string(),
        mac,
        addresses,
        gateway_v4,
        gateway_v6,
        core,
    })
}

/// Get MAC address using PowerShell Get-NetAdapter
pub(super) fn resolve_mac(name: &str) -> io::Result<[u8; 6]> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-NetAdapter -Name '{}' -ErrorAction Stop).MacAddress",
                escape_powershell_string(name)
            ),
        ])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run PowerShell: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Network adapter '{}' not found: {}", name, stderr),
        ));
    }

    let mac_str = String::from_utf8_lossy(&output.stdout);
    parse_mac_address(mac_str.trim())
}

/// Parse Windows MAC address format (e.g., "00-11-22-33-44-55" or "00:11:22:33:44:55")
fn parse_mac_address(mac_str: &str) -> io::Result<[u8; 6]> {
    let mac_str = mac_str.replace('-', ":");
    let parts: Vec<&str> = mac_str.split(':').collect();

    if parts.len() != 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Invalid MAC address format: {}", mac_str),
        ));
    }

    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part, 16).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid MAC byte '{}': {}", part, e),
            )
        })?;
    }
    Ok(mac)
}

/// Get IP addresses using PowerShell Get-NetIPAddress
pub(super) fn resolve_addresses(name: &str) -> io::Result<Vec<IpCidr>> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Get-NetIPAddress -InterfaceAlias '{}' -ErrorAction SilentlyContinue | \
                 Select-Object -Property IPAddress,PrefixLength | \
                 ConvertTo-Json -Compress",
                escape_powershell_string(name)
            ),
        ])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run PowerShell: {}", e)))?;

    if !output.status.success() || output.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    parse_ip_address_json(&json_str)
}

/// Parse JSON output from Get-NetIPAddress
fn parse_ip_address_json(json_str: &str) -> io::Result<Vec<IpCidr>> {
    let mut addresses = Vec::new();

    // Handle both single object and array
    let json_str = json_str.trim();
    if json_str.is_empty() {
        return Ok(addresses);
    }

    // Simple JSON parsing without external dependency
    // Format: [{"IPAddress":"10.0.1.10","PrefixLength":24}, ...]
    // or: {"IPAddress":"10.0.1.10","PrefixLength":24}

    for segment in json_str.split('}') {
        if let Some(ip_start) = segment.find("\"IPAddress\":\"") {
            let ip_start = ip_start + 13;
            if let Some(ip_end) = segment[ip_start..].find('"') {
                let ip_str = &segment[ip_start..ip_start + ip_end];

                if let Some(prefix_start) = segment.find("\"PrefixLength\":") {
                    let prefix_start = prefix_start + 15;
                    let prefix_str: String = segment[prefix_start..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();

                    if let Ok(prefix) = prefix_str.parse::<u8>() {
                        // Try IPv4 first
                        if let Ok(addr) = ip_str.parse::<Ipv4Addr>() {
                            addresses.push(IpCidr::Ipv4(Ipv4Cidr::new(
                                Ipv4Address::from_bytes(&addr.octets()),
                                prefix,
                            )));
                        }
                        // Then IPv6
                        else if let Ok(addr) = ip_str.parse::<Ipv6Addr>() {
                            // Skip link-local addresses (fe80::)
                            if !ip_str.starts_with("fe80:") {
                                addresses.push(IpCidr::Ipv6(Ipv6Cidr::new(
                                    Ipv6Address::from_bytes(&addr.octets()),
                                    prefix,
                                )));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(addresses)
}

/// Get IPv4 default gateway using PowerShell
pub(super) fn resolve_gateway_v4(name: &str) -> io::Result<Option<Ipv4Address>> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-NetRoute -InterfaceAlias '{}' -DestinationPrefix '0.0.0.0/0' \
                 -ErrorAction SilentlyContinue).NextHop",
                escape_powershell_string(name)
            ),
        ])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run PowerShell: {}", e)))?;

    if !output.status.success() {
        return Ok(None);
    }

    let gw_str = String::from_utf8_lossy(&output.stdout);
    let gw_str = gw_str.trim();

    if gw_str.is_empty() {
        return Ok(None);
    }

    match gw_str.parse::<Ipv4Addr>() {
        Ok(addr) => Ok(Some(Ipv4Address::from_bytes(&addr.octets()))),
        Err(_) => Ok(None),
    }
}

/// Get IPv6 default gateway using PowerShell
pub(super) fn resolve_gateway_v6(name: &str) -> io::Result<Option<Ipv6Address>> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-NetRoute -InterfaceAlias '{}' -DestinationPrefix '::/0' \
                 -ErrorAction SilentlyContinue).NextHop",
                escape_powershell_string(name)
            ),
        ])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run PowerShell: {}", e)))?;

    if !output.status.success() {
        return Ok(None);
    }

    let gw_str = String::from_utf8_lossy(&output.stdout);
    let gw_str = gw_str.trim();

    if gw_str.is_empty() {
        return Ok(None);
    }

    match gw_str.parse::<Ipv6Addr>() {
        Ok(addr) => Ok(Some(Ipv6Address::from_bytes(&addr.octets()))),
        Err(_) => Ok(None),
    }
}

/// Get CPU core from RSS processor affinity using PowerShell Get-NetAdapterRss
pub(super) fn resolve_irq_core(name: &str) -> io::Result<usize> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-NetAdapterRss -Name '{}' -ErrorAction SilentlyContinue).BaseProcessorNumber",
                escape_powershell_string(name)
            ),
        ])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run PowerShell: {}", e)))?;

    if !output.status.success() {
        // RSS not enabled or not supported, default to core 0
        return Ok(0);
    }

    let core_str = String::from_utf8_lossy(&output.stdout);
    let core_str = core_str.trim();

    if core_str.is_empty() {
        return Ok(0);
    }

    core_str
        .parse::<usize>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Invalid core: {}", e)))
}

/// Escape single quotes for PowerShell string
fn escape_powershell_string(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mac_address_colon() {
        let mac = parse_mac_address("00:11:22:33:44:55").unwrap();
        assert_eq!(mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn test_parse_mac_address_dash() {
        let mac = parse_mac_address("00-11-22-33-44-55").unwrap();
        assert_eq!(mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn test_escape_powershell_string() {
        assert_eq!(escape_powershell_string("Ethernet"), "Ethernet");
        assert_eq!(escape_powershell_string("Wi-Fi"), "Wi-Fi");
        assert_eq!(escape_powershell_string("It's"), "It''s");
    }
}

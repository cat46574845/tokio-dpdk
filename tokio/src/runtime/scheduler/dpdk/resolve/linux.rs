//! Linux-specific device resolution.
//!
//! Uses /sys/class/net and /proc for network device information.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::Command;

use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

use super::ResolvedDevice;

/// Resolve a device by name on Linux.
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

/// Read MAC address from /sys/class/net/{name}/address
pub(super) fn resolve_mac(name: &str) -> io::Result<[u8; 6]> {
    let path = format!("/sys/class/net/{}/address", name);
    let mac_str = fs::read_to_string(&path).map_err(|e| {
        io::Error::new(e.kind(), format!("Failed to read MAC from {}: {}", path, e))
    })?;

    parse_mac_address(mac_str.trim())
}

/// Parse MAC address string (e.g., "00:11:22:33:44:55")
fn parse_mac_address(mac_str: &str) -> io::Result<[u8; 6]> {
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

/// Get IP addresses using `ip addr show` command
pub(super) fn resolve_addresses(name: &str) -> io::Result<Vec<IpCidr>> {
    let output = Command::new("ip")
        .args(["addr", "show", "dev", name])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run 'ip addr show': {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Interface {} not found: {}", name, stderr),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ip_addr_output(&stdout)
}

/// Parse output of `ip addr show`
fn parse_ip_addr_output(output: &str) -> io::Result<Vec<IpCidr>> {
    let mut addresses = Vec::new();

    for line in output.lines() {
        let line = line.trim();

        // IPv4: "inet 10.0.1.10/24 ..."
        if line.starts_with("inet ") {
            if let Some(addr_str) = line.split_whitespace().nth(1) {
                if let Some(cidr) = parse_ipv4_cidr(addr_str) {
                    addresses.push(IpCidr::Ipv4(cidr));
                }
            }
        }
        // IPv6: "inet6 fd00::1/64 ..."
        else if line.starts_with("inet6 ") {
            if let Some(addr_str) = line.split_whitespace().nth(1) {
                if let Some(cidr) = parse_ipv6_cidr(addr_str) {
                    addresses.push(IpCidr::Ipv6(cidr));
                }
            }
        }
    }

    Ok(addresses)
}

/// Parse IPv4 CIDR string (e.g., "10.0.1.10/24")
fn parse_ipv4_cidr(s: &str) -> Option<Ipv4Cidr> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        return None;
    }

    let addr: Ipv4Addr = parts[0].parse().ok()?;
    let prefix: u8 = parts[1].parse().ok()?;

    Some(Ipv4Cidr::new(
        Ipv4Address::from_bytes(&addr.octets()),
        prefix,
    ))
}

/// Parse IPv6 CIDR string (e.g., "fd00::1/64")
fn parse_ipv6_cidr(s: &str) -> Option<Ipv6Cidr> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        return None;
    }

    let addr: Ipv6Addr = parts[0].parse().ok()?;
    let prefix: u8 = parts[1].parse().ok()?;

    Some(Ipv6Cidr::new(
        Ipv6Address::from_bytes(&addr.octets()),
        prefix,
    ))
}

/// Get IPv4 default gateway from routing table
pub(super) fn resolve_gateway_v4(name: &str) -> io::Result<Option<Ipv4Address>> {
    let output = Command::new("ip")
        .args(["route", "show", "dev", name])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to run 'ip route show': {}", e)))?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find "default via x.x.x.x"
    for line in stdout.lines() {
        if line.starts_with("default via ") {
            if let Some(gw_str) = line.split_whitespace().nth(2) {
                if let Ok(addr) = gw_str.parse::<Ipv4Addr>() {
                    return Ok(Some(Ipv4Address::from_bytes(&addr.octets())));
                }
            }
        }
    }

    Ok(None)
}

/// Get IPv6 default gateway from routing table
pub(super) fn resolve_gateway_v6(name: &str) -> io::Result<Option<Ipv6Address>> {
    let output = Command::new("ip")
        .args(["-6", "route", "show", "dev", name])
        .output()
        .map_err(|e| {
            io::Error::new(e.kind(), format!("Failed to run 'ip -6 route show': {}", e))
        })?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find "default via xxxx::xxxx"
    for line in stdout.lines() {
        if line.starts_with("default via ") {
            if let Some(gw_str) = line.split_whitespace().nth(2) {
                if let Ok(addr) = gw_str.parse::<Ipv6Addr>() {
                    return Ok(Some(Ipv6Address::from_bytes(&addr.octets())));
                }
            }
        }
    }

    Ok(None)
}

/// Get CPU core from IRQ affinity.
///
/// This reads the IRQ number for the device and then reads its CPU affinity
/// from /proc/irq/{irq}/smp_affinity.
pub(super) fn resolve_irq_core(name: &str) -> io::Result<usize> {
    // Try to find IRQ in /sys/class/net/{name}/device/msi_irqs/
    let msi_irqs_path = format!("/sys/class/net/{}/device/msi_irqs", name);

    if Path::new(&msi_irqs_path).exists() {
        // MSI-X: multiple IRQs, get the first one
        if let Ok(entries) = fs::read_dir(&msi_irqs_path) {
            for entry in entries.flatten() {
                if let Some(irq_str) = entry.file_name().to_str() {
                    if let Ok(irq) = irq_str.parse::<u32>() {
                        return read_irq_affinity(irq);
                    }
                }
            }
        }
    }

    // Fallback: try /proc/interrupts
    if let Some(irq) = find_irq_from_proc_interrupts(name)? {
        return read_irq_affinity(irq);
    }

    // Default to core 0 if we can't determine affinity
    Ok(0)
}

/// Find IRQ number from /proc/interrupts
fn find_irq_from_proc_interrupts(name: &str) -> io::Result<Option<u32>> {
    let content = fs::read_to_string("/proc/interrupts")?;

    for line in content.lines() {
        if line.contains(name) {
            // Line format: "  42:  ... device_name"
            let irq_str = line.trim().split(':').next();
            if let Some(irq_str) = irq_str {
                if let Ok(irq) = irq_str.trim().parse::<u32>() {
                    return Ok(Some(irq));
                }
            }
        }
    }

    Ok(None)
}

/// Read CPU affinity for an IRQ and return the first CPU in the mask
fn read_irq_affinity(irq: u32) -> io::Result<usize> {
    let path = format!("/proc/irq/{}/smp_affinity", irq);
    let affinity_str = fs::read_to_string(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to read IRQ {} affinity: {}", irq, e),
        )
    })?;

    // smp_affinity is a hex bitmask (e.g., "00000001" for CPU 0)
    parse_affinity_mask(affinity_str.trim())
}

/// Parse smp_affinity hex mask and return the first set CPU
fn parse_affinity_mask(hex_mask: &str) -> io::Result<usize> {
    // Remove commas (for large masks split across groups)
    let hex_mask = hex_mask.replace(',', "");

    // Parse as hex number
    let mask = u64::from_str_radix(&hex_mask, 16).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Invalid affinity mask '{}': {}", hex_mask, e),
        )
    })?;

    // Find first set bit (lowest numbered CPU)
    for i in 0..64 {
        if mask & (1 << i) != 0 {
            return Ok(i);
        }
    }

    // If mask is 0, default to CPU 0
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mac_address() {
        let mac = parse_mac_address("00:11:22:33:44:55").unwrap();
        assert_eq!(mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn test_parse_ipv4_cidr() {
        let cidr = parse_ipv4_cidr("10.0.1.10/24").unwrap();
        assert_eq!(cidr.prefix_len(), 24);
    }

    #[test]
    fn test_parse_affinity_mask() {
        assert_eq!(parse_affinity_mask("00000001").unwrap(), 0);
        assert_eq!(parse_affinity_mask("00000002").unwrap(), 1);
        assert_eq!(parse_affinity_mask("00000004").unwrap(), 2);
        assert_eq!(parse_affinity_mask("00000008").unwrap(), 3);
    }
}

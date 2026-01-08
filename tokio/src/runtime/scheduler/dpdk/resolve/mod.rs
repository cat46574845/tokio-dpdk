//! Device resolver module for DPDK runtime.
//!
//! This module provides OS-specific implementations for resolving
//! network device configuration from the system.

use std::io;

use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address};

/// Resolved device configuration from the system.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedDevice {
    /// Device name (e.g., "eth0", "ens5")
    pub name: String,

    /// MAC address
    pub mac: [u8; 6],

    /// IP addresses (IPv4 and IPv6)
    pub addresses: Vec<IpCidr>,

    /// IPv4 default gateway
    pub gateway_v4: Option<Ipv4Address>,

    /// IPv6 default gateway
    pub gateway_v6: Option<Ipv6Address>,

    /// CPU core to bind this device's worker to
    /// (derived from IRQ affinity)
    pub core: usize,
}

impl ResolvedDevice {
    /// Check if this device has any IPv4 addresses
    pub fn has_ipv4(&self) -> bool {
        self.addresses
            .iter()
            .any(|a| matches!(a.address(), IpAddress::Ipv4(_)))
    }

    /// Check if this device has any IPv6 addresses
    pub fn has_ipv6(&self) -> bool {
        self.addresses
            .iter()
            .any(|a| matches!(a.address(), IpAddress::Ipv6(_)))
    }
}

/// Device override configuration provided by user.
#[derive(Debug, Clone, Default)]
pub struct DeviceOverride {
    /// Override IP addresses (if Some, replaces auto-detected)
    pub addresses: Option<Vec<IpCidr>>,

    /// Override IPv4 gateway
    pub gateway_v4: Option<Ipv4Address>,

    /// Override IPv6 gateway
    pub gateway_v6: Option<Ipv6Address>,

    /// Override CPU core
    pub core: Option<usize>,
}

/// Resolve a device by name, optionally applying overrides.
pub(crate) fn resolve_device(
    name: &str,
    overrides: Option<&DeviceOverride>,
) -> io::Result<ResolvedDevice> {
    // Get base configuration from OS
    let mut device = resolve_device_from_os(name)?;

    // Apply overrides if provided
    if let Some(ovr) = overrides {
        if let Some(ref addrs) = ovr.addresses {
            device.addresses = addrs.clone();
        }
        if let Some(gw) = ovr.gateway_v4 {
            device.gateway_v4 = Some(gw);
        }
        if let Some(gw) = ovr.gateway_v6 {
            device.gateway_v6 = Some(gw);
        }
        if let Some(core) = ovr.core {
            device.core = core;
        }
    }

    Ok(device)
}

// OS-specific implementations
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
fn resolve_device_from_os(name: &str) -> io::Result<ResolvedDevice> {
    linux::resolve_device(name)
}

#[cfg(target_os = "windows")]
fn resolve_device_from_os(name: &str) -> io::Result<ResolvedDevice> {
    windows::resolve_device(name)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn resolve_device_from_os(_name: &str) -> io::Result<ResolvedDevice> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "DPDK is only supported on Linux and Windows",
    ))
}

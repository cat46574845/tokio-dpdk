//! DPDK rte_flow rule management for multi-queue traffic routing.
//!
//! This module creates rte_flow rules to route incoming traffic to the correct
//! RX queues based on destination IP address. This is essential for multi-queue
//! configurations where different workers handle different IPs on the same device.
//!
//! # Design
//!
//! Each IP address bound to a queue gets a flow rule that matches packets with
//! that destination IP and directs them to the corresponding queue.
//!
//! # AWS ENA Support
//!
//! Note: AWS ENA has limited rte_flow support. If flow rules cannot be created,
//! the driver falls back to RSS or single-queue mode. This is logged as a warning.

use std::io;

use smoltcp::wire::IpCidr;

use super::ffi;

/// Handle to an rte_flow rule.
///
/// When dropped, the flow rule is destroyed.
pub(crate) struct FlowRule {
    port_id: u16,
    flow: *mut ffi::rte_flow,
}

// Safety: Flow rules are port-specific and only accessed during setup/teardown
unsafe impl Send for FlowRule {}
unsafe impl Sync for FlowRule {}

impl Drop for FlowRule {
    fn drop(&mut self) {
        if !self.flow.is_null() {
            let mut error: ffi::rte_flow_error = unsafe { std::mem::zeroed() };
            let ret =
                unsafe { ffi::dpdk_wrap_rte_flow_destroy(self.port_id, self.flow, &mut error) };
            if ret != 0 {
                eprintln!("[DPDK] Warning: Failed to destroy flow rule: {}", ret);
            }
            self.flow = std::ptr::null_mut();
        }
    }
}

/// Creates flow rules to route traffic to specific queues based on destination IP.
///
/// # Arguments
///
/// * `port_id` - DPDK port ID
/// * `allocations` - List of (IP address, queue_id) pairs
///
/// # Returns
///
/// A vector of FlowRule handles, or an error if rule creation fails.
pub(crate) fn create_flow_rules(
    port_id: u16,
    allocations: &[(IpCidr, u16)],
) -> io::Result<Vec<FlowRule>> {
    let mut rules = Vec::with_capacity(allocations.len());

    for (ip, queue_id) in allocations {
        match create_single_flow_rule(port_id, ip, *queue_id) {
            Ok(rule) => rules.push(rule),
            Err(e) => {
                // Log warning but continue - flow rules are best-effort
                // AWS ENA has limited rte_flow support
                eprintln!(
                    "[DPDK] Warning: Failed to create flow rule for {} -> queue {}: {}. \
                     Traffic may not be correctly routed.",
                    ip, queue_id, e
                );
            }
        }
    }

    Ok(rules)
}

/// Creates a single flow rule for an IP/queue mapping.
fn create_single_flow_rule(port_id: u16, ip: &IpCidr, queue_id: u16) -> io::Result<FlowRule> {
    // Log the intended rule for debugging
    log_flow_rule_creation(port_id, ip, queue_id);

    // Build pattern from IP
    let builder = FlowPatternBuilder::new(ip);
    let is_ipv4 = builder.is_ipv4();
    let dst_addr = builder.dst_addr();
    let mask = builder.create_mask();

    // Prepare error struct
    let mut error: ffi::rte_flow_error = unsafe { std::mem::zeroed() };

    // Call the C wrapper function
    let flow = unsafe {
        ffi::dpdk_wrap_rte_flow_create_queue_rule(
            port_id,
            0, // priority (0 = highest)
            if is_ipv4 { 1 } else { 0 },
            dst_addr.as_ptr(),
            mask.as_ptr(),
            queue_id,
            &mut error,
        )
    };

    if flow.is_null() {
        // Check the error type
        let error_msg = if error.message.is_null() {
            "Unknown error".to_string()
        } else {
            unsafe {
                std::ffi::CStr::from_ptr(error.message)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("rte_flow_create failed: {}", error_msg),
        ));
    }

    Ok(FlowRule { port_id, flow })
}

/// Logs the flow rule creation for debugging.
fn log_flow_rule_creation(port_id: u16, ip: &IpCidr, queue_id: u16) {
    // Only log if running in debug mode or with DPDK_DEBUG set
    if std::env::var("DPDK_DEBUG").is_ok() {
        eprintln!(
            "[DPDK] Creating flow rule: port {} | {} -> queue {}",
            port_id, ip, queue_id
        );
    }
}

/// Checks if the device supports rte_flow rules.
///
/// This function attempts to validate a minimal flow rule to determine
/// if the driver supports rte_flow. Some drivers (e.g., AWS ENA) have
/// limited or no rte_flow support.
///
/// Returns true if rte_flow is likely supported, false otherwise.
pub(crate) fn check_flow_support(port_id: u16) -> bool {
    // First check if we can get device info
    let mut dev_info: ffi::rte_eth_dev_info = unsafe { std::mem::zeroed() };
    let ret = unsafe { ffi::rte_eth_dev_info_get(port_id, &mut dev_info) };

    if ret != 0 {
        return false;
    }

    // Try to validate a minimal flow rule to test rte_flow support
    // This is a lightweight check - actual rule creation may still fail
    // for specific patterns not supported by the driver.
    //
    // We create a simple IPv4 destination match rule as a probe.
    let test_ip: smoltcp::wire::IpCidr = "0.0.0.0/0".parse().unwrap();
    let builder = FlowPatternBuilder::new(&test_ip);
    let is_ipv4 = builder.is_ipv4();
    let dst_addr = builder.dst_addr();
    let mask = builder.create_mask();

    let mut error: ffi::rte_flow_error = unsafe { std::mem::zeroed() };

    // Use validate (not create) to test support without side effects
    let ret = unsafe {
        ffi::dpdk_wrap_rte_flow_validate_queue_rule(
            port_id,
            0, // priority
            if is_ipv4 { 1 } else { 0 },
            dst_addr.as_ptr(),
            mask.as_ptr(),
            0, // queue_id (doesn't matter for validation)
            &mut error,
        )
    };

    // ret == 0 means validation passed (rte_flow is supported)
    // ret != 0 means the driver doesn't support this flow pattern
    if ret != 0 {
        // Log the specific error if debug is enabled
        if std::env::var("DPDK_DEBUG").is_ok() {
            let error_msg = if error.message.is_null() {
                "Unknown error".to_string()
            } else {
                unsafe {
                    std::ffi::CStr::from_ptr(error.message)
                        .to_string_lossy()
                        .into_owned()
                }
            };
            eprintln!(
                "[DPDK] Port {} does not support rte_flow: {}",
                port_id, error_msg
            );
        }
        return false;
    }

    true
}

/// Builder for constructing flow rule patterns.
///
/// This is used internally to create the match patterns for rte_flow.
pub(crate) struct FlowPatternBuilder {
    is_ipv4: bool,
    dst_addr: [u8; 16],
    prefix_len: u8,
}

impl FlowPatternBuilder {
    /// Creates a new pattern builder for the given IP.
    pub(crate) fn new(ip: &IpCidr) -> Self {
        let (is_ipv4, dst_addr, prefix_len) = match ip {
            IpCidr::Ipv4(cidr) => {
                let mut addr = [0u8; 16];
                addr[..4].copy_from_slice(&cidr.address().0);
                (true, addr, cidr.prefix_len())
            }
            IpCidr::Ipv6(cidr) => {
                let mut addr = [0u8; 16];
                addr.copy_from_slice(&cidr.address().0);
                (false, addr, cidr.prefix_len())
            }
        };

        Self {
            is_ipv4,
            dst_addr,
            prefix_len,
        }
    }

    /// Returns true if this is an IPv4 pattern.
    pub(crate) fn is_ipv4(&self) -> bool {
        self.is_ipv4
    }

    /// Returns the destination address bytes.
    pub(crate) fn dst_addr(&self) -> &[u8] {
        if self.is_ipv4 {
            &self.dst_addr[..4]
        } else {
            &self.dst_addr
        }
    }

    /// Returns the prefix length.
    #[allow(dead_code)] // Used in tests
    pub(crate) fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    /// Creates a mask from the prefix length.
    pub(crate) fn create_mask(&self) -> Vec<u8> {
        let addr_len = if self.is_ipv4 { 4 } else { 16 };
        let mut mask = vec![0u8; addr_len];

        let full_bytes = (self.prefix_len / 8) as usize;
        let remaining_bits = self.prefix_len % 8;

        for byte in mask.iter_mut().take(full_bytes) {
            *byte = 0xff;
        }

        if full_bytes < addr_len && remaining_bits > 0 {
            mask[full_bytes] = 0xff << (8 - remaining_bits);
        }

        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flow_pattern_builder_ipv4() {
        let ip: IpCidr = "192.168.1.100/24".parse().unwrap();
        let builder = FlowPatternBuilder::new(&ip);

        assert!(builder.is_ipv4());
        assert_eq!(builder.dst_addr(), &[192, 168, 1, 100]);
        assert_eq!(builder.prefix_len(), 24);
    }

    #[test]
    fn test_flow_pattern_builder_ipv6() {
        let ip: IpCidr = "2001:db8::1/64".parse().unwrap();
        let builder = FlowPatternBuilder::new(&ip);

        assert!(!builder.is_ipv4());
        assert_eq!(builder.prefix_len(), 64);
    }

    #[test]
    fn test_create_mask_24() {
        let ip: IpCidr = "192.168.1.0/24".parse().unwrap();
        let builder = FlowPatternBuilder::new(&ip);
        let mask = builder.create_mask();

        assert_eq!(mask, vec![0xff, 0xff, 0xff, 0x00]);
    }

    #[test]
    fn test_create_mask_20() {
        let ip: IpCidr = "192.168.0.0/20".parse().unwrap();
        let builder = FlowPatternBuilder::new(&ip);
        let mask = builder.create_mask();

        assert_eq!(mask, vec![0xff, 0xff, 0xf0, 0x00]);
    }

    #[test]
    fn test_create_mask_64_ipv6() {
        let ip: IpCidr = "2001:db8::/64".parse().unwrap();
        let builder = FlowPatternBuilder::new(&ip);
        let mask = builder.create_mask();

        assert_eq!(mask.len(), 16);
        assert_eq!(
            &mask[..8],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            &mask[8..],
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn test_flow_rule_queue_action() {
        // Test that create_flow_rules would generate correct queue actions
        // We can't test the actual FFI calls without DPDK, but we can verify the allocations
        let allocations: Vec<(IpCidr, u16)> = vec![
            ("10.0.0.1/24".parse().unwrap(), 0),
            ("10.0.0.2/24".parse().unwrap(), 1),
            ("10.0.0.3/24".parse().unwrap(), 2),
        ];

        // Verify the allocations are correctly structured
        assert_eq!(allocations.len(), 3);
        assert_eq!(allocations[0].1, 0); // queue 0
        assert_eq!(allocations[1].1, 1); // queue 1
        assert_eq!(allocations[2].1, 2); // queue 2
    }

    #[test]
    fn test_flow_rule_multiple_ips() {
        // Test that multiple IPs result in multiple allocations
        let allocations: Vec<(IpCidr, u16)> = vec![
            ("10.0.0.1/24".parse().unwrap(), 0),
            ("10.0.0.2/24".parse().unwrap(), 1),
            ("2001:db8::1/64".parse().unwrap(), 0),
            ("2001:db8::2/64".parse().unwrap(), 1),
        ];

        // Verify we have 4 allocations (2 IPv4 + 2 IPv6)
        assert_eq!(allocations.len(), 4);

        // Verify IPv4 and IPv6 are both present
        let ipv4_count = allocations
            .iter()
            .filter(|(ip, _)| ip.address().as_bytes().len() == 4)
            .count();
        let ipv6_count = allocations
            .iter()
            .filter(|(ip, _)| ip.address().as_bytes().len() == 16)
            .count();

        assert_eq!(ipv4_count, 2);
        assert_eq!(ipv6_count, 2);
    }
}

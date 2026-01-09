//! DPDK runtime configuration.
//!
//! This module provides the configuration API for the DPDK scheduler.
//! The main entry point is `DpdkBuilder` which allows specifying network
//! devices by name and automatically resolves their configuration from
//! the system.

use std::collections::HashMap;
use std::io;

use super::resolve::{DeviceOverride, ResolvedDevice};

/// Builder for DPDK runtime configuration.
///
/// # Example
///
/// ```ignore
/// use tokio::runtime::Builder;
///
/// // Simple usage: just specify device names
/// let rt = Builder::new_dpdk()
///     .devices(&["eth0", "eth1", "eth2"])
///     .build()?;
///
/// // With custom EAL arguments
/// let rt = Builder::new_dpdk()
///     .devices(&["eth0"])
///     .eal_arg("--no-huge")
///     .eal_arg("-m").eal_arg("128")
///     .build()?;
/// ```
#[derive(Debug, Clone, Default)]
pub(crate) struct DpdkBuilder {
    /// Device names to use
    devices: Vec<String>,

    /// Per-device overrides
    device_overrides: HashMap<String, DeviceOverrideConfig>,

    /// Extra EAL arguments
    extra_eal_args: Vec<String>,

    /// Scheduler configuration (mirrors Tokio's Builder API)
    scheduler_config: DpdkSchedulerConfig,
}

/// Scheduler configuration for DPDK runtime.
/// This mirrors the scheduler-related settings from Tokio's Builder API.
#[derive(Debug, Clone)]
pub(crate) struct DpdkSchedulerConfig {
    /// How often to check for external events (I/O, signals, timers).
    /// This corresponds to `Builder::event_interval`.
    pub(crate) event_interval: u32,

    /// How often worker threads steal from the global queue.
    /// This corresponds to `Builder::global_queue_interval`.
    pub(crate) global_queue_interval: Option<u32>,

    /// Disable the LIFO slot optimization.
    /// This corresponds to `Builder::disable_lifo_slot`.
    pub(crate) disable_lifo_slot: bool,
}

impl Default for DpdkSchedulerConfig {
    fn default() -> Self {
        Self {
            // Default matches Tokio's defaults
            event_interval: 61,
            global_queue_interval: None,
            disable_lifo_slot: false,
        }
    }
}

/// Device override configuration (public API wrapper).
#[derive(Debug, Clone, Default)]
pub(crate) struct DeviceOverrideConfig {
    /// Override IP addresses
    pub addresses: Option<Vec<String>>,

    /// Override IPv4 gateway
    pub gateway_v4: Option<String>,

    /// Override IPv6 gateway
    pub gateway_v6: Option<String>,

    /// Override CPU core
    pub core: Option<usize>,
}

impl DpdkBuilder {
    /// Creates a new DPDK builder.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add devices by name. Device configuration (IP, gateway, MAC, CPU core)
    /// will be automatically resolved from the operating system.
    pub(crate) fn devices(mut self, names: &[&str]) -> Self {
        self.devices.extend(names.iter().map(|s| s.to_string()));
        self
    }

    /// Add a single device by name.
    pub(crate) fn device(mut self, name: &str) -> Self {
        self.devices.push(name.to_string());
        self
    }

    /// Add a device with override configuration.
    pub(crate) fn device_with_override(mut self, name: &str, config: DeviceOverrideConfig) -> Self {
        self.devices.push(name.to_string());
        self.device_overrides.insert(name.to_string(), config);
        self
    }

    /// Add an extra EAL argument.
    pub(crate) fn eal_arg(mut self, arg: &str) -> Self {
        self.extra_eal_args.push(arg.to_string());
        self
    }

    /// Add multiple EAL arguments.
    pub(crate) fn eal_args(mut self, args: &[&str]) -> Self {
        self.extra_eal_args
            .extend(args.iter().map(|s| s.to_string()));
        self
    }

    /// Get the extra EAL arguments.
    pub(crate) fn get_eal_args(&self) -> &[String] {
        &self.extra_eal_args
    }

    /// Get the configured devices.
    pub(crate) fn get_devices(&self) -> &[String] {
        &self.devices
    }

    /// Sets the number of scheduler ticks between checking for external events.
    /// This mirrors `Builder::event_interval`.
    pub(crate) fn event_interval(mut self, val: u32) -> Self {
        assert!(val > 0, "event_interval must be greater than 0");
        self.scheduler_config.event_interval = val;
        self
    }

    /// Sets the number of scheduler ticks after which the worker thread will
    /// try to steal tasks from the global queue.
    /// This mirrors `Builder::global_queue_interval`.
    pub(crate) fn global_queue_interval(mut self, val: u32) -> Self {
        assert!(val > 0, "global_queue_interval must be greater than 0");
        self.scheduler_config.global_queue_interval = Some(val);
        self
    }

    /// Disables the LIFO slot optimization.
    /// This mirrors `Builder::disable_lifo_slot`.
    pub(crate) fn disable_lifo_slot(mut self) -> Self {
        self.scheduler_config.disable_lifo_slot = true;
        self
    }

    /// Get the scheduler configuration.
    pub(crate) fn get_scheduler_config(&self) -> &DpdkSchedulerConfig {
        &self.scheduler_config
    }

    /// Resolve all devices and return their configurations.
    pub(crate) fn resolve_devices(&self) -> io::Result<Vec<ResolvedDevice>> {
        use super::resolve::resolve_device;

        let mut resolved = Vec::with_capacity(self.devices.len());
        for name in &self.devices {
            let override_config = self.device_overrides.get(name);
            let device_override = override_config.map(|c| c.to_device_override());
            let device = resolve_device(name, device_override.as_ref())?;
            resolved.push(device);
        }
        Ok(resolved)
    }

    /// Validate the configuration.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if self.devices.is_empty() {
            return Err(ConfigError {
                message: "at least one device must be specified".to_string(),
            });
        }

        Ok(())
    }
}

impl DeviceOverrideConfig {
    fn to_device_override(&self) -> DeviceOverride {
        use smoltcp::wire::{IpCidr, Ipv4Address, Ipv6Address};

        DeviceOverride {
            addresses: self.addresses.as_ref().map(|addrs| {
                addrs
                    .iter()
                    .filter_map(|s| s.parse::<IpCidr>().ok())
                    .collect()
            }),
            gateway_v4: self
                .gateway_v4
                .as_ref()
                .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
                .map(|a| Ipv4Address::from_bytes(&a.octets())),
            gateway_v6: self
                .gateway_v6
                .as_ref()
                .and_then(|s| s.parse::<std::net::Ipv6Addr>().ok())
                .map(|a| Ipv6Address::from_bytes(&a.octets())),
            core: self.core,
        }
    }
}

/// Error type for configuration validation.
#[derive(Debug)]
pub(crate) struct ConfigError {
    pub(crate) message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DPDK config error: {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for io::Error {
    fn from(err: ConfigError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dpdk_builder_empty_devices() {
        let builder = DpdkBuilder::new();
        assert!(builder.validate().is_err());
    }

    #[test]
    fn test_dpdk_builder_with_devices() {
        let builder = DpdkBuilder::new().devices(&["eth0", "eth1"]);
        assert!(builder.validate().is_ok());
        assert_eq!(builder.get_devices().len(), 2);
    }

    #[test]
    fn test_dpdk_builder_with_eal_args() {
        let builder = DpdkBuilder::new()
            .device("eth0")
            .eal_arg("--no-huge")
            .eal_args(&["-m", "128"]);
        assert!(builder.validate().is_ok());
        assert_eq!(builder.get_eal_args().len(), 3);
    }

    #[test]
    fn test_dpdk_builder_scheduler_config() {
        let builder = DpdkBuilder::new()
            .device("eth0")
            .event_interval(100)
            .global_queue_interval(50)
            .disable_lifo_slot();

        assert!(builder.validate().is_ok());
        assert_eq!(builder.get_scheduler_config().event_interval, 100);
        assert_eq!(
            builder.get_scheduler_config().global_queue_interval,
            Some(50)
        );
        assert!(builder.get_scheduler_config().disable_lifo_slot);
    }
}

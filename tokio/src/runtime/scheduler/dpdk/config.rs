//! DPDK runtime configuration.
//!
//! This module provides the configuration API for the DPDK scheduler.
//! The main entry point is `DpdkBuilder` which allows specifying network
//! devices by name and automatically resolves their configuration from
//! the system.

use std::collections::HashMap;
use std::io;

#[cfg(feature = "dpdk")]
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
pub struct DpdkBuilder {
    /// Device names to use
    devices: Vec<String>,

    /// Per-device overrides
    device_overrides: HashMap<String, DeviceOverrideConfig>,

    /// Extra EAL arguments
    extra_eal_args: Vec<String>,

    /// Scheduler-specific config
    scheduler_config: DpdkSchedulerConfig,
}

/// Device override configuration (public API wrapper).
#[derive(Debug, Clone, Default)]
pub struct DeviceOverrideConfig {
    /// Override IP addresses
    pub addresses: Option<Vec<String>>,

    /// Override IPv4 gateway
    pub gateway_v4: Option<String>,

    /// Override IPv6 gateway
    pub gateway_v6: Option<String>,

    /// Override CPU core
    pub core: Option<usize>,
}

/// Scheduler-specific configuration.
#[derive(Debug, Clone)]
pub struct DpdkSchedulerConfig {
    /// How often to run maintenance tasks (in ticks).
    pub event_interval: u32,

    /// How often to check the global injection queue (None for auto-tuning).
    pub global_queue_interval: Option<u32>,

    /// Whether to disable the LIFO slot optimization.
    pub disable_lifo_slot: bool,

    /// Callback to run before each poll cycle.
    pub before_poll: Option<fn()>,

    /// Callback to run after each poll cycle.
    pub after_poll: Option<fn()>,
}

impl Default for DpdkSchedulerConfig {
    fn default() -> Self {
        Self {
            event_interval: 61,
            global_queue_interval: None,
            disable_lifo_slot: false,
            before_poll: None,
            after_poll: None,
        }
    }
}

impl DpdkBuilder {
    /// Creates a new DPDK builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add devices by name. Device configuration (IP, gateway, MAC, CPU core)
    /// will be automatically resolved from the operating system.
    pub fn devices(mut self, names: &[&str]) -> Self {
        self.devices.extend(names.iter().map(|s| s.to_string()));
        self
    }

    /// Add a single device by name.
    pub fn device(mut self, name: &str) -> Self {
        self.devices.push(name.to_string());
        self
    }

    /// Add a device with override configuration.
    pub fn device_with_override(mut self, name: &str, config: DeviceOverrideConfig) -> Self {
        self.devices.push(name.to_string());
        self.device_overrides.insert(name.to_string(), config);
        self
    }

    /// Add an extra EAL argument.
    pub fn eal_arg(mut self, arg: &str) -> Self {
        self.extra_eal_args.push(arg.to_string());
        self
    }

    /// Add multiple EAL arguments.
    pub fn eal_args(mut self, args: &[&str]) -> Self {
        self.extra_eal_args
            .extend(args.iter().map(|s| s.to_string()));
        self
    }

    /// Set the event interval for maintenance tasks.
    pub fn event_interval(mut self, interval: u32) -> Self {
        self.scheduler_config.event_interval = interval;
        self
    }

    /// Set the global queue check interval.
    pub fn global_queue_interval(mut self, interval: u32) -> Self {
        self.scheduler_config.global_queue_interval = Some(interval);
        self
    }

    /// Disable the LIFO slot optimization.
    pub fn disable_lifo_slot(mut self) -> Self {
        self.scheduler_config.disable_lifo_slot = true;
        self
    }

    /// Set the before-poll callback.
    pub fn before_poll(mut self, callback: fn()) -> Self {
        self.scheduler_config.before_poll = Some(callback);
        self
    }

    /// Set the after-poll callback.
    pub fn after_poll(mut self, callback: fn()) -> Self {
        self.scheduler_config.after_poll = Some(callback);
        self
    }

    /// Get the configured devices.
    pub fn get_devices(&self) -> &[String] {
        &self.devices
    }

    /// Get the extra EAL arguments.
    pub fn get_eal_args(&self) -> &[String] {
        &self.extra_eal_args
    }

    /// Get the scheduler configuration.
    pub fn get_scheduler_config(&self) -> &DpdkSchedulerConfig {
        &self.scheduler_config
    }

    /// Resolve all devices and return their configurations.
    #[cfg(feature = "dpdk")]
    pub fn resolve_devices(&self) -> io::Result<Vec<ResolvedDevice>> {
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
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.devices.is_empty() {
            return Err(ConfigError {
                message: "at least one device must be specified".to_string(),
            });
        }

        if self.scheduler_config.event_interval == 0 {
            return Err(ConfigError {
                message: "event_interval must be greater than 0".to_string(),
            });
        }

        Ok(())
    }
}

#[cfg(feature = "dpdk")]
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
pub struct ConfigError {
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

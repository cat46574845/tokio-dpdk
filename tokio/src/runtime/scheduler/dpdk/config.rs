//! DPDK runtime configuration.
//!
//! This module provides the configuration API for the DPDK scheduler.
//! The main entry point is `DpdkBuilder` which allows specifying network
//! devices by name and automatically resolves their configuration from
//! the system.

use std::io;

use super::resolve::ResolvedDevice;

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

    /// Extra EAL arguments
    extra_eal_args: Vec<String>,
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

    /// Resolve all devices and return their configurations.
    pub(crate) fn resolve_devices(&self) -> io::Result<Vec<ResolvedDevice>> {
        use super::resolve::resolve_device;

        let mut resolved = Vec::with_capacity(self.devices.len());
        for name in &self.devices {
            let device = resolve_device(name)?;
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
}

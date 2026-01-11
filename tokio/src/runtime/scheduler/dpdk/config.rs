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
    /// Device names to use (resolved to MAC/IP from environment)
    devices: Vec<String>,

    /// Specific PCI addresses to use (multi-process mode)
    /// If specified, only these devices will be used.
    pci_addresses: Vec<String>,

    /// Maximum number of workers to create.
    /// If None, uses all available cores.
    num_workers: Option<usize>,

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

    /// Specify DPDK devices by PCI address.
    ///
    /// This is used for multi-process configurations where specific devices
    /// need to be allocated to this process.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let rt = Builder::new_dpdk()
    ///     .dpdk_devices(&["0000:28:00.0", "0000:29:00.0"])
    ///     .build()?;
    /// ```
    #[allow(dead_code)]
    pub(crate) fn dpdk_devices(mut self, pci_addresses: &[&str]) -> Self {
        self.pci_addresses
            .extend(pci_addresses.iter().map(|s| s.to_string()));
        self
    }

    /// Specify the number of worker threads to create.
    ///
    /// If not specified, uses all available cores. This is useful for
    /// multi-queue configurations where multiple workers share a single device.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Use 4 workers, even if more cores are available
    /// let rt = Builder::new_dpdk()
    ///     .devices(&["eth0"])
    ///     .dpdk_num_workers(4)
    ///     .build()?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error during build if `count` is 0.
    #[allow(dead_code)]
    pub(crate) fn dpdk_num_workers(mut self, count: usize) -> Self {
        self.num_workers = Some(count);
        self
    }

    /// Get the extra EAL arguments.
    pub(crate) fn get_eal_args(&self) -> &[String] {
        &self.extra_eal_args
    }

    /// Get the configured devices (by name).
    pub(crate) fn get_devices(&self) -> &[String] {
        &self.devices
    }

    /// Get the configured PCI addresses.
    pub(crate) fn get_pci_addresses(&self) -> &[String] {
        &self.pci_addresses
    }

    /// Get the configured number of workers.
    pub(crate) fn get_num_workers(&self) -> Option<usize> {
        self.num_workers
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
        // Must have either device names or PCI addresses
        if self.devices.is_empty() && self.pci_addresses.is_empty() {
            return Err(ConfigError {
                message: "at least one device must be specified (by name or PCI address)"
                    .to_string(),
            });
        }

        // If num_workers is specified, must be > 0
        if let Some(n) = self.num_workers {
            if n == 0 {
                return Err(ConfigError {
                    message: "num_workers must be greater than 0".to_string(),
                });
            }
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

    #[test]
    fn test_dpdk_builder_with_pci_addresses() {
        let builder = DpdkBuilder::new().dpdk_devices(&["0000:28:00.0", "0000:29:00.0"]);
        assert!(builder.validate().is_ok());
        assert_eq!(builder.get_pci_addresses().len(), 2);
        assert_eq!(builder.get_pci_addresses()[0], "0000:28:00.0");
    }

    #[test]
    fn test_dpdk_builder_with_num_workers() {
        let builder = DpdkBuilder::new()
            .dpdk_devices(&["0000:28:00.0"])
            .dpdk_num_workers(4);
        assert!(builder.validate().is_ok());
        assert_eq!(builder.get_num_workers(), Some(4));
    }

    #[test]
    fn test_dpdk_builder_zero_workers_error() {
        let builder = DpdkBuilder::new()
            .dpdk_devices(&["0000:28:00.0"])
            .dpdk_num_workers(0);
        let result = builder.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("num_workers"));
    }

    #[test]
    fn test_dpdk_builder_no_num_workers_uses_default() {
        let builder = DpdkBuilder::new().dpdk_devices(&["0000:28:00.0"]);
        assert_eq!(builder.get_num_workers(), None);
    }

    #[test]
    fn test_dpdk_builder_devices_and_pci_both_valid() {
        // Having both device names and PCI addresses is valid
        let builder = DpdkBuilder::new()
            .device("eth0")
            .dpdk_devices(&["0000:28:00.0"]);
        assert!(builder.validate().is_ok());
    }

    #[test]
    fn test_builder_dpdk_devices_filter() {
        // When PCI addresses are specified, only those devices should be used
        let builder = DpdkBuilder::new()
            .dpdk_devices(&["0000:28:00.0"])
            .dpdk_num_workers(2);
        assert_eq!(builder.get_pci_addresses().len(), 1);
        assert_eq!(builder.get_pci_addresses()[0], "0000:28:00.0");
    }

    #[test]
    fn test_builder_no_dpdk_devices_uses_all() {
        // When no PCI addresses specified, should use all available (empty list means "all")
        let builder = DpdkBuilder::new();
        assert!(builder.get_pci_addresses().is_empty());
        // Empty pci_addresses means "use all available devices" in AllocationPlan
    }
}

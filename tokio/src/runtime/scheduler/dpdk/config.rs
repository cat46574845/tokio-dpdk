//! DPDK runtime configuration.
//!
//! This module provides the configuration API for the DPDK scheduler.
//! The main entry point is `DpdkBuilder` which provides DPDK-specific
//! configuration options. Device configuration is read from env.json
//! via the AllocationPlan system.

use std::io;

/// Builder for DPDK runtime configuration.
///
/// # Example
///
/// ```ignore
/// use tokio::runtime::Builder;
///
/// // Simple usage: uses all available DPDK devices and cores from env.json
/// let rt = Builder::new_dpdk()
///     .enable_all()
///     .build()?;
///
/// // With specific PCI addresses
/// let rt = Builder::new_dpdk()
///     .dpdk_pci_addresses(&["0000:28:00.0"])
///     .worker_threads(4)
///     .build()?;
///
/// // With custom EAL arguments
/// let rt = Builder::new_dpdk()
///     .dpdk_eal_arg("--no-huge")
///     .dpdk_eal_arg("-m").dpdk_eal_arg("128")
///     .build()?;
/// ```
#[derive(Debug, Clone)]
pub(crate) struct DpdkBuilder {
    /// Specific PCI addresses to use.
    /// If empty, uses all available DPDK devices from env.json.
    pci_addresses: Vec<String>,

    /// Extra EAL arguments
    extra_eal_args: Vec<String>,

    /// Memory pool size (number of mbufs). Default: 8192.
    mempool_size: Option<u32>,

    /// Per-core cache size for mempool. Default: 256.
    cache_size: Option<u32>,

    /// Number of RX/TX queue descriptors per queue. Default: 128.
    queue_descriptors: Option<u16>,
}

impl Default for DpdkBuilder {
    fn default() -> Self {
        Self {
            pci_addresses: Vec::new(),
            extra_eal_args: Vec::new(),
            mempool_size: None,
            cache_size: None,
            queue_descriptors: None,
        }
    }
}

impl DpdkBuilder {
    /// Creates a new DPDK builder.
    pub(crate) fn new() -> Self {
        Self::default()
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
    /// If not specified, all available DPDK devices from env.json will be used.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let rt = Builder::new_dpdk()
    ///     .dpdk_pci_addresses(&["0000:28:00.0", "0000:29:00.0"])
    ///     .build()?;
    /// ```
    pub(crate) fn dpdk_devices(mut self, pci_addresses: &[&str]) -> Self {
        self.pci_addresses
            .extend(pci_addresses.iter().map(|s| s.to_string()));
        self
    }

    /// Set the memory pool size (number of mbufs).
    ///
    /// Default: 8192.
    pub(crate) fn mempool_size(mut self, size: u32) -> Self {
        self.mempool_size = Some(size);
        self
    }

    /// Set the per-core cache size for the memory pool.
    ///
    /// Default: 256.
    pub(crate) fn cache_size(mut self, size: u32) -> Self {
        self.cache_size = Some(size);
        self
    }

    /// Set the number of RX/TX queue descriptors per queue.
    ///
    /// Default: 128.
    pub(crate) fn queue_descriptors(mut self, desc: u16) -> Self {
        self.queue_descriptors = Some(desc);
        self
    }

    /// Get the extra EAL arguments.
    pub(crate) fn get_eal_args(&self) -> &[String] {
        &self.extra_eal_args
    }

    /// Get the configured PCI addresses.
    pub(crate) fn get_pci_addresses(&self) -> &[String] {
        &self.pci_addresses
    }

    /// Get the configured memory pool size (default: 8192).
    pub(crate) fn get_mempool_size(&self) -> u32 {
        self.mempool_size.unwrap_or(8192)
    }

    /// Get the configured cache size (default: 256).
    pub(crate) fn get_cache_size(&self) -> u32 {
        self.cache_size.unwrap_or(256)
    }

    /// Get the configured queue descriptors (default: 128).
    pub(crate) fn get_queue_descriptors(&self) -> u16 {
        self.queue_descriptors.unwrap_or(128)
    }

    /// Validate the configuration.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        // PCI addresses can be empty (means "use all available devices")
        // No further validation needed - env.json validation happens at build time
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
    fn test_dpdk_builder_empty_is_valid() {
        // Empty PCI list means "use all available devices" - valid
        let builder = DpdkBuilder::new();
        assert!(builder.validate().is_ok());
    }

    #[test]
    fn test_dpdk_builder_with_eal_args() {
        let builder = DpdkBuilder::new()
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
    fn test_dpdk_builder_default_mempool_size() {
        let builder = DpdkBuilder::new();
        assert_eq!(builder.get_mempool_size(), 8192);
        assert_eq!(builder.get_cache_size(), 256);
        assert_eq!(builder.get_queue_descriptors(), 128);
    }

    #[test]
    fn test_dpdk_builder_custom_mempool_size() {
        let builder = DpdkBuilder::new()
            .mempool_size(16384)
            .cache_size(512)
            .queue_descriptors(256);
        assert_eq!(builder.get_mempool_size(), 16384);
        assert_eq!(builder.get_cache_size(), 512);
        assert_eq!(builder.get_queue_descriptors(), 256);
    }

    #[test]
    fn test_dpdk_builder_no_pci_uses_all() {
        // When no PCI addresses specified, should use all available (empty list means "all")
        let builder = DpdkBuilder::new();
        assert!(builder.get_pci_addresses().is_empty());
    }
}

//! Device and core allocation for DPDK multi-process/multi-queue support.
//!
//! This module handles the allocation of DPDK devices, CPU cores, and IP addresses
//! to workers. It supports:
//!
//! - Multiple devices with single queue (one worker per device)
//! - Single device with multiple queues (multiple workers per device)
//! - IP address distribution across workers

use std::io;

use smoltcp::wire::{IpCidr, Ipv4Address, Ipv6Address};

use super::env_config::{DeviceConfig, DeviceRole, DpdkEnvConfig};
use super::resource_lock::ResourceLock;

/// Error type for allocation failures.
#[derive(Debug)]
pub(crate) enum AllocationError {
    /// No DPDK devices available
    NoDevicesAvailable,
    /// No CPU cores available
    NoCoresAvailable,
    /// Insufficient IP addresses for the number of workers
    InsufficientIps {
        device: String,
        workers: usize,
        ipv4_count: usize,
        ipv6_count: usize,
    },
    /// Device not found in configuration
    DeviceNotFound(String),
    /// IO error during lock acquisition
    LockError(io::Error),
}

impl std::fmt::Display for AllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationError::NoDevicesAvailable => {
                write!(
                    f,
                    "No DPDK devices available (all devices may be locked by other processes)"
                )
            }
            AllocationError::NoCoresAvailable => {
                write!(
                    f,
                    "No CPU cores available (all cores may be locked by other processes)"
                )
            }
            AllocationError::InsufficientIps {
                device,
                workers,
                ipv4_count,
                ipv6_count,
            } => {
                write!(
                    f,
                    "Device {} has insufficient IPs for {} workers (IPv4: {}, IPv6: {}). Each worker needs at least one IP.",
                    device, workers, ipv4_count, ipv6_count
                )
            }
            AllocationError::DeviceNotFound(pci) => {
                write!(f, "Device {} not found in configuration", pci)
            }
            AllocationError::LockError(e) => {
                write!(f, "Failed to acquire resource lock: {}", e)
            }
        }
    }
}

impl std::error::Error for AllocationError {}

impl From<io::Error> for AllocationError {
    fn from(e: io::Error) -> Self {
        AllocationError::LockError(e)
    }
}

impl From<AllocationError> for io::Error {
    fn from(e: AllocationError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
    }
}

/// Allocation plan describing how devices, cores, and IPs are distributed to workers.
#[derive(Debug, Clone)]
pub(crate) struct AllocationPlan {
    /// Per-worker allocation configuration.
    pub workers: Vec<WorkerAllocation>,
}

/// Configuration for a single worker.
#[derive(Debug, Clone)]
pub(crate) struct WorkerAllocation {
    /// DPDK device PCI address.
    pub pci_address: String,
    /// Queue ID on the device (for multi-queue support).
    pub queue_id: u16,
    /// CPU core to bind this worker to.
    pub core_id: usize,
    /// Assigned IPv4 address (if available).
    pub ipv4: Option<IpCidr>,
    /// Assigned IPv6 address (if available).
    pub ipv6: Option<IpCidr>,
    /// MAC address of the device.
    pub mac: [u8; 6],
    /// IPv4 gateway for this device.
    pub gateway_v4: Option<Ipv4Address>,
    /// IPv6 gateway for this device.
    pub gateway_v6: Option<Ipv6Address>,
    /// MTU for this device.
    #[allow(dead_code)]
    pub mtu: u16,
    /// Original interface name (for logging).
    #[allow(dead_code)]
    pub original_name: Option<String>,
}

/// Creates an allocation plan based on available resources and user requirements.
///
/// # Algorithm
///
/// 1. Filters available DPDK devices (role=dpdk, not locked)
/// 2. Filters available CPU cores (configured, not locked)
/// 3. Matches devices to cores:
///    - If cores < devices: use first N devices (by PCI address order)
///    - If cores >= devices: distribute cores across devices (multi-queue)
/// 4. Distributes IP addresses to workers on each device
///
/// # Arguments
///
/// * `env_config` - The DPDK environment configuration
/// * `requested_devices` - Optional list of specific devices to use (PCI addresses)
/// * `requested_num_workers` - Optional maximum number of workers to create
///
/// # Returns
///
/// An `AllocationPlan` describing the worker configuration, or an error.
pub(crate) fn create_allocation_plan(
    env_config: &DpdkEnvConfig,
    requested_devices: Option<&[String]>,
    requested_num_workers: Option<usize>,
) -> Result<AllocationPlan, AllocationError> {
    // 1. Get available DPDK devices
    let mut available_devices: Vec<&DeviceConfig> = env_config
        .devices
        .iter()
        .filter(|d| d.role == DeviceRole::Dpdk)
        .filter(|d| !ResourceLock::is_device_locked(&d.pci_address))
        .collect();

    // Filter by requested devices if specified
    if let Some(requested) = requested_devices {
        available_devices.retain(|d| requested.contains(&d.pci_address));

        // Check for missing devices
        for pci in requested {
            if !available_devices.iter().any(|d| &d.pci_address == pci) {
                return Err(AllocationError::DeviceNotFound(pci.clone()));
            }
        }
    }

    if available_devices.is_empty() {
        return Err(AllocationError::NoDevicesAvailable);
    }

    // Sort by PCI address for deterministic ordering
    available_devices.sort_by(|a, b| a.pci_address.cmp(&b.pci_address));

    // 2. Get available CPU cores
    let mut available_cores: Vec<usize> = get_available_cores(env_config);
    available_cores.retain(|&core| !ResourceLock::is_core_locked(core));

    if available_cores.is_empty() {
        return Err(AllocationError::NoCoresAvailable);
    }

    // Sort cores numerically
    available_cores.sort();

    // 3. Determine number of workers
    let num_devices = available_devices.len();
    let num_cores = available_cores.len();

    let num_workers = match requested_num_workers {
        Some(n) if n > 0 => n.min(num_cores),
        _ => num_cores,
    };

    // 4. Create worker allocations
    let mut workers = Vec::with_capacity(num_workers);

    if num_workers <= num_devices {
        // Case: fewer workers than devices - one worker per device, no multi-queue
        for (i, device) in available_devices.iter().take(num_workers).enumerate() {
            let ipv4 = device
                .addresses
                .iter()
                .find(|a| matches!(a.address(), smoltcp::wire::IpAddress::Ipv4(_)))
                .cloned();
            let ipv6 = device
                .addresses
                .iter()
                .find(|a| matches!(a.address(), smoltcp::wire::IpAddress::Ipv6(_)))
                .cloned();

            // Each worker needs at least one IP
            if ipv4.is_none() && ipv6.is_none() {
                return Err(AllocationError::InsufficientIps {
                    device: device.pci_address.clone(),
                    workers: 1,
                    ipv4_count: 0,
                    ipv6_count: 0,
                });
            }

            workers.push(WorkerAllocation {
                pci_address: device.pci_address.clone(),
                queue_id: 0,
                core_id: available_cores[i],
                ipv4,
                ipv6,
                mac: device.mac,
                gateway_v4: device.gateway_v4,
                gateway_v6: device.gateway_v6,
                mtu: device.mtu,
                original_name: device.original_name.clone(),
            });
        }
    } else {
        // Case: more workers than devices - multi-queue mode
        // Distribute workers across devices in round-robin fashion

        // First, count how many workers each device will get
        let mut device_worker_counts = vec![0usize; num_devices];
        for i in 0..num_workers {
            device_worker_counts[i % num_devices] += 1;
        }

        // Then allocate workers to each device
        let mut worker_idx = 0;
        for (device_idx, device) in available_devices.iter().enumerate() {
            let workers_for_device = device_worker_counts[device_idx];
            if workers_for_device == 0 {
                continue;
            }

            // Collect IPv4 and IPv6 addresses separately
            let ipv4_addrs: Vec<&IpCidr> = device
                .addresses
                .iter()
                .filter(|a| matches!(a.address(), smoltcp::wire::IpAddress::Ipv4(_)))
                .collect();
            let ipv6_addrs: Vec<&IpCidr> = device
                .addresses
                .iter()
                .filter(|a| matches!(a.address(), smoltcp::wire::IpAddress::Ipv6(_)))
                .collect();

            // Check if we have enough IPs
            let max_ips = ipv4_addrs.len().max(ipv6_addrs.len());
            if max_ips == 0 {
                return Err(AllocationError::InsufficientIps {
                    device: device.pci_address.clone(),
                    workers: workers_for_device,
                    ipv4_count: 0,
                    ipv6_count: 0,
                });
            }

            // Distribute IPs to workers for this device
            for queue_id in 0..workers_for_device {
                // Assign IPv4 if available (round-robin if not enough)
                let ipv4 = if !ipv4_addrs.is_empty() {
                    Some(*ipv4_addrs[queue_id % ipv4_addrs.len()])
                } else {
                    None
                };

                // Assign IPv6 if available (round-robin if not enough)
                let ipv6 = if !ipv6_addrs.is_empty() {
                    Some(*ipv6_addrs[queue_id % ipv6_addrs.len()])
                } else {
                    None
                };

                // Each worker needs at least one IP
                if ipv4.is_none() && ipv6.is_none() {
                    return Err(AllocationError::InsufficientIps {
                        device: device.pci_address.clone(),
                        workers: workers_for_device,
                        ipv4_count: ipv4_addrs.len(),
                        ipv6_count: ipv6_addrs.len(),
                    });
                }

                workers.push(WorkerAllocation {
                    pci_address: device.pci_address.clone(),
                    queue_id: queue_id as u16,
                    core_id: available_cores[worker_idx],
                    ipv4,
                    ipv6,
                    mac: device.mac,
                    gateway_v4: device.gateway_v4,
                    gateway_v6: device.gateway_v6,
                    mtu: device.mtu,
                    original_name: device.original_name.clone(),
                });

                worker_idx += 1;
            }
        }
    }

    Ok(AllocationPlan { workers })
}

/// Extracts available CPU cores from the environment configuration.
///
/// Requires `dpdk_cores` field to be set in the configuration.
fn get_available_cores(env_config: &DpdkEnvConfig) -> Vec<usize> {
    let mut cores = env_config.dpdk_cores.clone();
    cores.sort();
    cores.dedup();
    cores
}

/// Returns the number of queues needed for each device based on the allocation plan.
pub(crate) fn get_device_queue_counts(plan: &AllocationPlan) -> Vec<(String, u16)> {
    use std::collections::HashMap;

    let mut counts: HashMap<String, u16> = HashMap::new();

    for worker in &plan.workers {
        let entry = counts.entry(worker.pci_address.clone()).or_insert(0);
        *entry = (*entry).max(worker.queue_id + 1);
    }

    let mut result: Vec<_> = counts.into_iter().collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_device(pci: &str, ipv4: &str) -> DeviceConfig {
        DeviceConfig {
            pci_address: pci.to_string(),
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            addresses: vec![ipv4.parse().unwrap()],
            gateway_v4: Some(Ipv4Address::new(10, 0, 0, 1)),
            gateway_v6: None,
            mtu: 1500,
            role: DeviceRole::Dpdk,
            original_name: None,
        }
    }

    fn make_test_config(devices: Vec<DeviceConfig>, cores: Vec<usize>) -> DpdkEnvConfig {
        DpdkEnvConfig {
            devices,
            dpdk_cores: cores,
        }
    }

    #[test]
    fn test_allocation_single_device_single_core() {
        let config = make_test_config(
            vec![make_test_device("0000:01:00.0", "10.0.0.1/24")],
            vec![1],
        );

        let plan = create_allocation_plan(&config, None, None).unwrap();
        assert_eq!(plan.workers.len(), 1);
        assert_eq!(plan.workers[0].pci_address, "0000:01:00.0");
        assert_eq!(plan.workers[0].core_id, 1);
        assert_eq!(plan.workers[0].queue_id, 0);
    }

    #[test]
    fn test_allocation_multi_device_less_cores() {
        // 3 devices, 2 cores -> should use first 2 devices
        let config = make_test_config(
            vec![
                make_test_device("0000:03:00.0", "10.0.0.3/24"),
                make_test_device("0000:01:00.0", "10.0.0.1/24"),
                make_test_device("0000:02:00.0", "10.0.0.2/24"),
            ],
            vec![1, 2],
        );

        let plan = create_allocation_plan(&config, None, Some(2)).unwrap();
        assert_eq!(plan.workers.len(), 2);
        // Should be sorted by PCI address
        assert_eq!(plan.workers[0].pci_address, "0000:01:00.0");
        assert_eq!(plan.workers[1].pci_address, "0000:02:00.0");
    }

    #[test]
    fn test_allocation_no_ip_returns_error() {
        let mut device = make_test_device("0000:01:00.0", "10.0.0.1/24");
        device.addresses.clear();

        let config = make_test_config(vec![device], vec![1]);
        let result = create_allocation_plan(&config, None, None);

        assert!(matches!(
            result,
            Err(AllocationError::InsufficientIps { .. })
        ));
    }

    #[test]
    fn test_get_device_queue_counts() {
        let plan = AllocationPlan {
            workers: vec![
                WorkerAllocation {
                    pci_address: "0000:01:00.0".to_string(),
                    queue_id: 0,
                    core_id: 1,
                    ipv4: Some("10.0.0.1/24".parse().unwrap()),
                    ipv6: None,
                    mac: [0; 6],
                    gateway_v4: None,
                    gateway_v6: None,
                    mtu: 1500,
                    original_name: None,
                },
                WorkerAllocation {
                    pci_address: "0000:01:00.0".to_string(),
                    queue_id: 1,
                    core_id: 2,
                    ipv4: Some("10.0.0.2/24".parse().unwrap()),
                    ipv6: None,
                    mac: [0; 6],
                    gateway_v4: None,
                    gateway_v6: None,
                    mtu: 1500,
                    original_name: None,
                },
                WorkerAllocation {
                    pci_address: "0000:02:00.0".to_string(),
                    queue_id: 0,
                    core_id: 3,
                    ipv4: Some("10.0.0.3/24".parse().unwrap()),
                    ipv6: None,
                    mac: [0; 6],
                    gateway_v4: None,
                    gateway_v6: None,
                    mtu: 1500,
                    original_name: None,
                },
            ],
        };

        let counts = get_device_queue_counts(&plan);
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[0], ("0000:01:00.0".to_string(), 2)); // 2 queues
        assert_eq!(counts[1], ("0000:02:00.0".to_string(), 1)); // 1 queue
    }

    fn make_test_device_with_ips(pci: &str, ipv4s: &[&str], ipv6s: &[&str]) -> DeviceConfig {
        let mut addresses = Vec::new();
        for ip in ipv4s {
            addresses.push(ip.parse().unwrap());
        }
        for ip in ipv6s {
            addresses.push(ip.parse().unwrap());
        }
        DeviceConfig {
            pci_address: pci.to_string(),
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            addresses,
            gateway_v4: Some(Ipv4Address::new(10, 0, 0, 1)),
            gateway_v6: None,
            mtu: 1500,
            role: DeviceRole::Dpdk,
            original_name: None,
        }
    }

    #[test]
    fn test_allocation_single_device_multi_core() {
        // 1 device with 3 IPs, 3 cores -> each worker gets 1 IP
        let device = make_test_device_with_ips(
            "0000:01:00.0",
            &["10.0.0.1/24", "10.0.0.2/24", "10.0.0.3/24"],
            &[],
        );
        let config = make_test_config(vec![device], vec![1, 2, 3]);

        let plan = create_allocation_plan(&config, None, None).unwrap();
        assert_eq!(plan.workers.len(), 3);

        // Each worker should have its own queue on the same device
        assert_eq!(plan.workers[0].pci_address, "0000:01:00.0");
        assert_eq!(plan.workers[0].queue_id, 0);
        assert_eq!(plan.workers[1].queue_id, 1);
        assert_eq!(plan.workers[2].queue_id, 2);

        // Each worker should have its own IP
        assert!(plan.workers[0].ipv4.is_some());
        assert!(plan.workers[1].ipv4.is_some());
        assert!(plan.workers[2].ipv4.is_some());
    }

    #[test]
    fn test_allocation_single_device_multi_core_uneven_ip() {
        // 1 device with 2 IPs, 3 cores -> first 2 workers get IPs, third has none of that type
        let device = make_test_device_with_ips(
            "0000:01:00.0",
            &["10.0.0.1/24", "10.0.0.2/24"],
            &["fe80::1/64", "fe80::2/64", "fe80::3/64"], // Add IPv6 so third worker has IP
        );
        let config = make_test_config(vec![device], vec![1, 2, 3]);

        let plan = create_allocation_plan(&config, None, None).unwrap();
        assert_eq!(plan.workers.len(), 3);

        // First 2 workers get IPv4
        assert!(plan.workers[0].ipv4.is_some());
        assert!(plan.workers[1].ipv4.is_some());
        // Third worker might not have IPv4 (depends on distribution)
        // But should have IPv6
        assert!(plan.workers[2].ipv6.is_some());
    }

    #[test]
    fn test_allocation_multi_device_more_cores() {
        // 2 devices, 4 cores -> round-robin distribution
        let config = make_test_config(
            vec![
                make_test_device_with_ips("0000:01:00.0", &["10.0.0.1/24", "10.0.0.2/24"], &[]),
                make_test_device_with_ips("0000:02:00.0", &["10.0.0.3/24", "10.0.0.4/24"], &[]),
            ],
            vec![1, 2, 3, 4],
        );

        let plan = create_allocation_plan(&config, None, None).unwrap();
        assert_eq!(plan.workers.len(), 4);

        // Workers should be distributed across devices
        let device1_workers: Vec<_> = plan
            .workers
            .iter()
            .filter(|w| w.pci_address == "0000:01:00.0")
            .collect();
        let device2_workers: Vec<_> = plan
            .workers
            .iter()
            .filter(|w| w.pci_address == "0000:02:00.0")
            .collect();

        assert_eq!(device1_workers.len(), 2);
        assert_eq!(device2_workers.len(), 2);
    }

    #[test]
    fn test_allocation_no_available_devices() {
        // Config with only kernel devices (not DPDK)
        let mut device = make_test_device("0000:01:00.0", "10.0.0.1/24");
        device.role = DeviceRole::Kernel;
        let config = make_test_config(vec![device], vec![1]);

        let result = create_allocation_plan(&config, None, None);
        assert!(matches!(result, Err(AllocationError::NoDevicesAvailable)));
    }

    #[test]
    fn test_allocation_no_available_cores() {
        // Config with no cores
        let config = make_test_config(
            vec![make_test_device("0000:01:00.0", "10.0.0.1/24")],
            vec![], // No cores
        );

        let result = create_allocation_plan(&config, None, None);
        assert!(matches!(result, Err(AllocationError::NoCoresAvailable)));
    }

    #[test]
    fn test_allocation_device_sort_order() {
        // Devices should be sorted by PCI address (dictionary order)
        let config = make_test_config(
            vec![
                make_test_device("0000:29:00.0", "10.0.0.3/24"),
                make_test_device("0000:01:00.0", "10.0.0.1/24"),
                make_test_device("0000:28:00.0", "10.0.0.2/24"),
            ],
            vec![1, 2, 3],
        );

        let plan = create_allocation_plan(&config, None, Some(3)).unwrap();
        assert_eq!(plan.workers.len(), 3);
        // Should be sorted: 0000:01:00.0, 0000:28:00.0, 0000:29:00.0
        assert_eq!(plan.workers[0].pci_address, "0000:01:00.0");
        assert_eq!(plan.workers[1].pci_address, "0000:28:00.0");
        assert_eq!(plan.workers[2].pci_address, "0000:29:00.0");
    }

    #[test]
    fn test_allocation_ipv6_distribution() {
        // Test IPv6 address distribution
        let device = make_test_device_with_ips(
            "0000:01:00.0",
            &[],
            &["2001:db8::1/64", "2001:db8::2/64", "2001:db8::3/64"],
        );
        let config = make_test_config(vec![device], vec![1, 2, 3]);

        let plan = create_allocation_plan(&config, None, None).unwrap();
        assert_eq!(plan.workers.len(), 3);

        // Each worker should have IPv6
        assert!(plan.workers[0].ipv6.is_some());
        assert!(plan.workers[1].ipv6.is_some());
        assert!(plan.workers[2].ipv6.is_some());
    }
}

//! DPDK initialization module.
//!
//! This module provides functions for initializing DPDK:
//! - EAL (Environment Abstraction Layer) initialization
//! - Memory pool creation
//! - Port/queue setup
//! - CPU affinity management

use std::ffi::CString;
use std::io;

use super::device::DpdkDevice;
use super::ffi;

/// A fully initialized DPDK worker for multi-queue mode.
///
/// Each worker gets its own queue on a shared device.
pub(crate) struct InitializedWorker {
    /// The DPDK device for smoltcp (with specific queue_id)
    pub device: DpdkDevice,
    /// MAC address
    pub mac: [u8; 6],
    /// IP addresses
    pub addresses: Vec<smoltcp::wire::IpCidr>,
    /// IPv4 gateway
    pub gateway_v4: Option<smoltcp::wire::Ipv4Address>,
    /// IPv6 gateway
    pub gateway_v6: Option<smoltcp::wire::Ipv6Address>,
    /// CPU core for this worker
    pub core_id: usize,
    /// Queue ID on the device
    #[allow(dead_code)]
    pub queue_id: u16,
    /// DPDK port ID
    pub port_id: u16,
}

/// Holds DPDK resources for cleanup when the last Arc<Handle> is dropped.
///
/// This struct is stored in `Shared` and is dropped AFTER `drivers` (due to field order),
/// ensuring all DpdkDevice instances have released their mbufs before mempool is freed.
///
/// Drop order in Shared:
/// 1. drivers (DpdkDriver -> DpdkDevice::drop() frees mbufs)
/// 2. dpdk_resources (this struct - frees mempool and calls rte_eal_cleanup())
pub(crate) struct DpdkResourcesCleaner {
    /// Memory pool pointer to free
    mempool: *mut ffi::rte_mempool,
    /// Port IDs to stop and close
    ports: Vec<u16>,
}

// Safety: mempool is only accessed during Drop, which happens after all workers have stopped.
unsafe impl Send for DpdkResourcesCleaner {}
unsafe impl Sync for DpdkResourcesCleaner {}

impl DpdkResourcesCleaner {
    /// Creates a new cleaner that will free the mempool and cleanup EAL when dropped.
    pub(crate) fn new(mempool: *mut ffi::rte_mempool, ports: Vec<u16>) -> Self {
        Self { mempool, ports }
    }
}

impl Drop for DpdkResourcesCleaner {
    fn drop(&mut self) {
        // 1. Stop and close all ports
        for &port_id in &self.ports {
            unsafe {
                let _ = ffi::rte_eth_dev_stop(port_id);
                let _ = ffi::rte_eth_dev_close(port_id);
            }
        }

        // 2. Free mempool
        if !self.mempool.is_null() {
            unsafe {
                ffi::rte_mempool_free(self.mempool);
            }
        }

        // 3. Clean up EAL
        unsafe {
            let _ = ffi::rte_eal_cleanup();
        }
    }
}

/// Initialize DPDK EAL (Environment Abstraction Layer).
///
/// This must be called before any other DPDK functions.
pub(crate) fn init_eal(args: &[String]) -> io::Result<()> {
    let c_args: Vec<CString> = args
        .iter()
        .map(|s| CString::new(s.as_str()).expect("Invalid EAL argument"))
        .collect();

    let mut c_ptrs: Vec<*mut i8> = c_args.iter().map(|s| s.as_ptr() as *mut i8).collect();

    let ret = unsafe { ffi::rte_eal_init(c_ptrs.len() as i32, c_ptrs.as_mut_ptr()) };

    if ret < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("rte_eal_init failed with code: {}", ret),
        ));
    }

    Ok(())
}

/// Create DPDK memory pool for packet buffers.
pub(crate) fn create_mempool(
    name: &str,
    n_mbufs: u32,
    cache_size: u32,
) -> io::Result<*mut ffi::rte_mempool> {
    let c_name = CString::new(name).expect("Invalid mempool name");

    let pool = unsafe {
        ffi::rte_pktmbuf_pool_create(
            c_name.as_ptr(),
            n_mbufs,
            cache_size,
            0,    // priv_size
            2048, // data_room_size (RTE_MBUF_DEFAULT_BUF_SIZE)
            ffi::rte_socket_id() as i32,
        )
    };

    if pool.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to create mbuf pool",
        ));
    }

    Ok(pool)
}

/// Initialize a DPDK port.
pub(crate) fn init_port(
    port_id: u16,
    mempool: *mut ffi::rte_mempool,
    nb_rx_queues: u16,
    nb_tx_queues: u16,
    nb_descriptors: u16,
) -> io::Result<()> {
    // Get device info
    let mut dev_info: ffi::rte_eth_dev_info = unsafe { std::mem::zeroed() };
    let ret = unsafe { ffi::rte_eth_dev_info_get(port_id, &mut dev_info) };
    if ret != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to get device info for port {}", port_id),
        ));
    }

    // Configure the port
    let port_conf: ffi::rte_eth_conf = unsafe { std::mem::zeroed() };
    let ret =
        unsafe { ffi::rte_eth_dev_configure(port_id, nb_rx_queues, nb_tx_queues, &port_conf) };
    if ret != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to configure port {}: {}", port_id, ret),
        ));
    }

    // Setup RX queues
    let socket_id = unsafe { ffi::rte_eth_dev_socket_id(port_id) };
    for queue_id in 0..nb_rx_queues {
        let ret = unsafe {
            ffi::rte_eth_rx_queue_setup(
                port_id,
                queue_id,
                nb_descriptors,
                socket_id as u32,
                std::ptr::null(),
                mempool,
            )
        };
        if ret != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to setup RX queue {} for port {}", queue_id, port_id),
            ));
        }
    }

    // Setup TX queues
    for queue_id in 0..nb_tx_queues {
        let ret = unsafe {
            ffi::rte_eth_tx_queue_setup(
                port_id,
                queue_id,
                nb_descriptors,
                socket_id as u32,
                std::ptr::null(),
            )
        };
        if ret != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to setup TX queue {} for port {}", queue_id, port_id),
            ));
        }
    }

    // Start the port
    let ret = unsafe { ffi::rte_eth_dev_start(port_id) };
    if ret != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to start port {}: {}", port_id, ret),
        ));
    }

    // Enable promiscuous mode
    let ret = unsafe { ffi::rte_eth_promiscuous_enable(port_id) };
    if ret != 0 {
        // Non-fatal, just log
        eprintln!(
            "Warning: Failed to enable promiscuous mode for port {}",
            port_id
        );
    }

    // Enable allmulticast mode (required for IPv6 NDP neighbor discovery)
    let ret = unsafe { ffi::rte_eth_allmulticast_enable(port_id) };
    if ret != 0 {
        eprintln!(
            "Warning: Failed to enable allmulticast mode for port {} (IPv6 may not work)",
            port_id
        );
    }

    Ok(())
}

/// Get MAC address for a DPDK port.
pub(crate) fn get_mac_address(port_id: u16) -> io::Result<[u8; 6]> {
    let mut mac: ffi::rte_ether_addr = unsafe { std::mem::zeroed() };
    let ret = unsafe { ffi::rte_eth_macaddr_get(port_id, &mut mac) };

    if ret != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to get MAC address for port {}", port_id),
        ));
    }

    Ok(mac.addr_bytes)
}

/// Find DPDK port by MAC address.
pub(crate) fn find_port_by_mac(target_mac: &[u8; 6]) -> io::Result<Option<u16>> {
    let n_ports = unsafe { ffi::rte_eth_dev_count_avail() };

    for port_id in 0..n_ports {
        if let Ok(mac) = get_mac_address(port_id) {
            if &mac == target_mac {
                return Ok(Some(port_id));
            }
        }
    }

    Ok(None)
}

/// Generate EAL arguments from allocation plan.
pub(crate) fn generate_eal_args_from_plan(
    plan: &super::allocation::AllocationPlan,
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec!["tokio-dpdk".to_string()];

    // Generate core list from allocation plan
    let mut cores: Vec<usize> = plan.workers.iter().map(|w| w.core_id).collect();
    cores.sort();
    cores.dedup();

    if !cores.is_empty() {
        let core_list: String = cores
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");
        args.push("-l".to_string());
        args.push(core_list);
    }

    // Add extra user-provided arguments
    args.extend(extra_args.iter().cloned());

    args
}

/// Initialize DPDK resources based on an allocation plan.
///
/// This function:
/// 1. Initializes DPDK EAL with cores from the allocation plan
/// 2. Creates mempool with configurable size and cache
/// 3. Initializes each unique port with the required number of queues
/// 4. Creates InitializedWorker for each worker allocation
pub(crate) fn initialize_dpdk_from_plan(
    plan: &super::allocation::AllocationPlan,
    eal_args: &[String],
    mempool_size: u32,
    cache_size: u32,
    queue_descriptors: u16,
) -> io::Result<(DpdkResourcesMultiQueue, Vec<InitializedWorker>)> {
    use super::allocation::get_device_queue_counts;
    use std::collections::HashMap;

    if plan.workers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Allocation plan has no workers",
        ));
    }

    // 1. Generate and call EAL init
    let full_eal_args = generate_eal_args_from_plan(plan, eal_args);
    init_eal(&full_eal_args)?;

    // 2. Create mempool with configurable parameters
    let mempool = create_mempool("tokio_dpdk_mbuf_pool", mempool_size, cache_size)?;

    // 3. Get queue counts per device
    let queue_counts = get_device_queue_counts(plan);

    // Track port_id -> num_queues mapping and initialized ports
    let mut pci_to_port: HashMap<String, u16> = HashMap::new();
    let mut initialized_ports = Vec::new();

    // Initialize each unique device
    for (pci_address, num_queues) in &queue_counts {
        // Find port by MAC (we need to look up MAC from allocation)
        let worker = plan
            .workers
            .iter()
            .find(|w| &w.pci_address == pci_address)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("No worker found for device {}", pci_address),
                )
            })?;

        let port_id = find_port_by_mac(&worker.mac)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Device {} not found by MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    pci_address,
                    worker.mac[0],
                    worker.mac[1],
                    worker.mac[2],
                    worker.mac[3],
                    worker.mac[4],
                    worker.mac[5]
                ),
            )
        })?;

        // Initialize port with required number of queues and configurable descriptors
        init_port(port_id, mempool, *num_queues, *num_queues, queue_descriptors)?;

        pci_to_port.insert(pci_address.clone(), port_id);
        initialized_ports.push(port_id);
    }

    // 4. Create InitializedWorker for each allocation
    let mut workers = Vec::with_capacity(plan.workers.len());

    for allocation in &plan.workers {
        let port_id = *pci_to_port.get(&allocation.pci_address).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Port not initialized for {}", allocation.pci_address),
            )
        })?;

        // Build addresses list from allocation
        let mut addresses = Vec::new();
        if let Some(ipv4) = allocation.ipv4 {
            addresses.push(ipv4);
        }
        if let Some(ipv6) = allocation.ipv6 {
            addresses.push(ipv6);
        }

        // Create DpdkDevice with specific queue_id
        let dpdk_device = unsafe { DpdkDevice::new(port_id, allocation.queue_id, mempool) };

        workers.push(InitializedWorker {
            device: dpdk_device,
            mac: allocation.mac,
            addresses,
            gateway_v4: allocation.gateway_v4,
            gateway_v6: allocation.gateway_v6,
            core_id: allocation.core_id,
            queue_id: allocation.queue_id,
            port_id,
        });
    }

    Ok((
        DpdkResourcesMultiQueue {
            mempool,
            ports: initialized_ports,
        },
        workers,
    ))
}

/// DPDK resources for multi-queue mode.
pub(crate) struct DpdkResourcesMultiQueue {
    /// Memory pool for packet buffers.
    pub mempool: *mut ffi::rte_mempool,
    /// List of initialized port IDs.
    pub ports: Vec<u16>,
}

// Safety: DPDK mempools are thread-safe
unsafe impl Send for DpdkResourcesMultiQueue {}
unsafe impl Sync for DpdkResourcesMultiQueue {}

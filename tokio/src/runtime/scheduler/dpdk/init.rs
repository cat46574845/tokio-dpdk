//! DPDK initialization module.
//!
//! This module provides functions for initializing DPDK:
//! - EAL (Environment Abstraction Layer) initialization
//! - Memory pool creation
//! - Port/queue setup
//! - CPU affinity management

use std::ffi::CString;
use std::io;

use super::config::DpdkBuilder;
use super::device::DpdkDevice;
use super::ffi;
use super::resolve::{resolve_device, ResolvedDevice};

/// Initialized DPDK runtime resources.
pub struct DpdkResources {
    /// Memory pool for packet buffers
    pub mempool: *mut ffi::rte_mempool,
    /// Initialized devices (one per network interface)
    pub devices: Vec<InitializedDevice>,
}

/// A fully initialized DPDK device ready for use.
pub struct InitializedDevice {
    /// The resolved device configuration
    pub config: ResolvedDevice,
    /// The DPDK device for smoltcp
    pub device: DpdkDevice,
    /// DPDK port ID
    pub port_id: u16,
}

impl DpdkResources {
    /// Clean up DPDK resources.
    pub fn cleanup(&mut self) {
        // Note: In a full implementation, we would properly cleanup
        // DPDK resources here. For now, this is a placeholder.
    }
}

/// Initialize DPDK EAL (Environment Abstraction Layer).
///
/// This must be called before any other DPDK functions.
pub fn init_eal(args: &[String]) -> io::Result<()> {
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
pub fn create_mempool(
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
pub fn init_port(
    port_id: u16,
    mempool: *mut ffi::rte_mempool,
    nb_rx_queues: u16,
    nb_tx_queues: u16,
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
                128, // nb_rx_desc
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
                128, // nb_tx_desc
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

    Ok(())
}

/// Get MAC address for a DPDK port.
pub fn get_mac_address(port_id: u16) -> io::Result<[u8; 6]> {
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
pub fn find_port_by_mac(target_mac: &[u8; 6]) -> io::Result<Option<u16>> {
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

/// Generate EAL arguments from resolved devices.
pub fn generate_eal_args(devices: &[ResolvedDevice], extra_args: &[String]) -> Vec<String> {
    let mut args = vec!["tokio-dpdk".to_string()];

    // Generate core list from device configurations
    let cores: Vec<usize> = devices.iter().map(|d| d.core).collect();
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

/// Initialize all DPDK resources based on builder configuration.
pub fn initialize_dpdk(builder: &DpdkBuilder) -> io::Result<DpdkResources> {
    // 1. Resolve all devices
    let resolved_devices = builder.resolve_devices()?;

    if resolved_devices.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "No devices configured",
        ));
    }

    // 2. Generate and call EAL init
    let eal_args = generate_eal_args(&resolved_devices, builder.get_eal_args());
    init_eal(&eal_args)?;

    // 3. Create mempool
    let mempool = create_mempool("tokio_dpdk_mbuf_pool", 8192, 256)?;

    // 4. Initialize each device
    let mut devices = Vec::with_capacity(resolved_devices.len());

    for (index, resolved) in resolved_devices.into_iter().enumerate() {
        // Find DPDK port by MAC address
        let port_id = find_port_by_mac(&resolved.mac)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Device {} not found by MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    resolved.name,
                    resolved.mac[0],
                    resolved.mac[1],
                    resolved.mac[2],
                    resolved.mac[3],
                    resolved.mac[4],
                    resolved.mac[5]
                ),
            )
        })?;

        // Initialize port with 1 RX queue and 1 TX queue
        init_port(port_id, mempool, 1, 1)?;

        // Create DpdkDevice
        let dpdk_device = unsafe { DpdkDevice::new(port_id, index as u16, mempool) };

        devices.push(InitializedDevice {
            config: resolved,
            device: dpdk_device,
            port_id,
        });
    }

    Ok(DpdkResources { mempool, devices })
}

/// Set CPU affinity for the current thread.
#[cfg(target_os = "linux")]
pub fn set_cpu_affinity(core: usize) -> io::Result<()> {
    use std::mem;

    let mut cpuset: libc::cpu_set_t = unsafe { mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(core, &mut cpuset);

        let result = libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &cpuset);
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_cpu_affinity(_core: usize) -> io::Result<()> {
    // CPU affinity is not supported on this platform
    Ok(())
}

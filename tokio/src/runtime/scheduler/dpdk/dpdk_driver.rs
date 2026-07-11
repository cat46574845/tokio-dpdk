//! DPDK Driver for smoltcp integration.
//!
//! This module provides:
//! - `DpdkDriver`: Network stack driver managing DpdkDevice + smoltcp Interface + SocketSet
//!
//! The DpdkDriver is the central component that bridges DPDK packet I/O with
//! the smoltcp TCP/IP stack, enabling async socket operations.
//!
//! Note: Waker management is handled by smoltcp's native waker mechanism
//! (`register_recv_waker`/`register_send_waker`), not by a separate ScheduledIo.

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::task::Waker;
use std::time::{Duration, Instant};

use smoltcp::iface::{
    Config as IfaceConfig, GatewayNeighborProbeError, GatewayNeighborProbeResult, Interface,
    PollEgressHandleResult, PollIngressSingleResult, PollResult, SocketHandle, SocketSet,
    TcpFlowCacheError,
};
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::Socket as SmolSocket;
use smoltcp::storage::LinearBuffer;
use smoltcp::time::{Duration as SmolDuration, Instant as SmolInstant};
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address,
    Ipv6Address,
};

use super::device::DpdkDevice;
use super::raw_tail::{RawTailHandle, RawTailRecord, RawTailTable, RawTailTuple};

// =============================================================================
// Constants
// =============================================================================

/// Default TCP RX buffer size
/// Increased from 64KB to 512KB to reduce window-zero situations
/// which can cause ~RTT delays when buffer fills up.
/// Larger buffer provides more headroom for bursty traffic.
const TCP_RX_BUFFER_SIZE: usize = 524288;

/// Default TCP TX buffer size
const TCP_TX_BUFFER_SIZE: usize = 65536;

/// Default buffer pool size (number of connections)
const DEFAULT_BUFFER_POOL_SIZE: usize = 2048;
const GATEWAY_SOFT_STALE_AFTER: SmolDuration = SmolDuration::from_secs(300);
const INFRA_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(1);
const PENDING_EGRESS_NONE: usize = usize::MAX;
#[cfg(feature = "market-trace")]
const TRACE_AUX_FIELD_BITS: u64 = 21;
#[cfg(feature = "market-trace")]
const TRACE_AUX_FIELD_MASK: u64 = (1u64 << TRACE_AUX_FIELD_BITS) - 1;

#[cfg(feature = "market-trace")]
#[inline(always)]
fn pack_trace_aux3(a: usize, b: usize, c: usize) -> u64 {
    let a = a as u64;
    let b = b as u64;
    let c = c as u64;
    assert!(a <= TRACE_AUX_FIELD_MASK, "market trace aux field a overflow value={}", a);
    assert!(b <= TRACE_AUX_FIELD_MASK, "market trace aux field b overflow value={}", b);
    assert!(c <= TRACE_AUX_FIELD_MASK, "market trace aux field c overflow value={}", c);
    a | (b << TRACE_AUX_FIELD_BITS) | (c << (TRACE_AUX_FIELD_BITS * 2))
}

fn dpdk_iface_config(mac: [u8; 6]) -> IfaceConfig {
    let mut config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.tcp_flow_cache_capacity = DEFAULT_BUFFER_POOL_SIZE;
    config
}

#[inline(always)]
fn socket_egress_retry_due(
    device_exhausted: bool,
    now: SmolInstant,
    poll_at: Option<SmolInstant>,
) -> Option<SmolInstant> {
    if device_exhausted {
        Some(now)
    } else {
        poll_at
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct GatewayProbeErrorCounters {
    non_ethernet_medium: u64,
    non_ipv4_gateway: u64,
    no_source_address: u64,
    dispatch_failed: u64,
    gateway_changed: u64,
}

impl GatewayProbeErrorCounters {
    fn record(&mut self, error: GatewayNeighborProbeError) {
        let counter = match error {
            GatewayNeighborProbeError::NonEthernetMedium => &mut self.non_ethernet_medium,
            GatewayNeighborProbeError::NonIpv4Gateway => &mut self.non_ipv4_gateway,
            GatewayNeighborProbeError::NoSourceAddress => &mut self.no_source_address,
            GatewayNeighborProbeError::DispatchFailed => &mut self.dispatch_failed,
            GatewayNeighborProbeError::GatewayChanged => &mut self.gateway_changed,
        };
        *counter = counter.saturating_add(1);
    }

    #[inline(always)]
    fn is_empty(self) -> bool {
        self.non_ethernet_medium == 0
            && self.non_ipv4_gateway == 0
            && self.no_source_address == 0
            && self.dispatch_failed == 0
            && self.gateway_changed == 0
    }
}
// =============================================================================
// TcpBufferPool - Pre-allocated buffer management
// =============================================================================

/// Pre-allocated buffer pool for TCP socket buffers.
///
/// Provides zero-allocation socket creation by reusing a fixed startup pool.
pub(crate) struct TcpBufferPool {
    /// Free RX buffers available for allocation
    rx_free: Vec<Vec<u8>>,
    /// Free TX buffers available for allocation
    tx_free: Vec<Vec<u8>>,
    /// Maximum pool capacity
    capacity: usize,
    /// Required RX buffer size
    rx_buffer_size: usize,
    /// Required TX buffer size
    tx_buffer_size: usize,
    /// Fixed listener waiter slots. A slot is allocated only while a listener
    /// is blocked on this pool, and never grows after driver startup.
    waiter_slots: Vec<TcpBufferWaiterSlot>,
    /// Free waiter-slot indices, also fully allocated at driver startup.
    waiter_free: Vec<usize>,
    /// Dense fixed bitset of waiter slots which currently contain a waker.
    waiter_registered_bits: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TcpBufferWaiterHandle {
    index: usize,
    generation: u64,
}

struct TcpBufferWaiterSlot {
    generation: u64,
    allocated: bool,
    waker: Option<Waker>,
}

#[derive(Debug, PartialEq, Eq)]
enum TcpBufferWaiterError {
    Full { capacity: usize },
    InvalidHandle { index: usize },
    StaleHandle { index: usize },
    DuplicateFree { index: usize },
}

impl std::fmt::Display for TcpBufferWaiterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full { capacity } => {
                write!(f, "TCP buffer listener waiter capacity exhausted capacity={}", capacity)
            }
            Self::InvalidHandle { index } => {
                write!(f, "invalid TCP buffer listener waiter index={}", index)
            }
            Self::StaleHandle { index } => {
                write!(f, "stale TCP buffer listener waiter index={}", index)
            }
            Self::DuplicateFree { index } => {
                write!(f, "TCP buffer listener waiter already released index={}", index)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TcpBufferPoolReleaseError {
    RxLength { expected: usize, actual: usize },
    TxLength { expected: usize, actual: usize },
    Imbalanced { rx_available: usize, tx_available: usize },
    Full { capacity: usize },
}

impl std::fmt::Display for TcpBufferPoolReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RxLength { expected, actual } => {
                write!(f, "TCP RX buffer length mismatch expected={} actual={}", expected, actual)
            }
            Self::TxLength { expected, actual } => {
                write!(f, "TCP TX buffer length mismatch expected={} actual={}", expected, actual)
            }
            Self::Imbalanced {
                rx_available,
                tx_available,
            } => write!(
                f,
                "TCP buffer pool is imbalanced rx_available={} tx_available={}",
                rx_available, tx_available
            ),
            Self::Full { capacity } => {
                write!(f, "TCP buffer pool release exceeds fixed capacity={}", capacity)
            }
        }
    }
}

impl TcpBufferPool {
    /// Create a new buffer pool with pre-allocated buffers.
    ///
    /// # Arguments
    /// * `capacity` - Number of connection buffer pairs to pre-allocate
    /// * `rx_size` - Size of each RX buffer
    /// * `tx_size` - Size of each TX buffer
    pub(crate) fn new(capacity: usize, rx_size: usize, tx_size: usize) -> Self {
        let mut rx_free = Vec::with_capacity(capacity);
        let mut tx_free = Vec::with_capacity(capacity);
        let mut waiter_slots = Vec::with_capacity(capacity);
        let mut waiter_free = Vec::with_capacity(capacity);

        // Pre-allocate all buffers at startup
        for index in 0..capacity {
            rx_free.push(vec![0u8; rx_size]);
            tx_free.push(vec![0u8; tx_size]);
            waiter_slots.push(TcpBufferWaiterSlot {
                generation: 0,
                allocated: false,
                waker: None,
            });
            waiter_free.push(capacity - index - 1);
        }

        Self {
            rx_free,
            tx_free,
            capacity,
            rx_buffer_size: rx_size,
            tx_buffer_size: tx_size,
            waiter_slots,
            waiter_free,
            waiter_registered_bits: vec![0; (capacity + 63) / 64],
        }
    }

    /// Create with default settings.
    pub(crate) fn with_defaults() -> Self {
        Self::new(
            DEFAULT_BUFFER_POOL_SIZE,
            TCP_RX_BUFFER_SIZE,
            TCP_TX_BUFFER_SIZE,
        )
    }

    /// Acquire a buffer pair for a new socket.
    ///
    /// Returns `None` if pool is exhausted.
    pub(crate) fn acquire(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.rx_free.is_empty() || self.tx_free.is_empty() {
            return None;
        }
        let rx = self.rx_free.pop()?;
        let tx = self.tx_free.pop()?;
        Some((rx, tx))
    }

    /// Number of available buffer pairs.
    pub(crate) fn available(&self) -> usize {
        self.rx_free.len().min(self.tx_free.len())
    }

    fn release(
        &mut self,
        rx: Vec<u8>,
        tx: Vec<u8>,
    ) -> Result<(), TcpBufferPoolReleaseError> {
        if rx.len() != self.rx_buffer_size {
            return Err(TcpBufferPoolReleaseError::RxLength {
                expected: self.rx_buffer_size,
                actual: rx.len(),
            });
        }
        if tx.len() != self.tx_buffer_size {
            return Err(TcpBufferPoolReleaseError::TxLength {
                expected: self.tx_buffer_size,
                actual: tx.len(),
            });
        }
        if self.rx_free.len() != self.tx_free.len() {
            return Err(TcpBufferPoolReleaseError::Imbalanced {
                rx_available: self.rx_free.len(),
                tx_available: self.tx_free.len(),
            });
        }
        if self.rx_free.len() >= self.capacity {
            return Err(TcpBufferPoolReleaseError::Full {
                capacity: self.capacity,
            });
        }
        self.rx_free.push(rx);
        self.tx_free.push(tx);
        self.wake_listener_waiters();
        Ok(())
    }

    fn allocate_listener_waiter(
        &mut self,
    ) -> Result<TcpBufferWaiterHandle, TcpBufferWaiterError> {
        let Some(&index) = self.waiter_free.last() else {
            return Err(TcpBufferWaiterError::Full {
                capacity: self.waiter_slots.len(),
            });
        };
        let Some(slot) = self.waiter_slots.get_mut(index) else {
            return Err(TcpBufferWaiterError::InvalidHandle { index });
        };
        if slot.allocated {
            return Err(TcpBufferWaiterError::StaleHandle { index });
        }
        self.waiter_free.pop();
        slot.generation = slot.generation.wrapping_add(1);
        slot.allocated = true;
        slot.waker = None;
        Ok(TcpBufferWaiterHandle {
            index,
            generation: slot.generation,
        })
    }

    fn register_listener_waiter(
        &mut self,
        handle: TcpBufferWaiterHandle,
        waker: &Waker,
    ) -> Result<(), TcpBufferWaiterError> {
        let slot = self.listener_waiter_slot_mut(handle)?;
        if slot
            .waker
            .as_ref()
            .is_some_and(|registered| registered.will_wake(waker))
        {
            return Ok(());
        }
        slot.waker = Some(waker.clone());
        let word = handle.index / 64;
        let bit = 1u64 << (handle.index % 64);
        self.waiter_registered_bits[word] |= bit;
        Ok(())
    }

    fn clear_listener_waiter(
        &mut self,
        handle: TcpBufferWaiterHandle,
    ) -> Result<(), TcpBufferWaiterError> {
        self.listener_waiter_slot_mut(handle)?.waker = None;
        let word = handle.index / 64;
        let bit = 1u64 << (handle.index % 64);
        self.waiter_registered_bits[word] &= !bit;
        Ok(())
    }

    fn release_listener_waiter(
        &mut self,
        handle: TcpBufferWaiterHandle,
    ) -> Result<(), TcpBufferWaiterError> {
        if self.waiter_free.len() >= self.waiter_slots.len() {
            return Err(TcpBufferWaiterError::DuplicateFree {
                index: handle.index,
            });
        }
        self.clear_listener_waiter(handle)?;
        let slot = self.listener_waiter_slot_mut(handle)?;
        slot.allocated = false;
        self.waiter_free.push(handle.index);
        Ok(())
    }

    fn listener_waiter_slot_mut(
        &mut self,
        handle: TcpBufferWaiterHandle,
    ) -> Result<&mut TcpBufferWaiterSlot, TcpBufferWaiterError> {
        let Some(slot) = self.waiter_slots.get_mut(handle.index) else {
            return Err(TcpBufferWaiterError::InvalidHandle {
                index: handle.index,
            });
        };
        if !slot.allocated || slot.generation != handle.generation {
            return Err(TcpBufferWaiterError::StaleHandle {
                index: handle.index,
            });
        }
        Ok(slot)
    }

    fn wake_listener_waiters(&mut self) {
        for word_index in 0..self.waiter_registered_bits.len() {
            let mut registered = std::mem::replace(
                &mut self.waiter_registered_bits[word_index],
                0,
            );
            while registered != 0 {
                let bit_index = registered.trailing_zeros() as usize;
                registered &= registered - 1;
                let slot_index = word_index * 64 + bit_index;
                let Some(slot) = self.waiter_slots.get_mut(slot_index) else {
                    eprintln!(
                        "[tokio-dpdk] ERROR TCP buffer waiter bit references invalid slot index={}",
                        slot_index
                    );
                    continue;
                };
                let Some(waker) = slot.waker.take() else {
                    eprintln!(
                        "[tokio-dpdk] ERROR TCP buffer waiter bit has no waker index={}",
                        slot_index
                    );
                    continue;
                };
                waker.wake();
            }
        }
    }
}

// =============================================================================
// DpdkDriver - Network stack driver
// =============================================================================

#[derive(Clone, Copy, Debug)]
pub(crate) struct StartedTcpConnect {
    pub(crate) handle: SocketHandle,
    pub(crate) local_addr: SocketAddr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConnectResources {
    handle: SocketHandle,
    local_port: u16,
}

trait ConnectResourceOwner {
    fn cleanup_connect_socket(&mut self, handle: SocketHandle) -> io::Result<()>;
    fn cleanup_connect_port(&mut self, port: u16);
}

struct ConnectSetupGuard<'a, O: ConnectResourceOwner> {
    owner: &'a mut O,
    handle: SocketHandle,
    local_port: Option<u16>,
    armed: bool,
}

struct ListenSetupGuard<'a, O: ConnectResourceOwner> {
    owner: &'a mut O,
    handle: SocketHandle,
    armed: bool,
}

impl<'a, O: ConnectResourceOwner> ListenSetupGuard<'a, O> {
    fn new(owner: &'a mut O, handle: SocketHandle) -> Self {
        Self {
            owner,
            handle,
            armed: true,
        }
    }

    fn owner_mut(&mut self) -> &mut O {
        self.owner
    }

    fn disarm(mut self) -> SocketHandle {
        self.armed = false;
        self.handle
    }
}

impl<'a, O: ConnectResourceOwner> ConnectSetupGuard<'a, O> {
    fn new(owner: &'a mut O, handle: SocketHandle) -> Self {
        Self {
            owner,
            handle,
            local_port: None,
            armed: true,
        }
    }

    fn owner_mut(&mut self) -> &mut O {
        self.owner
    }

    fn set_local_port(&mut self, port: u16) {
        self.local_port = Some(port);
    }

    fn disarm(mut self) -> io::Result<ConnectResources> {
        let Some(local_port) = self.local_port else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TCP connect setup completed without owning a local port",
            ));
        };
        self.armed = false;
        Ok(ConnectResources {
            handle: self.handle,
            local_port,
        })
    }
}

impl<O: ConnectResourceOwner> Drop for ConnectSetupGuard<'_, O> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self.owner.cleanup_connect_socket(self.handle) {
            eprintln!(
                "[tokio-dpdk] ERROR TCP connect setup failed to remove socket handle={:?} error={}",
                self.handle, error
            );
        }
        if let Some(port) = self.local_port.take() {
            self.owner.cleanup_connect_port(port);
        }
    }
}

impl<O: ConnectResourceOwner> Drop for ListenSetupGuard<'_, O> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self.owner.cleanup_connect_socket(self.handle) {
            eprintln!(
                "[tokio-dpdk] ERROR TCP listen setup failed to remove socket handle={:?} error={}",
                self.handle, error
            );
        }
    }
}

/// DPDK network driver.
///
/// Manages:
/// - `DpdkDevice`: DPDK packet I/O
/// - `smoltcp::Interface`: IP layer processing
/// - `smoltcp::SocketSet`: TCP/UDP sockets
///
/// Waker management is handled by smoltcp's native `register_recv_waker`/`register_send_waker`.
/// The driver is polled in the worker event loop to process packets.
pub(crate) struct DpdkDriver {
    worker_index: usize,
    /// DPDK device for packet I/O
    device: DpdkDevice,
    /// smoltcp network interface
    iface: Interface,
    /// Socket set containing TCP/UDP sockets
    sockets: SocketSet<'static, LinearBuffer<'static>>,
    /// Start time for timestamp calculation
    start_time: Instant,
    /// Set of registered socket handles (part of has_poll_work)
    registered_sockets: HashSet<SocketHandle>,
    /// Pre-allocated buffer pool for TCP sockets (zero-allocation at runtime)
    buffer_pool: TcpBufferPool,
    /// Set of bound ports to track address-in-use (since smoltcp doesn't expose this)
    bound_ports: HashSet<u16>,
    /// RSS-hash based lossy tail receiver for market-data flows.
    raw_tail: RawTailTable,
    /// Min-heap of socket handles with pending smoltcp egress work and their due time.
    pending_egress_heap: Vec<(SmolInstant, SocketHandle)>,
    /// Dense handle-index to pending_egress_heap slot map. PENDING_EGRESS_NONE means absent.
    pending_egress_pos: Vec<usize>,
    /// Socket handles touched by the current smoltcp ingress drain.
    ingress_touched: Vec<SocketHandle>,
    /// Dense bitset used to deduplicate ingress touched handles.
    ingress_touched_bits: Vec<u64>,
    /// True after the configured IPv4 gateway has been registered for LKG probing.
    gateway_neighbor_configured: bool,
    /// Fixed counters for typed gateway probe failures.
    gateway_probe_errors: GatewayProbeErrorCounters,
    /// Last cold-path aggregated infrastructure ERROR report.
    last_infra_error_log: Option<Instant>,
}

impl ConnectResourceOwner for DpdkDriver {
    fn cleanup_connect_socket(&mut self, handle: SocketHandle) -> io::Result<()> {
        self.remove_socket(handle)
    }

    fn cleanup_connect_port(&mut self, port: u16) {
        self.release_port(port);
    }
}

impl DpdkDriver {
    /// Create a new DPDK driver.
    ///
    /// # Arguments
    /// * `device` - DPDK device for packet I/O
    /// * `mac` - MAC address
    /// * `addresses` - IP addresses with subnets (IPv4 and/or IPv6)
    /// * `gateway_v4` - Optional IPv4 default gateway
    /// * `gateway_v6` - Optional IPv6 default gateway
    pub(crate) fn new(
        mut device: DpdkDevice,
        worker_index: usize,
        mac: [u8; 6],
        addresses: Vec<IpCidr>,
        gateway_v4: Option<Ipv4Address>,
        gateway_v6: Option<Ipv6Address>,
    ) -> Self {
        let start_time = Instant::now();
        let now = SmolInstant::from_millis(0);

        // Create smoltcp interface config
        let config = dpdk_iface_config(mac);

        // Create interface
        let mut iface = Interface::new(config, &mut device, now);

        // Configure all IP addresses (IPv4 and IPv6)
        // Note: smoltcp limit is configured via SMOLTCP_IFACE_MAX_ADDR_COUNT in .cargo/config.toml
        const MAX_IP_ADDRS: usize = 128;
        iface.update_ip_addrs(|addrs| {
            for (i, addr) in addresses.iter().enumerate() {
                if i >= MAX_IP_ADDRS {
                    // smoltcp has a compile-time limit, skip excess addresses
                    eprintln!(
                        "Warning: Skipping IP address {} (smoltcp limit is {})",
                        addr, MAX_IP_ADDRS
                    );
                    break;
                }
                if let Err(e) = addrs.push(*addr) {
                    eprintln!("Warning: Failed to add IP address {}: {:?}", addr, e);
                }
            }
        });

        // Configure IPv4 default gateway and its last-known-good ARP state.
        let mut gateway_neighbor_configured = false;
        if let Some(gw) = gateway_v4 {
            iface
                .routes_mut()
                .add_default_ipv4_route(gw)
                .expect("Failed to add IPv4 default route");
            match iface.configure_gateway_neighbor(
                now,
                IpAddress::Ipv4(gw),
                GATEWAY_SOFT_STALE_AFTER,
            ) {
                Ok(()) => gateway_neighbor_configured = true,
                Err(error) => {
                    eprintln!(
                        "[tokio-dpdk] ERROR failed to configure IPv4 gateway neighbor gateway={} error={}",
                        gw, error
                    );
                }
            }
        }

        // Configure IPv6 default gateway
        if let Some(gw) = gateway_v6 {
            iface
                .routes_mut()
                .add_default_ipv6_route(gw)
                .expect("Failed to add IPv6 default route");
        }

        // Pre-allocate every TCP-lifecycle index at the same fixed socket cap.
        let sockets = SocketSet::new(Vec::with_capacity(DEFAULT_BUFFER_POOL_SIZE));
        let raw_tail_rss_key = device.raw_tail_rss_key().to_vec();

        Self {
            worker_index,
            device,
            iface,
            sockets,
            start_time,
            registered_sockets: HashSet::with_capacity(DEFAULT_BUFFER_POOL_SIZE),
            buffer_pool: TcpBufferPool::with_defaults(),
            bound_ports: HashSet::with_capacity(DEFAULT_BUFFER_POOL_SIZE),
            raw_tail: RawTailTable::new(worker_index, &raw_tail_rss_key),
            pending_egress_heap: Vec::with_capacity(DEFAULT_BUFFER_POOL_SIZE),
            pending_egress_pos: vec![PENDING_EGRESS_NONE; DEFAULT_BUFFER_POOL_SIZE],
            ingress_touched: Vec::with_capacity(DEFAULT_BUFFER_POOL_SIZE),
            ingress_touched_bits: vec![0; (DEFAULT_BUFFER_POOL_SIZE + 63) / 64],
            gateway_neighbor_configured,
            gateway_probe_errors: GatewayProbeErrorCounters::default(),
            last_infra_error_log: None,
        }
    }

    /// Poll the network stack.
    ///
    /// This should be called in the worker event loop. It:
    /// 1. Flushes pending TX packets
    /// 2. Processes incoming packets through smoltcp
    /// 3. smoltcp internally wakes registered wakers on socket state changes
    ///
    /// Returns `true` if there was network activity.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        #[cfg(feature = "market-trace")]
        let track_id = crate::runtime::market_trace::dpdk_track(self.worker_index);
        let smol_now = self.smol_instant(now);
        // Flush pending TX packets first.
        #[cfg(feature = "market-trace")]
        let trace_flush_tx_before = self.device.has_pending_tx();
        #[cfg(feature = "market-trace")]
        let flush_tx_before_start_ns = if trace_flush_tx_before {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        #[cfg(feature = "market-trace")]
        let flush_tx_before_stats = if trace_flush_tx_before {
            self.device.flush_tx_with_stats()
        } else {
            let _ = self.device.flush_tx();
            Default::default()
        };
        #[cfg(not(feature = "market-trace"))]
        let _ = self.device.flush_tx();
        #[cfg(feature = "market-trace")]
        let flush_tx_before_dur_ns = if trace_flush_tx_before {
            crate::runtime::market_trace::now_ns().saturating_sub(flush_tx_before_start_ns)
        } else {
            0
        };

        // Drain the hardware RX queue at poll entry until rx_burst returns no
        // packets. The collected mbufs are then processed in NIC arrival order inside
        // this same poll.
        let drain_rx_stats = self.device.drain_rx(&mut self.raw_tail);
        let received_rx = drain_rx_stats.received_any();

        #[cfg(feature = "market-trace")]
        let trace_poll = trace_flush_tx_before || received_rx;
        #[cfg(feature = "market-trace")]
        let poll_start_ns = if flush_tx_before_start_ns != 0 {
            flush_tx_before_start_ns
        } else if drain_rx_stats.trace_start_ns != 0 {
            drain_rx_stats.trace_start_ns
        } else {
            0
        };

        #[cfg(feature = "market-trace")]
        let flush_acks_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        self.raw_tail.finish_drain(&mut self.device);
        #[cfg(feature = "market-trace")]
        let flush_acks_dur_ns = if trace_poll {
            crate::runtime::market_trace::now_ns().saturating_sub(flush_acks_start_ns)
        } else {
            0
        };

        let has_smoltcp_rx = self.device.has_unprocessed_rx_pending();

        // Poll smoltcp (processes RX, generates TX)
        // smoltcp will automatically call wake() on registered wakers when:
        // - rx_buffer has new data (register_recv_waker)
        // - tx_buffer has new space (register_send_waker)
        // - connection state changes (both wakers)
        #[cfg(feature = "market-trace")]
        let smoltcp_poll_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        let mut result = PollResult::None;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_ingress_packets = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_ingress_state_changes = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_ingress_touched = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_poll_at_handle_count = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_poll_at_handle_queued = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_tcp_cache_hits = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_tcp_cache_misses = 0usize;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_tcp_linear_scanned = 0usize;

        #[cfg(feature = "market-trace")]
        let smoltcp_ingress_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        #[cfg(feature = "market-trace")]
        let mut smoltcp_ingress_done_ns = smoltcp_ingress_start_ns;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_poll_at_handle_start_ns = 0u64;
        #[cfg(feature = "market-trace")]
        let mut smoltcp_poll_at_handle_dur_ns = 0u64;
        if has_smoltcp_rx {
            let mut ingress_touched = std::mem::take(&mut self.ingress_touched);
            let mut ingress_touched_bits = std::mem::take(&mut self.ingress_touched_bits);
            ingress_touched.clear();
            ingress_touched_bits.fill(0);
            loop {
                match self
                    .iface
                    .poll_ingress_single_touched(
                        smol_now,
                        &mut self.device,
                        &mut self.sockets,
                        |handle| {
                            Self::push_handle_dedup(
                                &mut ingress_touched,
                                &mut ingress_touched_bits,
                                handle,
                            );
                        },
                    )
                {
                    PollIngressSingleResult::None => break,
                    PollIngressSingleResult::PacketProcessed => {
                        #[cfg(feature = "market-trace")]
                        {
                            smoltcp_ingress_packets += 1;
                        }
                    }
                    PollIngressSingleResult::SocketStateChanged => {
                        result = PollResult::SocketStateChanged;
                        #[cfg(feature = "market-trace")]
                        {
                            smoltcp_ingress_packets += 1;
                            smoltcp_ingress_state_changes += 1;
                        }
                    }
                }
            }
            #[cfg(feature = "market-trace")]
            {
                smoltcp_ingress_touched = ingress_touched.len();
            }
            #[cfg(feature = "market-trace")]
            let tcp_probe_stats = self.iface.take_tcp_probe_stats();
            #[cfg(feature = "market-trace")]
            {
                smoltcp_tcp_cache_hits = tcp_probe_stats.cache_hits;
                smoltcp_tcp_cache_misses = tcp_probe_stats.cache_misses;
                smoltcp_tcp_linear_scanned = tcp_probe_stats.linear_scanned;
            }
            #[cfg(feature = "market-trace")]
            {
                smoltcp_ingress_done_ns = crate::runtime::market_trace::now_ns();
            }
            #[cfg(feature = "market-trace")]
            {
                smoltcp_poll_at_handle_start_ns = if trace_poll {
                    crate::runtime::market_trace::now_ns()
                } else {
                    0
                };
            }
            for handle in ingress_touched.iter().copied() {
                #[cfg(feature = "market-trace")]
                {
                    smoltcp_poll_at_handle_count += 1;
                }
                if let Some(next_due) = self.iface.poll_at_handle(smol_now, &self.sockets, handle) {
                    #[cfg(feature = "market-trace")]
                    {
                        smoltcp_poll_at_handle_queued += 1;
                    }
                    self.queue_egress_at(handle, next_due);
                }
            }
            #[cfg(feature = "market-trace")]
            {
                smoltcp_poll_at_handle_dur_ns = if trace_poll {
                    crate::runtime::market_trace::now_ns().saturating_sub(smoltcp_poll_at_handle_start_ns)
                } else {
                    0
                };
            }
            ingress_touched.clear();
            self.ingress_touched = ingress_touched;
            self.ingress_touched_bits = ingress_touched_bits;
        }
        #[cfg(feature = "market-trace")]
        let smoltcp_ingress_dur_ns = if trace_poll {
            smoltcp_ingress_done_ns.saturating_sub(smoltcp_ingress_start_ns)
        } else {
            0
        };

        #[cfg(feature = "market-trace")]
        let smoltcp_egress_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };

        // Ingress is complete before probing so an ARP reply or trusted LKG
        // update received in this same poll suppresses an unnecessary request.
        // DeviceExhausted remains due and is retried only by a later poll.
        let gateway_probe_due = self.gateway_neighbor_configured
            && self.iface.gateway_neighbor_probe_due(smol_now).is_some();
        let gateway_probe_sent = if gateway_probe_due {
            match self
                .iface
                .poll_gateway_neighbor_probe(smol_now, &mut self.device)
            {
                Ok(GatewayNeighborProbeResult::Sent) => true,
                Ok(GatewayNeighborProbeResult::NotDue)
                | Ok(GatewayNeighborProbeResult::DeviceExhausted) => false,
                Err(error) => {
                    self.gateway_probe_errors.record(error);
                    false
                }
            }
        } else {
            false
        };

        #[cfg(feature = "market-trace")]
        let smoltcp_egress_pending_start = self.pending_egress_heap.len();
        let (egress_result, smoltcp_egress_processed) = self.poll_pending_egress(smol_now);
        #[cfg(not(feature = "market-trace"))]
        let _ = smoltcp_egress_processed;
        if egress_result == PollResult::SocketStateChanged {
            result = PollResult::SocketStateChanged;
        }
        #[cfg(feature = "market-trace")]
        let smoltcp_egress_pending_end = self.pending_egress_heap.len();
        #[cfg(feature = "market-trace")]
        let smoltcp_egress_dur_ns = if trace_poll {
            crate::runtime::market_trace::now_ns().saturating_sub(smoltcp_egress_start_ns)
        } else {
            0
        };
        #[cfg(feature = "market-trace")]
        let smoltcp_poll_dur_ns = if trace_poll {
            crate::runtime::market_trace::now_ns().saturating_sub(smoltcp_poll_start_ns)
        } else {
            0
        };
        // Flush any new TX packets (e.g., ACKs, SYN-ACK)
        #[cfg(feature = "market-trace")]
        let trace_flush_tx_after = trace_poll && self.device.has_pending_tx();
        #[cfg(feature = "market-trace")]
        let flush_tx_after_start_ns = if trace_flush_tx_after {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        #[cfg(feature = "market-trace")]
        let flush_tx_after_stats = if trace_flush_tx_after {
            self.device.flush_tx_with_stats()
        } else {
            let _ = self.device.flush_tx();
            Default::default()
        };
        #[cfg(not(feature = "market-trace"))]
        let _ = self.device.flush_tx();
        #[cfg(feature = "market-trace")]
        let flush_tx_after_dur_ns = if trace_flush_tx_after {
            crate::runtime::market_trace::now_ns().saturating_sub(flush_tx_after_start_ns)
        } else {
            0
        };

        // Note: dispatch_wakers() is no longer needed!
        // smoltcp's native waker mechanism handles wakeups internally.

        let active = received_rx
            || gateway_probe_sent
            || self.device.has_pending_tx()
            || self.device.has_unprocessed_rx_pending()
            || matches!(result, smoltcp::iface::PollResult::SocketStateChanged);
        self.report_infra_errors(now);
        #[cfg(feature = "market-trace")]
        if trace_poll {
            let poll_dur_ns = crate::runtime::market_trace::now_ns().saturating_sub(poll_start_ns);
            crate::runtime::market_trace::complete(
                poll_start_ns,
                poll_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_DRIVER_POLL,
                track_id,
                0,
            );
            crate::runtime::market_trace::complete(
                flush_tx_before_start_ns,
                flush_tx_before_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_FLUSH_TX,
                track_id,
                pack_trace_aux3(
                    0,
                    flush_tx_before_stats.total_packets,
                    flush_tx_before_stats.zero_retries,
                ),
            );
            if drain_rx_stats.trace_start_ns != 0 {
                crate::runtime::market_trace::complete(
                    drain_rx_stats.trace_start_ns,
                    drain_rx_stats.trace_dur_ns,
                    crate::runtime::market_trace::SPAN_DPDK_DRAIN_RX,
                    track_id,
                    pack_trace_aux3(
                        drain_rx_stats.received,
                        drain_rx_stats.raw_tail_captured,
                        drain_rx_stats.smoltcp_pending,
                    ),
                );
            }
            crate::runtime::market_trace::complete(
                flush_acks_start_ns,
                flush_acks_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_FLUSH_ACKS,
                track_id,
                0,
            );
            crate::runtime::market_trace::complete(
                smoltcp_poll_start_ns,
                smoltcp_poll_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_POLL,
                track_id,
                pack_trace_aux3(
                    usize::from(has_smoltcp_rx),
                    smoltcp_egress_pending_start,
                    usize::from(matches!(result, PollResult::SocketStateChanged)),
                ),
            );
            crate::runtime::market_trace::complete(
                smoltcp_ingress_start_ns,
                smoltcp_ingress_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_INGRESS,
                track_id,
                pack_trace_aux3(
                    smoltcp_ingress_packets,
                    smoltcp_ingress_touched,
                    smoltcp_ingress_state_changes,
                ),
            );
            crate::runtime::market_trace::complete(
                smoltcp_ingress_done_ns,
                0,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_TCP_LOOKUP,
                track_id,
                pack_trace_aux3(
                    smoltcp_tcp_cache_hits,
                    smoltcp_tcp_cache_misses,
                    smoltcp_tcp_linear_scanned,
                ),
            );
            crate::runtime::market_trace::complete(
                smoltcp_poll_at_handle_start_ns,
                smoltcp_poll_at_handle_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_POLL_AT_HANDLE,
                track_id,
                pack_trace_aux3(
                    smoltcp_poll_at_handle_count,
                    smoltcp_poll_at_handle_queued,
                    smoltcp_ingress_touched,
                ),
            );
            crate::runtime::market_trace::complete(
                smoltcp_egress_start_ns,
                smoltcp_egress_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_EGRESS,
                track_id,
                pack_trace_aux3(
                    smoltcp_egress_pending_start,
                    smoltcp_egress_processed,
                    smoltcp_egress_pending_end,
                ),
            );
            crate::runtime::market_trace::complete(
                flush_tx_after_start_ns,
                flush_tx_after_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_FLUSH_TX,
                track_id,
                pack_trace_aux3(
                    1,
                    flush_tx_after_stats.total_packets,
                    flush_tx_after_stats.zero_retries,
                ),
            );
        }
        active
    }

    pub(crate) fn activate_raw_tail_for_socket(
        &mut self,
        handle: SocketHandle,
    ) -> std::io::Result<RawTailHandle> {
        let socket = self.get_tcp_socket_mut(handle);
        let local = socket.local_endpoint().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "socket has no local endpoint")
        })?;
        let remote = socket.remote_endpoint().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "socket has no remote endpoint")
        })?;
        let local_addr = endpoint_to_socket_addr(local)?;
        let remote_addr = endpoint_to_socket_addr(remote)?;
        let tuple = RawTailTuple::from_addrs(local_addr, remote_addr)?;
        let raw_tail = self.raw_tail.register(tuple)?;
        self.raw_tail.activate(raw_tail)?;
        Ok(raw_tail)
    }

    pub(crate) fn reserve_raw_tail(
        &mut self,
        local_addr: std::net::SocketAddr,
        remote_addr: std::net::SocketAddr,
    ) -> std::io::Result<RawTailHandle> {
        let tuple = RawTailTuple::from_addrs(local_addr, remote_addr)?;
        self.raw_tail.register(tuple)
    }

    pub(crate) fn activate_raw_tail(&mut self, handle: RawTailHandle) -> std::io::Result<()> {
        self.raw_tail.activate(handle)
    }

    pub(crate) fn unregister_raw_tail(&mut self, handle: RawTailHandle) {
        self.raw_tail.unregister(handle);
    }

    pub(crate) fn poll_raw_tail_record<R>(
        &mut self,
        handle: RawTailHandle,
        cx: &mut std::task::Context<'_>,
        consume: impl for<'record> FnOnce(RawTailRecord<'record>) -> R,
    ) -> std::task::Poll<std::io::Result<R>> {
        self.raw_tail.poll_record(handle, cx, consume)
    }

    /// Create a new TCP socket using pre-allocated buffers from the pool.
    ///
    /// This method provides zero-allocation socket creation at runtime by
    /// reusing buffers from the internal pool. The buffer size parameters
    /// are ignored when using the pool (pool buffers have fixed size).
    ///
    /// # Returns
    /// `Some(SocketHandle)` if buffers are available, `None` if pool is exhausted.
    pub(crate) fn create_tcp_socket(&mut self) -> Option<SocketHandle> {
        // Acquire pre-allocated buffers from pool
        let (rx_buf, tx_buf) = self.buffer_pool.acquire()?;

        // Use LinearBuffer with on-demand compaction and virtual window.
        // window_reserve: head space beyond this is added to advertised TCP window.
        // Small reserve = more aggressive window advertisement (less compaction).
        // Compaction only triggers when write would exceed capacity.
        const WINDOW_RESERVE: usize = 4096;
        let rx_buffer = LinearBuffer::with_reserve(rx_buf, WINDOW_RESERVE);
        let tx_buffer = LinearBuffer::with_reserve(tx_buf, WINDOW_RESERVE);

        let mut socket: TcpSocket<'_, LinearBuffer<'_>> = TcpSocket::new(rx_buffer, tx_buffer);

        // Disable delayed ACK to reduce latency.
        // By default smoltcp has ACK_DELAY_DEFAULT = 10ms, which adds latency
        // for small packets. For low-latency trading, immediate ACKs are preferred.
        socket.set_ack_delay(None);

        // Disable Nagle's algorithm (equivalent to TCP_NODELAY).
        // By default smoltcp enables Nagle, which delays small packets until
        // either a full MSS is accumulated or the previous packet is ACKed.
        // This adds significant latency for request-response patterns.
        socket.set_nagle_enabled(false);

        Some(self.sockets.add(socket))
    }

    /// Create and configure one listening TCP socket. `Ok(None)` means the
    /// fixed buffer pool is temporarily exhausted; no resource was acquired.
    pub(crate) fn try_create_tcp_listen_socket(
        &mut self,
        endpoint: IpListenEndpoint,
    ) -> io::Result<Option<SocketHandle>> {
        let Some(handle) = self.create_tcp_socket() else {
            return Ok(None);
        };
        let mut setup = ListenSetupGuard::new(self, handle);
        setup.owner_mut().register_listen_socket(handle);
        setup
            .owner_mut()
            .get_tcp_socket_mut(handle)
            .listen(endpoint)
            .map_err(listen_error_to_io)?;
        Ok(Some(setup.disarm()))
    }

    pub(crate) fn allocate_tcp_buffer_waiter(
        &mut self,
    ) -> io::Result<TcpBufferWaiterHandle> {
        self.buffer_pool
            .allocate_listener_waiter()
            .map_err(tcp_buffer_waiter_error_to_io)
    }

    pub(crate) fn register_tcp_buffer_waiter(
        &mut self,
        handle: TcpBufferWaiterHandle,
        waker: &Waker,
    ) -> io::Result<()> {
        self.buffer_pool
            .register_listener_waiter(handle, waker)
            .map_err(tcp_buffer_waiter_error_to_io)
    }

    pub(crate) fn release_tcp_buffer_waiter(
        &mut self,
        handle: TcpBufferWaiterHandle,
    ) -> io::Result<()> {
        self.buffer_pool
            .release_listener_waiter(handle)
            .map_err(tcp_buffer_waiter_error_to_io)
    }

    /// Register a connect socket for tracking.
    ///
    /// This adds the socket handle to the registered set so has_poll_work() stays active.
    /// Waker management is now handled by smoltcp's native mechanism.
    pub(crate) fn register_socket(&mut self, handle: SocketHandle) {
        self.registered_sockets.insert(handle);
    }

    /// Register a listen socket for tracking.
    ///
    /// This adds the socket handle to the registered set so has_poll_work() stays active.
    /// Waker management is now handled by smoltcp's native mechanism.
    pub(crate) fn register_listen_socket(&mut self, handle: SocketHandle) {
        self.registered_sockets.insert(handle);
    }

    /// Create, bind and initiate an outbound TCP connection as one guarded setup.
    pub(crate) fn start_tcp_connect(
        &mut self,
        remote_addr: SocketAddr,
        local_hint: Option<SocketAddr>,
    ) -> io::Result<StartedTcpConnect> {
        let (local_ip, local_std_ip, requested_port) =
            self.resolve_outbound_local(remote_addr, local_hint)?;
        let remote_endpoint = socket_addr_to_endpoint(remote_addr);

        let handle = self.create_tcp_socket().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "No buffers available in fixed TCP socket pool",
            )
        })?;
        let mut setup = ConnectSetupGuard::new(self, handle);
        setup.owner_mut().register_socket(handle);

        let local_port = setup.owner_mut().reserve_local_port(requested_port)?;
        setup.set_local_port(local_port);
        let local_endpoint = IpEndpoint::new(local_ip, local_port);
        setup
            .owner_mut()
            .tcp_connect(handle, remote_endpoint, local_endpoint)?;
        let resources = setup.disarm()?;

        Ok(StartedTcpConnect {
            handle: resources.handle,
            local_addr: SocketAddr::new(local_std_ip, resources.local_port),
        })
    }

    /// Initiate TCP connection and register its complete tuple before SYN egress.
    fn tcp_connect(
        &mut self,
        handle: SocketHandle,
        remote_endpoint: IpEndpoint,
        local_endpoint: IpEndpoint,
    ) -> io::Result<()> {
        connect_socket_and_register_flow(
            &mut self.iface,
            &mut self.sockets,
            handle,
            remote_endpoint,
            local_endpoint,
        )?;
        self.mark_socket_egress_pending(handle);
        Ok(())
    }

    /// Get TCP socket mutable reference.
    pub(crate) fn get_tcp_socket_mut(&mut self, handle: SocketHandle) -> &mut TcpSocket<'static, LinearBuffer<'static>> {
        self.sockets.get_mut::<TcpSocket<'_, LinearBuffer<'_>>>(handle)
    }

    /// Remove a socket from the socket set and return its buffers to the pool.
    pub(crate) fn remove_socket(&mut self, handle: SocketHandle) -> io::Result<()> {
        validate_socket_for_removal(&self.sockets, &self.registered_sockets, handle)?;

        if !self.registered_sockets.remove(&handle) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("socket handle {:?} registration changed during removal", handle),
            ));
        }
        self.iface.unregister_tcp_flow(handle);
        self.clear_socket_egress_pending(handle);
        // SocketSet is exclusively borrowed and was validated immediately
        // above, so its panic-only invalid-handle branch is unreachable here.
        let socket = self.sockets.remove(handle);
        recycle_removed_socket(&mut self.buffer_pool, socket).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to recycle socket handle={:?}: {}", handle, error),
            )
        })
    }

    /// Get available buffer count.
    #[allow(dead_code)] // Reserved for monitoring/diagnostics
    pub(crate) fn buffer_pool_available(&self) -> usize {
        self.buffer_pool.available()
    }

    /// Keep the driver live for gateway resolution and retained device work,
    /// even before the first socket is registered or after the last is removed.
    pub(crate) fn has_poll_work(&self) -> bool {
        !self.registered_sockets.is_empty()
            || self.gateway_neighbor_configured
            || self.device.has_pending_tx()
            || self.device.has_unprocessed_rx_pending()
            || !self.device.error_counters().is_empty()
            || !self.gateway_probe_errors.is_empty()
    }

    #[inline(always)]
    pub(crate) fn mark_socket_egress_pending(&mut self, handle: SocketHandle) {
        self.queue_egress_at(handle, SmolInstant::ZERO);
    }

    pub(crate) fn flush_socket_egress(&mut self, handle: SocketHandle) -> std::io::Result<()> {
        let now = self.smol_instant(Instant::now());
        self.clear_socket_egress_pending(handle);
        let mut device_exhausted = false;
        loop {
            match self
                .iface
                .poll_egress_handle(now, &mut self.device, &mut self.sockets, handle)
            {
                PollEgressHandleResult::SocketStateChanged => {}
                PollEgressHandleResult::None => break,
                PollEgressHandleResult::DeviceExhausted => {
                    device_exhausted = true;
                    break;
                }
            }
        }

        // Restore the handle's schedule before the only TX burst below can
        // return WouldBlock. A globally exhausted device never closes a flow.
        let poll_at = if device_exhausted {
            None
        } else {
            self.iface.poll_at_handle(now, &self.sockets, handle)
        };
        if let Some(next_due) = socket_egress_retry_due(device_exhausted, now, poll_at) {
            self.queue_egress_at(handle, next_due);
        }

        let flush_result = self.device.flush_tx();
        if device_exhausted || flush_result.is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "DPDK TX device has retained pending socket egress",
            ));
        }
        Ok(())
    }

    fn report_infra_errors(&mut self, now: Instant) {
        let device = self.device.error_counters();
        if device.is_empty() && self.gateway_probe_errors.is_empty() {
            return;
        }
        if self
            .last_infra_error_log
            .is_some_and(|last| now.saturating_duration_since(last) < INFRA_ERROR_LOG_INTERVAL)
        {
            return;
        }

        let device = self.device.take_error_counters();
        let gateway = std::mem::take(&mut self.gateway_probe_errors);
        self.last_infra_error_log = Some(now);
        eprintln!(
            "[tokio-dpdk] ERROR bounded infrastructure failures worker={} tx_mbuf_exhausted={} tx_pending_full={} tx_append_failed={} tx_burst_invalid={} rx_mbuf_invalid={} gateway_non_ethernet={} gateway_non_ipv4={} gateway_no_source={} gateway_dispatch_failed={} gateway_changed={}",
            self.worker_index,
            device.tx_mbuf_exhausted,
            device.tx_pending_full,
            device.tx_append_failed,
            device.tx_burst_invalid,
            device.rx_mbuf_invalid,
            gateway.non_ethernet_medium,
            gateway.non_ipv4_gateway,
            gateway.no_source_address,
            gateway.dispatch_failed,
            gateway.gateway_changed,
        );
    }

    fn poll_pending_egress(&mut self, now: SmolInstant) -> (PollResult, usize) {
        let mut result = PollResult::None;
        let mut processed = 0usize;
        loop {
            let Some((due, _)) = self.pending_egress_heap.first().copied() else {
                break;
            };
            if due > now {
                break;
            }
            let Some((_, handle)) = self.pop_pending_egress_min() else {
                break;
            };
            processed += 1;
            match self
                .iface
                .poll_egress_handle(now, &mut self.device, &mut self.sockets, handle)
            {
                PollEgressHandleResult::SocketStateChanged => {
                    result = PollResult::SocketStateChanged;
                }
                PollEgressHandleResult::None => {}
                PollEgressHandleResult::DeviceExhausted => {
                    self.queue_egress_at(handle, now);
                    return (result, processed);
                }
            }
            if let Some(next_due) = self.iface.poll_at_handle(now, &self.sockets, handle) {
                self.queue_egress_at(handle, next_due);
            }
        }
        (result, processed)
    }

    fn clear_socket_egress_pending(&mut self, handle: SocketHandle) {
        let index = handle.index();
        let Some(pos) = self.pending_egress_pos.get(index).copied() else {
            return;
        };
        if pos == PENDING_EGRESS_NONE {
            return;
        }
        if pos >= self.pending_egress_heap.len() || self.pending_egress_heap[pos].1 != handle {
            return;
        }
        self.remove_pending_egress_at(pos);
    }

    fn queue_egress_at(&mut self, handle: SocketHandle, due: SmolInstant) {
        let index = handle.index();
        if self.pending_egress_pos.len() <= index {
            self.pending_egress_pos.resize(index + 1, PENDING_EGRESS_NONE);
        }
        let pos = self.pending_egress_pos[index];
        if pos == PENDING_EGRESS_NONE {
            let pos = self.pending_egress_heap.len();
            self.pending_egress_pos[index] = pos;
            self.pending_egress_heap.push((due, handle));
            self.sift_pending_egress_up(pos);
            return;
        }
        if pos < self.pending_egress_heap.len() && self.pending_egress_heap[pos].1 == handle {
            if due < self.pending_egress_heap[pos].0 {
                self.pending_egress_heap[pos].0 = due;
                self.sift_pending_egress_up(pos);
            }
            return;
        }
        let pos = self.pending_egress_heap.len();
        self.pending_egress_pos[index] = pos;
        self.pending_egress_heap.push((due, handle));
        self.sift_pending_egress_up(pos);
    }

    fn pop_pending_egress_min(&mut self) -> Option<(SmolInstant, SocketHandle)> {
        if self.pending_egress_heap.is_empty() {
            return None;
        }
        Some(self.remove_pending_egress_at(0))
    }

    fn remove_pending_egress_at(&mut self, pos: usize) -> (SmolInstant, SocketHandle) {
        let last = self.pending_egress_heap.len() - 1;
        self.swap_pending_egress(pos, last);
        let removed = self
            .pending_egress_heap
            .pop()
            .expect("pending egress heap cannot be empty after checked remove");
        self.pending_egress_pos[removed.1.index()] = PENDING_EGRESS_NONE;
        if pos < self.pending_egress_heap.len() {
            if pos > 0 && self.pending_egress_less(pos, (pos - 1) / 2) {
                self.sift_pending_egress_up(pos);
            } else {
                self.sift_pending_egress_down(pos);
            }
        }
        removed
    }

    fn sift_pending_egress_up(&mut self, mut pos: usize) {
        while pos > 0 {
            let parent = (pos - 1) / 2;
            if !self.pending_egress_less(pos, parent) {
                break;
            }
            self.swap_pending_egress(pos, parent);
            pos = parent;
        }
    }

    fn sift_pending_egress_down(&mut self, mut pos: usize) {
        loop {
            let left = pos * 2 + 1;
            let right = left + 1;
            let mut smallest = pos;
            if left < self.pending_egress_heap.len()
                && self.pending_egress_less(left, smallest)
            {
                smallest = left;
            }
            if right < self.pending_egress_heap.len()
                && self.pending_egress_less(right, smallest)
            {
                smallest = right;
            }
            if smallest == pos {
                break;
            }
            self.swap_pending_egress(pos, smallest);
            pos = smallest;
        }
    }

    fn pending_egress_less(&self, lhs: usize, rhs: usize) -> bool {
        let (lhs_due, lhs_handle) = self.pending_egress_heap[lhs];
        let (rhs_due, rhs_handle) = self.pending_egress_heap[rhs];
        lhs_due < rhs_due
            || (lhs_due == rhs_due && lhs_handle.index() < rhs_handle.index())
    }

    fn swap_pending_egress(&mut self, lhs: usize, rhs: usize) {
        self.pending_egress_heap.swap(lhs, rhs);
        let lhs_handle = self.pending_egress_heap[lhs].1;
        let rhs_handle = self.pending_egress_heap[rhs].1;
        self.pending_egress_pos[lhs_handle.index()] = lhs;
        self.pending_egress_pos[rhs_handle.index()] = rhs;
    }

    fn push_handle_dedup(
        handles: &mut Vec<SocketHandle>,
        bits: &mut Vec<u64>,
        handle: SocketHandle,
    ) {
        let index = handle.index();
        let word = index / 64;
        let bit = 1u64 << (index % 64);
        if bits.len() <= word {
            bits.resize(word + 1, 0);
        }
        if bits[word] & bit != 0 {
            return;
        }
        bits[word] |= bit;
        handles.push(handle);
    }

    #[inline(always)]
    fn smol_instant(&self, now: Instant) -> SmolInstant {
        SmolInstant::from_millis(now.duration_since(self.start_time).as_millis() as i64)
    }

    /// Get the first IPv4 address configured on this interface.
    ///
    /// This is used as the default source address for outgoing connections.
    /// To get all configured addresses, use `get_ipv4_addresses()`.
    pub(crate) fn get_ipv4_address(&self) -> Option<smoltcp::wire::Ipv4Address> {
        for cidr in self.iface.ip_addrs() {
            if let smoltcp::wire::IpAddress::Ipv4(addr) = cidr.address() {
                // Skip unspecified address
                if !addr.is_unspecified() {
                    return Some(addr);
                }
            }
        }
        None
    }

    /// Get the first global unicast IPv6 address configured on this interface.
    ///
    /// This is used as the default source address for outgoing IPv6 connections.
    /// Link-local addresses (fe80::) are excluded as they're not routable.
    /// To get all configured addresses, use `get_ipv6_addresses()`.
    pub(crate) fn get_ipv6_address(&self) -> Option<smoltcp::wire::Ipv6Address> {
        for cidr in self.iface.ip_addrs() {
            if let smoltcp::wire::IpAddress::Ipv6(addr) = cidr.address() {
                // Skip unspecified and link-local addresses
                if !addr.is_unspecified() && !addr.is_unicast_link_local() {
                    return Some(addr);
                }
            }
        }
        None
    }

    /// Get all IPv4 addresses configured on this interface.
    ///
    /// Filters out unspecified addresses (0.0.0.0).
    /// Returns addresses in the order configured by smoltcp.
    pub(crate) fn get_ipv4_addresses(&self) -> Vec<smoltcp::wire::Ipv4Address> {
        let mut addrs = Vec::new();
        for cidr in self.iface.ip_addrs() {
            if let smoltcp::wire::IpAddress::Ipv4(addr) = cidr.address() {
                if !addr.is_unspecified() {
                    addrs.push(addr);
                }
            }
        }
        addrs
    }

    /// Get all global unicast IPv6 addresses configured on this interface.
    ///
    /// Excludes link-local (fe80::) and unspecified (::) addresses.
    /// Returns addresses in the order configured by smoltcp.
    pub(crate) fn get_ipv6_addresses(&self) -> Vec<smoltcp::wire::Ipv6Address> {
        let mut addrs = Vec::new();
        for cidr in self.iface.ip_addrs() {
            if let smoltcp::wire::IpAddress::Ipv6(addr) = cidr.address() {
                if !addr.is_unspecified() && !addr.is_unicast_link_local() {
                    addrs.push(addr);
                }
            }
        }
        addrs
    }

    fn resolve_outbound_local(
        &self,
        remote_addr: SocketAddr,
        local_hint: Option<SocketAddr>,
    ) -> io::Result<(IpAddress, IpAddr, u16)> {
        let requested_port = local_hint.map(|addr| addr.port()).unwrap_or(0);
        match remote_addr {
            SocketAddr::V4(_) => {
                let local_ip = match local_hint {
                    Some(SocketAddr::V4(local)) if !local.ip().is_unspecified() => {
                        Ipv4Address::from_octets(local.ip().octets())
                    }
                    Some(SocketAddr::V4(_)) | None => self.get_ipv4_address().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "No IPv4 address configured on DPDK interface",
                        )
                    })?,
                    Some(SocketAddr::V6(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "local and remote address families differ",
                        ));
                    }
                };
                let octets = local_ip.octets();
                Ok((
                    IpAddress::Ipv4(local_ip),
                    IpAddr::V4(std::net::Ipv4Addr::from(octets)),
                    requested_port,
                ))
            }
            SocketAddr::V6(_) => {
                let local_ip = match local_hint {
                    Some(SocketAddr::V6(local)) if !local.ip().is_unspecified() => {
                        Ipv6Address::from_octets(local.ip().octets())
                    }
                    Some(SocketAddr::V6(_)) | None => self.get_ipv6_address().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "No global unicast IPv6 address configured on DPDK interface",
                        )
                    })?,
                    Some(SocketAddr::V4(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "local and remote address families differ",
                        ));
                    }
                };
                let octets = local_ip.octets();
                Ok((
                    IpAddress::Ipv6(local_ip),
                    IpAddr::V6(std::net::Ipv6Addr::from(octets)),
                    requested_port,
                ))
            }
        }
    }

    /// Check if a port is already in use.
    ///
    /// This uses the internal bound_ports set since smoltcp doesn't reliably expose
    /// listening port information.
    pub(crate) fn is_port_in_use(&self, port: u16) -> bool {
        self.bound_ports.contains(&port)
    }

    /// Mark a port as bound (in use).
    /// Should be called when a socket binds to a port.
    pub(crate) fn bind_port(&mut self, port: u16) {
        self.bound_ports.insert(port);
    }

    /// Release a port (mark as no longer in use).
    /// Should be called when a socket is closed.
    pub(crate) fn release_port(&mut self, port: u16) {
        self.bound_ports.remove(&port);
    }

    fn reserve_local_port(&mut self, requested_port: u16) -> io::Result<u16> {
        if requested_port == 0 {
            return self.allocate_ephemeral_port().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "No available ephemeral ports",
                )
            });
        }
        reserve_explicit_port(&mut self.bound_ports, requested_port)
    }

    /// Allocate an ephemeral port that is not currently in use.
    ///
    /// Uses time-based randomization with collision avoidance.
    /// Returns None if no port is available after max attempts.
    fn allocate_ephemeral_port(&mut self) -> Option<u16> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Ephemeral port range: 49152-65535 (16384 ports)
        const EPHEMERAL_START: u16 = 49152;
        const EPHEMERAL_RANGE: u16 = 16384;
        const MAX_ATTEMPTS: u16 = 100;

        let base = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u16)
            .unwrap_or(12345);

        for i in 0..MAX_ATTEMPTS {
            let port = EPHEMERAL_START + ((base.wrapping_add(i)) % EPHEMERAL_RANGE);
            if !self.is_port_in_use(port) {
                self.bind_port(port); // Mark it as used immediately
                return Some(port);
            }
        }

        None
    }
}

fn reserve_explicit_port(bound_ports: &mut HashSet<u16>, port: u16) -> io::Result<u16> {
    if !bound_ports.insert(port) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("local TCP port {} is already in use", port),
        ));
    }
    Ok(port)
}

fn socket_addr_to_endpoint(addr: SocketAddr) -> IpEndpoint {
    match addr {
        SocketAddr::V4(addr) => IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::from_octets(addr.ip().octets())),
            addr.port(),
        ),
        SocketAddr::V6(addr) => IpEndpoint::new(
            IpAddress::Ipv6(Ipv6Address::from_octets(addr.ip().octets())),
            addr.port(),
        ),
    }
}

fn connect_error_to_io(error: smoltcp::socket::tcp::ConnectError) -> io::Error {
    let kind = match error {
        smoltcp::socket::tcp::ConnectError::InvalidState => io::ErrorKind::AlreadyExists,
        smoltcp::socket::tcp::ConnectError::Unaddressable => io::ErrorKind::InvalidInput,
    };
    io::Error::new(kind, format!("TCP connect setup failed: {}", error))
}

fn listen_error_to_io(error: smoltcp::socket::tcp::ListenError) -> io::Error {
    let kind = match error {
        smoltcp::socket::tcp::ListenError::InvalidState => io::ErrorKind::AlreadyExists,
        smoltcp::socket::tcp::ListenError::Unaddressable => io::ErrorKind::InvalidInput,
    };
    io::Error::new(kind, format!("TCP listen setup failed: {}", error))
}

fn tcp_buffer_waiter_error_to_io(error: TcpBufferWaiterError) -> io::Error {
    let kind = match &error {
        TcpBufferWaiterError::Full { .. } => io::ErrorKind::OutOfMemory,
        TcpBufferWaiterError::InvalidHandle { .. }
        | TcpBufferWaiterError::StaleHandle { .. }
        | TcpBufferWaiterError::DuplicateFree { .. } => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, error.to_string())
}

fn connect_socket_and_register_flow<'a>(
    iface: &mut Interface,
    sockets: &mut SocketSet<'a, LinearBuffer<'a>>,
    handle: SocketHandle,
    remote_endpoint: IpEndpoint,
    local_endpoint: IpEndpoint,
) -> io::Result<()> {
    {
        let cx = iface.context();
        sockets
            .get_mut::<TcpSocket<'_, LinearBuffer<'_>>>(handle)
            .connect(cx, remote_endpoint, local_endpoint)
            .map_err(connect_error_to_io)?;
    }

    let (registered_local, registered_remote) = {
        let socket = sockets.get::<TcpSocket<'_, LinearBuffer<'_>>>(handle);
        let local = socket.local_endpoint().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "connected TCP socket has no local endpoint",
            )
        })?;
        let remote = socket.remote_endpoint().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "connected TCP socket has no remote endpoint",
            )
        })?;
        (local, remote)
    };
    register_outbound_tcp_flow(iface, handle, registered_local, registered_remote)
}

fn register_outbound_tcp_flow(
    iface: &mut Interface,
    handle: SocketHandle,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> io::Result<()> {
    iface
        .register_tcp_flow(handle, local, remote)
        .map_err(|error| {
            let kind = match error {
                TcpFlowCacheError::Full => io::ErrorKind::OutOfMemory,
                TcpFlowCacheError::HandleOutOfRange => io::ErrorKind::InvalidData,
            };
            io::Error::new(
                kind,
                format!(
                    "failed to register outbound TCP flow handle={:?} local={} remote={}: {}",
                    handle, local, remote, error
                ),
            )
        })
}

fn recycle_removed_socket<'a>(
    pool: &mut TcpBufferPool,
    socket: SmolSocket<'a, LinearBuffer<'a>>,
) -> io::Result<()> {
    let socket = match socket {
        SmolSocket::Tcp(socket) => socket,
        other => {
            drop(other);
            return Ok(());
        }
    };
    let (rx, tx) = match socket.into_owned_buffers() {
        Ok(buffers) => buffers,
        Err(socket) => {
            drop(socket);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "removed TCP socket did not own its LinearBuffer allocations",
            ));
        }
    };
    pool.release(rx, tx)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn validate_socket_for_removal<'a>(
    sockets: &SocketSet<'a, LinearBuffer<'a>>,
    registered_sockets: &HashSet<SocketHandle>,
    handle: SocketHandle,
) -> io::Result<()> {
    if !registered_sockets.contains(&handle) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("socket handle {:?} is not registered", handle),
        ));
    }
    if !sockets.iter().any(|(candidate, _)| candidate == handle) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "registered socket handle {:?} is absent from SocketSet",
                handle
            ),
        ));
    }
    Ok(())
}

fn endpoint_to_socket_addr(endpoint: smoltcp::wire::IpEndpoint) -> std::io::Result<std::net::SocketAddr> {
    let ip = match endpoint.addr {
        smoltcp::wire::IpAddress::Ipv4(addr) => {
            let octets = addr.octets();
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                octets[0], octets[1], octets[2], octets[3],
            ))
        }
        smoltcp::wire::IpAddress::Ipv6(addr) => {
            let octets = addr.octets();
            std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets))
        }
    };
    Ok(std::net::SocketAddr::new(ip, endpoint.port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::phy::{Loopback, Medium};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    struct CountWake {
        count: AtomicUsize,
    }

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    struct FakeConnectOwner {
        removed: Vec<SocketHandle>,
        released: Vec<u16>,
    }

    impl ConnectResourceOwner for FakeConnectOwner {
        fn cleanup_connect_socket(&mut self, handle: SocketHandle) -> io::Result<()> {
            self.removed.push(handle);
            Ok(())
        }

        fn cleanup_connect_port(&mut self, port: u16) {
            self.released.push(port);
        }
    }

    #[test]
    fn iface_flow_cache_matches_fixed_tcp_socket_capacity() {
        let config = dpdk_iface_config([0x02, 0, 0, 0, 0, 1]);
        assert_eq!(config.tcp_flow_cache_capacity, DEFAULT_BUFFER_POOL_SIZE);
    }

    #[test]
    fn device_exhaustion_forces_immediate_socket_egress_retry() {
        let now = SmolInstant::from_millis(100);
        let later = SmolInstant::from_millis(500);
        assert_eq!(
            socket_egress_retry_due(true, now, Some(later)),
            Some(now)
        );
        assert_eq!(
            socket_egress_retry_due(false, now, Some(later)),
            Some(later)
        );
        assert_eq!(socket_egress_retry_due(false, now, None), None);
    }

    #[test]
    fn gateway_probe_failures_keep_typed_fixed_counters() {
        let mut counters = GatewayProbeErrorCounters::default();
        counters.record(GatewayNeighborProbeError::NonEthernetMedium);
        counters.record(GatewayNeighborProbeError::NonIpv4Gateway);
        counters.record(GatewayNeighborProbeError::NoSourceAddress);
        counters.record(GatewayNeighborProbeError::DispatchFailed);
        counters.record(GatewayNeighborProbeError::GatewayChanged);
        counters.record(GatewayNeighborProbeError::GatewayChanged);

        assert_eq!(counters.non_ethernet_medium, 1);
        assert_eq!(counters.non_ipv4_gateway, 1);
        assert_eq!(counters.no_source_address, 1);
        assert_eq!(counters.dispatch_failed, 1);
        assert_eq!(counters.gateway_changed, 2);
    }

    #[test]
    fn explicit_local_port_check_and_mark_is_atomic() {
        let mut ports = HashSet::with_capacity(1);
        assert_eq!(reserve_explicit_port(&mut ports, 50_000).unwrap(), 50_000);
        let error = reserve_explicit_port(&mut ports, 50_000)
            .expect_err("the second owner must not acquire the same port");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(ports.len(), 1);
    }

    #[test]
    fn removed_tcp_socket_reuses_original_buffer_pointers() {
        let mut pool = TcpBufferPool::new(1, 64, 32);
        let (rx, tx) = pool.acquire().expect("startup pool must contain one pair");
        let rx_ptr = rx.as_ptr();
        let tx_ptr = tx.as_ptr();
        let socket = TcpSocket::new(
            LinearBuffer::with_reserve(rx, 8),
            LinearBuffer::with_reserve(tx, 8),
        );

        recycle_removed_socket(&mut pool, SmolSocket::Tcp(socket))
            .expect("owned TCP buffers must return to the pool");
        let (reused_rx, reused_tx) = pool.acquire().expect("released pair must be reusable");
        assert_eq!(reused_rx.as_ptr(), rx_ptr);
        assert_eq!(reused_tx.as_ptr(), tx_ptr);
    }

    #[test]
    fn removing_non_tcp_socket_does_not_touch_tcp_pool() {
        let mut pool = TcpBufferPool::new(1, 64, 32);
        let rx = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY],
            vec![0; 32],
        );
        let tx = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY],
            vec![0; 32],
        );
        let socket: SmolSocket<'_, LinearBuffer<'_>> =
            SmolSocket::Udp(smoltcp::socket::udp::Socket::new(rx, tx));

        recycle_removed_socket(&mut pool, socket)
            .expect("non-TCP socket removal must preserve its existing drop behavior");
        assert_eq!(pool.available(), 1);
    }

    #[test]
    fn invalid_socket_removal_is_reported_before_socket_set_remove() {
        let sockets: SocketSet<'_, LinearBuffer<'_>> =
            SocketSet::new(Vec::with_capacity(1));
        let handle = SocketHandle::default();
        let mut registered = HashSet::with_capacity(1);

        let unregistered = validate_socket_for_removal(&sockets, &registered, handle)
            .expect_err("an unregistered handle must be rejected");
        assert_eq!(unregistered.kind(), io::ErrorKind::NotFound);

        registered.insert(handle);
        let absent = validate_socket_for_removal(&sockets, &registered, handle)
            .expect_err("a stale registered handle must be rejected without panic");
        assert_eq!(absent.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn fixed_pool_reports_exhaustion_and_rejects_invalid_release() {
        let mut pool = TcpBufferPool::new(1, 64, 32);
        assert_eq!(
            pool.release(vec![0; 64], vec![0; 32]),
            Err(TcpBufferPoolReleaseError::Full { capacity: 1 })
        );

        let (mut rx, tx) = pool.acquire().expect("startup pair must be available");
        assert!(pool.acquire().is_none());
        rx.truncate(63);
        assert_eq!(
            pool.release(rx, tx),
            Err(TcpBufferPoolReleaseError::RxLength {
                expected: 64,
                actual: 63,
            })
        );
        assert_eq!(pool.available(), 0);
    }

    #[test]
    fn fixed_pool_release_wakes_every_registered_listener_without_allocating() {
        let mut pool = TcpBufferPool::new(2, 64, 32);
        let (rx, tx) = pool.acquire().expect("one buffer pair must be checked out");
        let first = pool
            .allocate_listener_waiter()
            .expect("first fixed waiter slot must exist");
        let second = pool
            .allocate_listener_waiter()
            .expect("second fixed waiter slot must exist");
        assert_eq!(
            pool.allocate_listener_waiter(),
            Err(TcpBufferWaiterError::Full { capacity: 2 })
        );

        let first_wake = Arc::new(CountWake {
            count: AtomicUsize::new(0),
        });
        let second_wake = Arc::new(CountWake {
            count: AtomicUsize::new(0),
        });
        let first_waker = Waker::from(first_wake.clone());
        let second_waker = Waker::from(second_wake.clone());
        pool.register_listener_waiter(first, &first_waker)
            .expect("first listener must register");
        pool.register_listener_waiter(second, &second_waker)
            .expect("second listener must register");

        pool.release(rx, tx)
            .expect("returning a checked-out pair must wake waiters");
        assert_eq!(first_wake.count.load(Ordering::Relaxed), 1);
        assert_eq!(second_wake.count.load(Ordering::Relaxed), 1);

        let (rx, tx) = pool
            .acquire()
            .expect("a woken listener must be able to consume the returned pair");
        pool.register_listener_waiter(first, &first_waker)
            .expect("an exhausted listener must be able to register again");
        pool.release(rx, tx)
            .expect("a later return must wake a listener registered again");
        assert_eq!(first_wake.count.load(Ordering::Relaxed), 2);
        assert_eq!(second_wake.count.load(Ordering::Relaxed), 1);

        pool.release_listener_waiter(first)
            .expect("first waiter slot must return to the fixed free list");
        pool.release_listener_waiter(second)
            .expect("second waiter slot must return to the fixed free list");
        assert_eq!(pool.waiter_free.len(), 2);
    }

    #[test]
    fn outbound_flow_is_registered_before_first_syn_ack_lookup() {
        let mut device = Loopback::new(Medium::Ethernet);
        let config = dpdk_iface_config([0x02, 0, 0, 0, 0, 1]);
        let mut iface = Interface::new(config, &mut device, SmolInstant::ZERO);
        let mut sockets = SocketSet::new(Vec::with_capacity(1));
        let socket = TcpSocket::new(
            LinearBuffer::with_reserve(vec![0; 64], 8),
            LinearBuffer::with_reserve(vec![0; 64], 8),
        );
        let handle = sockets.add(socket);
        let local = IpEndpoint::new(Ipv4Address::new(10, 0, 0, 1).into(), 50_000);
        let remote = IpEndpoint::new(Ipv4Address::new(10, 0, 0, 2).into(), 443);

        connect_socket_and_register_flow(&mut iface, &mut sockets, handle, remote, local)
            .expect("SYN setup must register its tuple before returning");
        assert_eq!(
            sockets
                .get::<TcpSocket<'_, LinearBuffer<'_>>>(handle)
                .state(),
            smoltcp::socket::tcp::State::SynSent
        );
        assert!(iface.unregister_tcp_flow(handle));
    }

    #[test]
    fn connect_setup_guard_releases_every_acquired_resource() {
        let handle = SocketHandle::default();
        let mut owner = FakeConnectOwner::default();
        {
            let _guard = ConnectSetupGuard::new(&mut owner, handle);
        }
        assert_eq!(owner.removed, vec![handle]);
        assert!(owner.released.is_empty());

        owner.removed.clear();
        {
            let mut guard = ConnectSetupGuard::new(&mut owner, handle);
            guard.set_local_port(50_001);
        }
        assert_eq!(owner.removed, vec![handle]);
        assert_eq!(owner.released, vec![50_001]);

        owner.removed.clear();
        owner.released.clear();
        let resources = {
            let mut guard = ConnectSetupGuard::new(&mut owner, handle);
            guard.set_local_port(50_002);
            guard.disarm().expect("complete setup must transfer ownership")
        };
        assert_eq!(resources.handle, handle);
        assert_eq!(resources.local_port, 50_002);
        assert!(owner.removed.is_empty());
        assert!(owner.released.is_empty());
    }

    #[test]
    fn listen_setup_guard_removes_socket_on_every_early_return() {
        let handle = SocketHandle::default();
        let mut owner = FakeConnectOwner::default();
        {
            let _guard = ListenSetupGuard::new(&mut owner, handle);
        }
        assert_eq!(owner.removed, vec![handle]);

        owner.removed.clear();
        let transferred = ListenSetupGuard::new(&mut owner, handle).disarm();
        assert_eq!(transferred, handle);
        assert!(owner.removed.is_empty());
    }
}

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
use std::time::Instant;

use smoltcp::iface::{
    Config as IfaceConfig, Interface, PollEgressHandleResult, PollIngressSingleResult, PollResult,
    SocketHandle, SocketSet,
};
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::storage::LinearBuffer;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address, Ipv6Address};

use super::device::DpdkDevice;
use super::raw_tail::{RawTailHandle, RawTailReadRequest, RawTailRecord, RawTailTable, RawTailTuple};

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
const PENDING_EGRESS_NONE: usize = usize::MAX;
// =============================================================================
// TcpBufferPool - Pre-allocated buffer management
// =============================================================================

/// Pre-allocated buffer pool for TCP socket buffers.
///
/// Provides zero-allocation socket creation by reusing buffers.
/// When pool runs low, it can be replenished by background tasks.
pub(crate) struct TcpBufferPool {
    /// Free RX buffers available for allocation
    rx_free: Vec<Vec<u8>>,
    /// Free TX buffers available for allocation
    tx_free: Vec<Vec<u8>>,
    /// Maximum pool capacity
    capacity: usize,
    /// RX buffer size for replenishment
    rx_buffer_size: usize,
    /// TX buffer size for replenishment
    tx_buffer_size: usize,
    /// Low watermark - trigger replenishment when available < this
    low_watermark: usize,
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

        // Pre-allocate all buffers at startup
        for _ in 0..capacity {
            rx_free.push(vec![0u8; rx_size]);
            tx_free.push(vec![0u8; tx_size]);
        }

        Self {
            rx_free,
            tx_free,
            capacity,
            rx_buffer_size: rx_size,
            tx_buffer_size: tx_size,
            low_watermark: capacity / 4, // 25% threshold
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
        let rx = self.rx_free.pop()?;
        let tx = self.tx_free.pop()?;
        Some((rx, tx))
    }

    /// Number of available buffer pairs.
    pub(crate) fn available(&self) -> usize {
        self.rx_free.len().min(self.tx_free.len())
    }

    /// Check if pool needs replenishment.
    pub(crate) fn needs_replenish(&self) -> bool {
        self.available() < self.low_watermark
    }

    /// Replenish the pool by adding new buffer pairs.
    ///
    /// This should be called from a background task to avoid
    /// blocking the hot path.
    ///
    /// Returns the number of buffer pairs added.
    pub(crate) fn replenish(&mut self, count: usize) -> usize {
        let mut added = 0;
        let target = self.capacity.min(self.available() + count);

        while self.available() < target {
            self.rx_free.push(vec![0u8; self.rx_buffer_size]);
            self.tx_free.push(vec![0u8; self.tx_buffer_size]);
            added += 1;
        }

        added
    }
}

// =============================================================================
// DpdkDriver - Network stack driver
// =============================================================================

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
    /// Set of registered socket handles (for has_registered_sockets check)
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
        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));

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

        // Configure IPv4 default gateway
        if let Some(gw) = gateway_v4 {
            iface
                .routes_mut()
                .add_default_ipv4_route(gw)
                .expect("Failed to add IPv4 default route");
        }

        // Configure IPv6 default gateway
        if let Some(gw) = gateway_v6 {
            iface
                .routes_mut()
                .add_default_ipv6_route(gw)
                .expect("Failed to add IPv6 default route");
        }

        // Create socket set
        let sockets = SocketSet::new(vec![]);

        Self {
            worker_index,
            device,
            iface,
            sockets,
            start_time,
            registered_sockets: HashSet::new(),
            buffer_pool: TcpBufferPool::with_defaults(),
            bound_ports: HashSet::new(),
            raw_tail: RawTailTable::new(worker_index),
            pending_egress_heap: Vec::new(),
            pending_egress_pos: Vec::new(),
            ingress_touched: Vec::new(),
            ingress_touched_bits: Vec::new(),
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
        #[cfg(feature = "market-trace")]
        let poll_start_ns = crate::runtime::market_trace::now_ns();
        let smol_now = self.smol_instant(now);
        // Flush pending TX packets first.
        #[cfg(feature = "market-trace")]
        let flush_tx_before_start_ns = crate::runtime::market_trace::now_ns();
        let _ = self.device.flush_tx();
        #[cfg(feature = "market-trace")]
        let flush_tx_before_dur_ns =
            crate::runtime::market_trace::now_ns().saturating_sub(flush_tx_before_start_ns);

        // Drain the hardware RX queue at poll entry until rx_burst cannot fill
        // the preallocated batch buffer. The collected mbufs are then processed
        // newest-first inside this same poll.
        #[cfg(feature = "market-trace")]
        let drain_rx_start_ns = crate::runtime::market_trace::now_ns();
        let received_rx = self.device.drain_rx(&mut self.raw_tail);
        #[cfg(feature = "market-trace")]
        let drain_rx_dur_ns =
            crate::runtime::market_trace::now_ns().saturating_sub(drain_rx_start_ns);

        #[cfg(feature = "market-trace")]
        let trace_poll = received_rx;

        #[cfg(feature = "market-trace")]
        let flush_acks_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        self.raw_tail.flush_acks(&mut self.device);
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
        let smoltcp_ingress_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
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
                    PollIngressSingleResult::PacketProcessed => {}
                    PollIngressSingleResult::SocketStateChanged => result = PollResult::SocketStateChanged,
                }
            }
            for handle in ingress_touched.iter().copied() {
                if let Some(next_due) = self.iface.poll_at_handle(smol_now, &self.sockets, handle) {
                    self.queue_egress_at(handle, next_due);
                }
            }
            ingress_touched.clear();
            self.ingress_touched = ingress_touched;
            self.ingress_touched_bits = ingress_touched_bits;
        }
        #[cfg(feature = "market-trace")]
        let smoltcp_ingress_dur_ns = if trace_poll {
            crate::runtime::market_trace::now_ns().saturating_sub(smoltcp_ingress_start_ns)
        } else {
            0
        };

        #[cfg(feature = "market-trace")]
        let smoltcp_egress_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        if self.poll_pending_egress(smol_now) == PollResult::SocketStateChanged {
            result = PollResult::SocketStateChanged;
        }
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
        self.device.drop_unprocessed_rx_pending();

        // Flush any new TX packets (e.g., ACKs, SYN-ACK)
        #[cfg(feature = "market-trace")]
        let flush_tx_after_start_ns = if trace_poll {
            crate::runtime::market_trace::now_ns()
        } else {
            0
        };
        let _ = self.device.flush_tx();
        #[cfg(feature = "market-trace")]
        let flush_tx_after_dur_ns = if trace_poll {
            crate::runtime::market_trace::now_ns().saturating_sub(flush_tx_after_start_ns)
        } else {
            0
        };

        // Note: dispatch_wakers() is no longer needed!
        // smoltcp's native waker mechanism handles wakeups internally.

        let active = received_rx || matches!(result, smoltcp::iface::PollResult::SocketStateChanged);
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
                0,
            );
            crate::runtime::market_trace::complete(
                drain_rx_start_ns,
                drain_rx_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_DRAIN_RX,
                track_id,
                0,
            );
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
                u64::from(!has_smoltcp_rx),
            );
            crate::runtime::market_trace::complete(
                smoltcp_ingress_start_ns,
                smoltcp_ingress_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_INGRESS,
                track_id,
                0,
            );
            crate::runtime::market_trace::complete(
                smoltcp_egress_start_ns,
                smoltcp_egress_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_SMOLTCP_EGRESS,
                track_id,
                0,
            );
            crate::runtime::market_trace::complete(
                flush_tx_after_start_ns,
                flush_tx_after_dur_ns,
                crate::runtime::market_trace::SPAN_DPDK_FLUSH_TX,
                track_id,
                1,
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

    pub(crate) fn poll_raw_tail_ready(
        &mut self,
        handle: RawTailHandle,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.raw_tail.poll_ready(handle, cx)
    }

    pub(crate) fn next_raw_tail_record<'a>(
        &mut self,
        handle: RawTailHandle,
        request: RawTailReadRequest,
        out: &'a mut Vec<u8>,
    ) -> std::io::Result<Option<RawTailRecord<'a>>> {
        self.raw_tail.next_record(handle, request, out)
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

    /// Register a connect socket for tracking.
    ///
    /// This adds the socket handle to the registered set so has_registered_sockets() works.
    /// Waker management is now handled by smoltcp's native mechanism.
    pub(crate) fn register_socket(&mut self, handle: SocketHandle) {
        self.registered_sockets.insert(handle);
    }

    /// Register a listen socket for tracking.
    ///
    /// This adds the socket handle to the registered set so has_registered_sockets() works.
    /// Waker management is now handled by smoltcp's native mechanism.
    pub(crate) fn register_listen_socket(&mut self, handle: SocketHandle) {
        self.registered_sockets.insert(handle);
    }

    /// Unregister a socket from readiness tracking.
    pub(crate) fn unregister_socket(&mut self, handle: SocketHandle) {
        self.registered_sockets.remove(&handle);
    }

    /// Initiate TCP connection.
    ///
    /// This is a non-blocking call that initiates the TCP handshake.
    /// The connection completes asynchronously.
    pub(crate) fn tcp_connect<T, U>(
        &mut self,
        handle: SocketHandle,
        remote_endpoint: T,
        local_endpoint: U,
    ) -> Result<(), smoltcp::socket::tcp::ConnectError>
    where
        T: Into<smoltcp::wire::IpEndpoint>,
        U: Into<smoltcp::wire::IpListenEndpoint>,
    {
        let cx = self.iface.context();
        let result = self.sockets
            .get_mut::<TcpSocket<'_, LinearBuffer<'_>>>(handle)
            .connect(cx, remote_endpoint, local_endpoint);
        if result.is_ok() {
            self.mark_socket_egress_pending(handle);
        }
        result
    }

    /// Get TCP socket mutable reference.
    pub(crate) fn get_tcp_socket_mut(&mut self, handle: SocketHandle) -> &mut TcpSocket<'static, LinearBuffer<'static>> {
        self.sockets.get_mut::<TcpSocket<'_, LinearBuffer<'_>>>(handle)
    }

    /// Remove a socket from the socket set and return its buffers to the pool.
    pub(crate) fn remove_socket(&mut self, handle: SocketHandle) {
        self.unregister_socket(handle);
        self.clear_socket_egress_pending(handle);

        // Remove socket and get its buffers
        let socket = self.sockets.remove(handle);

        // Extract buffers from socket for reuse (if this is a TCP socket)
        // Note: smoltcp's Socket type doesn't provide direct buffer extraction,
        // so we rely on the socket being dropped and buffers being freed.
        // For true zero-allocation, we would need to store buffer ownership separately.
        drop(socket);
    }

    /// Check if buffer pool needs replenishment.
    pub(crate) fn buffer_pool_needs_replenish(&self) -> bool {
        self.buffer_pool.needs_replenish()
    }

    /// Get available buffer count.
    #[allow(dead_code)] // Reserved for monitoring/diagnostics
    pub(crate) fn buffer_pool_available(&self) -> usize {
        self.buffer_pool.available()
    }

    /// Replenish the buffer pool.
    /// Returns the number of buffer pairs added.
    pub(crate) fn buffer_pool_replenish(&mut self, count: usize) -> usize {
        self.buffer_pool.replenish(count)
    }

    /// Check if there are any registered sockets.
    /// Used to skip polling when no sockets need service.
    pub(crate) fn has_registered_sockets(&self) -> bool {
        !self.registered_sockets.is_empty()
    }

    #[inline(always)]
    pub(crate) fn mark_socket_egress_pending(&mut self, handle: SocketHandle) {
        self.queue_egress_at(handle, SmolInstant::ZERO);
    }

    pub(crate) fn flush_socket_egress(&mut self, handle: SocketHandle) -> std::io::Result<()> {
        let now = self.smol_instant(Instant::now());
        self.clear_socket_egress_pending(handle);
        loop {
            match self
                .iface
                .poll_egress_handle(now, &mut self.device, &mut self.sockets, handle)
            {
                PollEgressHandleResult::SocketStateChanged => {
                    self.device.flush_tx()?;
                }
                PollEgressHandleResult::None => {
                    self.device.flush_tx()?;
                    if let Some(next_due) = self.iface.poll_at_handle(now, &self.sockets, handle) {
                        self.queue_egress_at(handle, next_due);
                    }
                    return Ok(());
                }
                PollEgressHandleResult::DeviceExhausted => {
                    let _ = self.device.flush_tx();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "DPDK TX device exhausted during best-effort socket egress flush",
                    ));
                }
            }
        }
    }

    fn poll_pending_egress(&mut self, now: SmolInstant) -> PollResult {
        let mut result = PollResult::None;
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
                    return result;
                }
            }
            if let Some(next_due) = self.iface.poll_at_handle(now, &self.sockets, handle) {
                self.queue_egress_at(handle, next_due);
            }
        }
        result
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

    /// Allocate an ephemeral port that is not currently in use.
    ///
    /// Uses time-based randomization with collision avoidance.
    /// Returns None if no port is available after max attempts.
    pub(crate) fn allocate_ephemeral_port(&mut self) -> Option<u16> {
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

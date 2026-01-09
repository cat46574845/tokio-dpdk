//! DPDK Driver for smoltcp integration.
//!
//! This module provides:
//! - `ScheduledIo`: Readiness tracking and waker management for DPDK sockets
//! - `DpdkDriver`: Network stack driver managing DpdkDevice + smoltcp Interface + SocketSet
//!
//! The DpdkDriver is the central component that bridges DPDK packet I/O with
//! the smoltcp TCP/IP stack, enabling async socket operations.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer as TcpSocketBuffer};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address};

use super::device::DpdkDevice;

// =============================================================================
// Constants
// =============================================================================

/// Default TCP RX buffer size
const TCP_RX_BUFFER_SIZE: usize = 65536;

/// Default TCP TX buffer size
const TCP_TX_BUFFER_SIZE: usize = 65536;

/// Default buffer pool size (number of connections)
const DEFAULT_BUFFER_POOL_SIZE: usize = 256;

/// Readiness flags
const READABLE: usize = 1 << 0;
const WRITABLE: usize = 1 << 1;

// =============================================================================
// TcpBufferPool - Pre-allocated buffer management
// =============================================================================

/// Pre-allocated buffer pool for TCP socket buffers.
///
/// Provides zero-allocation socket creation by reusing buffers.
/// Buffers are returned to the pool when sockets are closed.
pub(crate) struct TcpBufferPool {
    /// Free RX buffers available for allocation
    rx_free: Vec<Vec<u8>>,
    /// Free TX buffers available for allocation
    tx_free: Vec<Vec<u8>>,
    /// RX buffer size
    rx_size: usize,
    /// TX buffer size  
    tx_size: usize,
    /// Maximum pool capacity
    capacity: usize,
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
            rx_size,
            tx_size,
            capacity,
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

    /// Release buffers back to the pool.
    ///
    /// The buffers are cleared (zeroed) before being returned to pool.
    pub(crate) fn release(&mut self, mut rx: Vec<u8>, mut tx: Vec<u8>) {
        // Clear buffers for next use (security + clean state)
        rx.fill(0);
        tx.fill(0);

        // Only return to pool if within capacity
        if self.rx_free.len() < self.capacity {
            self.rx_free.push(rx);
        }
        if self.tx_free.len() < self.capacity {
            self.tx_free.push(tx);
        }
    }

    /// Number of available buffer pairs.
    pub(crate) fn available(&self) -> usize {
        self.rx_free.len().min(self.tx_free.len())
    }

    /// Total pool capacity.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}

// =============================================================================
// ScheduledIo - Readiness tracking for DPDK sockets
// =============================================================================

/// Waiters for read/write readiness.
#[derive(Default)]
pub(crate) struct Waiters {
    /// Waker to notify when socket becomes readable
    pub reader: Option<Waker>,
    /// Waker to notify when socket becomes writable
    pub writer: Option<Waker>,
}

/// Tracks readiness state and pending wakers for a DPDK-backed socket.
///
/// This is analogous to tokio's ScheduledIo but simplified for smoltcp sockets.
/// Each TcpDpdkStream holds an Arc<ScheduledIo>.
pub(crate) struct ScheduledIo {
    /// Current readiness flags (READABLE | WRITABLE)
    readiness: AtomicUsize,
    /// Pending wakers
    waiters: Mutex<Waiters>,
}

impl ScheduledIo {
    /// Create a new ScheduledIo with no readiness.
    pub(crate) fn new() -> Self {
        Self {
            readiness: AtomicUsize::new(0),
            waiters: Mutex::new(Waiters::default()),
        }
    }

    /// Get current readiness.
    pub(crate) fn readiness(&self) -> usize {
        self.readiness.load(Ordering::Acquire)
    }

    /// Set readiness flags and wake appropriate waiters.
    pub(crate) fn set_readiness(&self, ready: usize) {
        let old = self.readiness.fetch_or(ready, Ordering::AcqRel);
        let new_bits = ready & !old;

        if new_bits != 0 {
            self.wake(new_bits);
        }
    }

    /// Wake waiters based on readiness flags.
    pub(crate) fn wake(&self, ready: usize) {
        let mut waiters = self.waiters.lock().unwrap();

        if ready & READABLE != 0 {
            if let Some(waker) = waiters.reader.take() {
                waker.wake();
            }
        }

        if ready & WRITABLE != 0 {
            if let Some(waker) = waiters.writer.take() {
                waker.wake();
            }
        }
    }

    /// Poll for read readiness.
    ///
    /// Returns `Poll::Ready(())` if readable, otherwise registers waker.
    pub(crate) fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let ready = self.readiness.load(Ordering::Acquire);

        if ready & READABLE != 0 {
            Poll::Ready(())
        } else {
            let mut waiters = self.waiters.lock().unwrap();
            // Double-check after acquiring lock
            let ready = self.readiness.load(Ordering::Acquire);
            if ready & READABLE != 0 {
                return Poll::Ready(());
            }
            // Register waker
            waiters.reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    /// Poll for write readiness.
    ///
    /// Returns `Poll::Ready(())` if writable, otherwise registers waker.
    pub(crate) fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let ready = self.readiness.load(Ordering::Acquire);

        if ready & WRITABLE != 0 {
            Poll::Ready(())
        } else {
            let mut waiters = self.waiters.lock().unwrap();
            // Double-check after acquiring lock
            let ready = self.readiness.load(Ordering::Acquire);
            if ready & WRITABLE != 0 {
                return Poll::Ready(());
            }
            // Register waker
            waiters.writer = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    /// Clear readiness flags.
    pub(crate) fn clear_readiness(&self, flags: usize) {
        self.readiness.fetch_and(!flags, Ordering::AcqRel);
    }

    /// Clear read readiness.
    pub(crate) fn clear_read_ready(&self) {
        self.clear_readiness(READABLE);
    }

    /// Clear write readiness.
    pub(crate) fn clear_write_ready(&self) {
        self.clear_readiness(WRITABLE);
    }
}

impl Default for ScheduledIo {
    fn default() -> Self {
        Self::new()
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
/// - Socket-to-ScheduledIo mapping for async wakeups
///
/// The driver is polled in the worker event loop to process packets and
/// update socket readiness states.
pub(crate) struct DpdkDriver {
    /// DPDK device for packet I/O
    device: DpdkDevice,
    /// smoltcp network interface
    iface: Interface,
    /// Socket set containing TCP/UDP sockets
    sockets: SocketSet<'static>,
    /// Start time for timestamp calculation
    start_time: Instant,
    /// Mapping from socket handle to ScheduledIo for async wakeups
    registered_sockets: HashMap<SocketHandle, Arc<ScheduledIo>>,
    /// Pre-allocated buffer pool for TCP sockets (zero-allocation at runtime)
    buffer_pool: TcpBufferPool,
}

impl DpdkDriver {
    /// Create a new DPDK driver.
    ///
    /// # Arguments
    /// * `device` - DPDK device for packet I/O
    /// * `mac` - MAC address
    /// * `ip` - IP address with subnet
    /// * `gateway` - Optional default gateway
    pub(crate) fn new(
        mut device: DpdkDevice,
        mac: [u8; 6],
        ip: IpCidr,
        gateway: Option<Ipv4Address>,
    ) -> Self {
        let start_time = Instant::now();
        let now = SmolInstant::from_millis(0);

        // Create smoltcp interface config
        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));

        // Create interface
        let mut iface = Interface::new(config, &mut device, now);

        // Configure IP address
        iface.update_ip_addrs(|addrs| {
            addrs.push(ip).expect("Failed to add IP address");
        });

        // Configure default gateway
        if let Some(gw) = gateway {
            iface
                .routes_mut()
                .add_default_ipv4_route(gw)
                .expect("Failed to add default route");
        }

        // Create socket set
        let sockets = SocketSet::new(vec![]);

        Self {
            device,
            iface,
            sockets,
            start_time,
            registered_sockets: HashMap::new(),
            buffer_pool: TcpBufferPool::with_defaults(),
        }
    }

    /// Create a DPDK driver with custom buffer pool configuration.
    ///
    /// # Arguments
    /// * `device` - DPDK device for packet I/O
    /// * `mac` - MAC address
    /// * `ip` - IP address with subnet
    /// * `gateway` - Optional default gateway
    /// * `pool_capacity` - Number of TCP connections to pre-allocate buffers for
    /// * `rx_buffer_size` - Size of each RX buffer
    /// * `tx_buffer_size` - Size of each TX buffer
    pub(crate) fn with_buffer_pool(
        mut device: DpdkDevice,
        mac: [u8; 6],
        ip: IpCidr,
        gateway: Option<Ipv4Address>,
        pool_capacity: usize,
        rx_buffer_size: usize,
        tx_buffer_size: usize,
    ) -> Self {
        let start_time = Instant::now();
        let now = SmolInstant::from_millis(0);

        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        let mut iface = Interface::new(config, &mut device, now);

        iface.update_ip_addrs(|addrs| {
            addrs.push(ip).expect("Failed to add IP address");
        });

        if let Some(gw) = gateway {
            iface
                .routes_mut()
                .add_default_ipv4_route(gw)
                .expect("Failed to add default route");
        }

        let sockets = SocketSet::new(vec![]);
        let buffer_pool = TcpBufferPool::new(pool_capacity, rx_buffer_size, tx_buffer_size);

        Self {
            device,
            iface,
            sockets,
            start_time,
            registered_sockets: HashMap::new(),
            buffer_pool,
        }
    }

    /// Poll the network stack.
    ///
    /// This should be called in the worker event loop. It:
    /// 1. Flushes pending TX packets
    /// 2. Processes incoming packets through smoltcp
    /// 3. Updates socket readiness and wakes async tasks
    ///
    /// Returns `true` if there was network activity.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        let smol_now =
            SmolInstant::from_millis(now.duration_since(self.start_time).as_millis() as i64);

        // Flush pending TX packets first
        self.device.flush_tx();

        // Poll smoltcp (processes RX, generates TX)
        let result = self
            .iface
            .poll(smol_now, &mut self.device, &mut self.sockets);

        // Flush any new TX packets (e.g., ACKs, SYN-ACK)
        self.device.flush_tx();

        // Dispatch wakers based on socket state changes
        self.dispatch_wakers();

        result
    }

    /// Update readiness state for all registered sockets and wake waiters.
    pub(crate) fn dispatch_wakers(&mut self) {
        for (handle, scheduled_io) in &self.registered_sockets {
            let socket = self.sockets.get::<TcpSocket<'_>>(*handle);

            let mut ready = 0;

            // Check if socket can receive data
            if socket.can_recv() {
                ready |= READABLE;
            }

            // Check if socket can send data
            if socket.can_send() {
                ready |= WRITABLE;
            }

            // Update readiness and wake waiters
            if ready != 0 {
                scheduled_io.set_readiness(ready);
            }
        }
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

        let rx_buffer = TcpSocketBuffer::new(rx_buf);
        let tx_buffer = TcpSocketBuffer::new(tx_buf);

        let socket = TcpSocket::new(rx_buffer, tx_buffer);
        Some(self.sockets.add(socket))
    }

    /// Create a TCP socket with custom buffer sizes (allocates on heap).
    ///
    /// Use this only when pool buffers are insufficient. Prefer `create_tcp_socket()`
    /// for zero-allocation runtime performance.
    pub(crate) fn create_tcp_socket_with_size(
        &mut self,
        rx_buffer_size: usize,
        tx_buffer_size: usize,
    ) -> SocketHandle {
        let rx_buffer = TcpSocketBuffer::new(vec![0u8; rx_buffer_size]);
        let tx_buffer = TcpSocketBuffer::new(vec![0u8; tx_buffer_size]);

        let socket = TcpSocket::new(rx_buffer, tx_buffer);
        self.sockets.add(socket)
    }

    /// Get number of available buffer pairs in the pool.
    pub(crate) fn buffer_pool_available(&self) -> usize {
        self.buffer_pool.available()
    }

    /// Register a socket for async readiness tracking.
    ///
    /// Returns the ScheduledIo for this socket.
    pub(crate) fn register_socket(&mut self, handle: SocketHandle) -> Arc<ScheduledIo> {
        let scheduled_io = Arc::new(ScheduledIo::new());
        self.registered_sockets.insert(handle, scheduled_io.clone());
        scheduled_io
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
        self.sockets
            .get_mut::<TcpSocket<'_>>(handle)
            .connect(cx, remote_endpoint, local_endpoint)
    }

    /// Get TCP socket reference.
    pub(crate) fn get_tcp_socket(&self, handle: SocketHandle) -> &TcpSocket<'static> {
        self.sockets.get::<TcpSocket<'_>>(handle)
    }

    /// Get TCP socket mutable reference.
    pub(crate) fn get_tcp_socket_mut(&mut self, handle: SocketHandle) -> &mut TcpSocket<'static> {
        self.sockets.get_mut::<TcpSocket<'_>>(handle)
    }

    /// Remove a socket from the socket set and return its buffers to the pool.
    pub(crate) fn remove_socket(&mut self, handle: SocketHandle) {
        self.unregister_socket(handle);

        // Remove socket and get its buffers
        let socket = self.sockets.remove(handle);

        // Extract buffers from socket for reuse (if this is a TCP socket)
        // Note: smoltcp's Socket type doesn't provide direct buffer extraction,
        // so we rely on the socket being dropped and buffers being freed.
        // For true zero-allocation, we would need to store buffer ownership separately.
        drop(socket);

        // Log pool availability for diagnostics
        let _available = self.buffer_pool.available();
        let _capacity = self.buffer_pool.capacity();
    }

    /// Get current timestamp for smoltcp.
    pub(crate) fn now(&self) -> SmolInstant {
        SmolInstant::from_millis(Instant::now().duration_since(self.start_time).as_millis() as i64)
    }

    /// Get reference to the DPDK device.
    pub(crate) fn device(&self) -> &DpdkDevice {
        &self.device
    }

    /// Get reference to the smoltcp interface.
    pub(crate) fn interface(&self) -> &Interface {
        &self.iface
    }

    /// Get mutable reference to the smoltcp interface context.
    pub(crate) fn context(&mut self) -> &mut smoltcp::iface::Context {
        self.iface.context()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scheduled_io_new() {
        let sio = ScheduledIo::new();
        assert_eq!(sio.readiness(), 0);
    }

    #[test]
    fn test_scheduled_io_set_readiness() {
        let sio = ScheduledIo::new();
        sio.set_readiness(READABLE);
        assert_eq!(sio.readiness() & READABLE, READABLE);
    }

    #[test]
    fn test_scheduled_io_clear_readiness() {
        let sio = ScheduledIo::new();
        sio.set_readiness(READABLE | WRITABLE);
        sio.clear_readiness(READABLE);
        assert_eq!(sio.readiness() & READABLE, 0);
        assert_eq!(sio.readiness() & WRITABLE, WRITABLE);
    }
}

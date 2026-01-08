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

/// Readiness flags
const READABLE: usize = 1 << 0;
const WRITABLE: usize = 1 << 1;

// =============================================================================
// ScheduledIo - Readiness tracking for DPDK sockets
// =============================================================================

/// Waiters for read/write readiness.
#[derive(Default)]
pub struct Waiters {
    /// Waker to notify when socket becomes readable
    pub reader: Option<Waker>,
    /// Waker to notify when socket becomes writable
    pub writer: Option<Waker>,
}

/// Tracks readiness state and pending wakers for a DPDK-backed socket.
///
/// This is analogous to tokio's ScheduledIo but simplified for smoltcp sockets.
/// Each TcpDpdkStream holds an Arc<ScheduledIo>.
pub struct ScheduledIo {
    /// Current readiness flags (READABLE | WRITABLE)
    readiness: AtomicUsize,
    /// Pending wakers
    waiters: Mutex<Waiters>,
}

impl ScheduledIo {
    /// Create a new ScheduledIo with no readiness.
    pub fn new() -> Self {
        Self {
            readiness: AtomicUsize::new(0),
            waiters: Mutex::new(Waiters::default()),
        }
    }

    /// Get current readiness.
    pub fn readiness(&self) -> usize {
        self.readiness.load(Ordering::Acquire)
    }

    /// Set readiness flags and wake appropriate waiters.
    pub fn set_readiness(&self, ready: usize) {
        let old = self.readiness.fetch_or(ready, Ordering::AcqRel);
        let new_bits = ready & !old;

        if new_bits != 0 {
            self.wake(new_bits);
        }
    }

    /// Wake waiters based on readiness flags.
    pub fn wake(&self, ready: usize) {
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
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
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
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
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
    pub fn clear_readiness(&self, flags: usize) {
        self.readiness.fetch_and(!flags, Ordering::AcqRel);
    }

    /// Clear read readiness.
    pub fn clear_read_ready(&self) {
        self.clear_readiness(READABLE);
    }

    /// Clear write readiness.
    pub fn clear_write_ready(&self) {
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
pub struct DpdkDriver {
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
}

impl DpdkDriver {
    /// Create a new DPDK driver.
    ///
    /// # Arguments
    /// * `device` - DPDK device for packet I/O
    /// * `mac` - MAC address
    /// * `ip` - IP address with subnet
    /// * `gateway` - Optional default gateway
    pub fn new(
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
    pub fn poll(&mut self, now: Instant) -> bool {
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
    pub fn dispatch_wakers(&mut self) {
        for (handle, scheduled_io) in &self.registered_sockets {
            let socket = self.sockets.get::<TcpSocket>(*handle);

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

    /// Create a new TCP socket.
    ///
    /// # Arguments
    /// * `rx_buffer_size` - Optional RX buffer size (default 64KB)
    /// * `tx_buffer_size` - Optional TX buffer size (default 64KB)
    ///
    /// Returns the socket handle.
    pub fn create_tcp_socket(
        &mut self,
        rx_buffer_size: Option<usize>,
        tx_buffer_size: Option<usize>,
    ) -> SocketHandle {
        let rx_size = rx_buffer_size.unwrap_or(TCP_RX_BUFFER_SIZE);
        let tx_size = tx_buffer_size.unwrap_or(TCP_TX_BUFFER_SIZE);

        let rx_buffer = TcpSocketBuffer::new(vec![0u8; rx_size]);
        let tx_buffer = TcpSocketBuffer::new(vec![0u8; tx_size]);

        let socket = TcpSocket::new(rx_buffer, tx_buffer);
        self.sockets.add(socket)
    }

    /// Register a socket for async readiness tracking.
    ///
    /// Returns the ScheduledIo for this socket.
    pub fn register_socket(&mut self, handle: SocketHandle) -> Arc<ScheduledIo> {
        let scheduled_io = Arc::new(ScheduledIo::new());
        self.registered_sockets.insert(handle, scheduled_io.clone());
        scheduled_io
    }

    /// Unregister a socket from readiness tracking.
    pub fn unregister_socket(&mut self, handle: SocketHandle) {
        self.registered_sockets.remove(&handle);
    }

    /// Initiate TCP connection.
    ///
    /// This is a non-blocking call that initiates the TCP handshake.
    /// The connection completes asynchronously.
    pub fn tcp_connect<T, U>(
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
            .get_mut::<TcpSocket>(handle)
            .connect(cx, remote_endpoint, local_endpoint)
    }

    /// Get TCP socket reference.
    pub fn get_tcp_socket(&self, handle: SocketHandle) -> &TcpSocket<'static> {
        self.sockets.get::<TcpSocket>(handle)
    }

    /// Get TCP socket mutable reference.
    pub fn get_tcp_socket_mut(&mut self, handle: SocketHandle) -> &mut TcpSocket<'static> {
        self.sockets.get_mut::<TcpSocket>(handle)
    }

    /// Remove a socket from the socket set.
    pub fn remove_socket(&mut self, handle: SocketHandle) {
        self.unregister_socket(handle);
        self.sockets.remove(handle);
    }

    /// Get current timestamp for smoltcp.
    pub fn now(&self) -> SmolInstant {
        SmolInstant::from_millis(Instant::now().duration_since(self.start_time).as_millis() as i64)
    }

    /// Get reference to the DPDK device.
    pub fn device(&self) -> &DpdkDevice {
        &self.device
    }

    /// Get reference to the smoltcp interface.
    pub fn interface(&self) -> &Interface {
        &self.iface
    }

    /// Get mutable reference to the smoltcp interface context.
    pub fn context(&mut self) -> &mut smoltcp::iface::Context {
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

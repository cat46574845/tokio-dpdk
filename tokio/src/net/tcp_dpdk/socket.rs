//! TcpDpdkSocket implementation.
//!
//! A TCP socket configurator for DPDK-backed connections, allowing pre-connection
//! configuration of socket options.

use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use super::listener::TcpDpdkListener;
use super::stream::TcpDpdkStream;

/// A TCP socket configurator for DPDK connections.
///
/// This struct implements a "blueprint" pattern where configuration is stored
/// and then applied when creating a connection or listener. Unlike standard
/// sockets, DPDK sockets don't have a separate socket descriptor before
/// connection establishment.
///
/// # Usage
///
/// ```ignore
/// use tokio::net::TcpDpdkSocket;
/// use std::net::SocketAddr;
///
/// // Create a socket and configure it
/// let socket = TcpDpdkSocket::new_v4()?;
/// socket.set_nodelay(true)?;
/// socket.set_send_buffer_size(128 * 1024)?;
///
/// // Connect using the configured options
/// let stream = socket.connect("1.2.3.4:8080".parse()?).await?;
/// ```
pub struct TcpDpdkSocket {
    /// IP version (4 or 6)
    is_ipv4: bool,
    /// Local address to bind to (if any)
    local_addr: Mutex<Option<SocketAddr>>,
    /// Receive buffer size
    rx_buffer_size: Mutex<usize>,
    /// Send buffer size
    tx_buffer_size: Mutex<usize>,
    /// Nagle's algorithm disabled
    nodelay: Mutex<bool>,
    /// IP hop limit (TTL)
    hop_limit: Mutex<Option<u8>>,
    /// Keep-alive enabled
    keepalive: Mutex<bool>,
    /// Keep-alive interval (smoltcp uses Duration)
    keepalive_interval: Mutex<Option<Duration>>,
}

/// Default buffer size (64KB)
const DEFAULT_BUFFER_SIZE: usize = 65536;

impl TcpDpdkSocket {
    /// Creates a new IPv4 TCP socket.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tokio::net::TcpDpdkSocket;
    ///
    /// let socket = TcpDpdkSocket::new_v4()?;
    /// ```
    pub fn new_v4() -> io::Result<Self> {
        Ok(Self {
            is_ipv4: true,
            local_addr: Mutex::new(None),
            rx_buffer_size: Mutex::new(DEFAULT_BUFFER_SIZE),
            tx_buffer_size: Mutex::new(DEFAULT_BUFFER_SIZE),
            nodelay: Mutex::new(false),
            hop_limit: Mutex::new(None),
            keepalive: Mutex::new(false),
            keepalive_interval: Mutex::new(None),
        })
    }

    /// Creates a new IPv6 TCP socket.
    pub fn new_v6() -> io::Result<Self> {
        Ok(Self {
            is_ipv4: false,
            local_addr: Mutex::new(None),
            rx_buffer_size: Mutex::new(DEFAULT_BUFFER_SIZE),
            tx_buffer_size: Mutex::new(DEFAULT_BUFFER_SIZE),
            nodelay: Mutex::new(false),
            hop_limit: Mutex::new(None),
            keepalive: Mutex::new(false),
            keepalive_interval: Mutex::new(None),
        })
    }

    /// Binds the socket to the specified address.
    ///
    /// This sets the local address for subsequent `connect` or `listen` calls.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        // Validate address family
        if self.is_ipv4 && !addr.is_ipv4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot bind IPv6 address to IPv4 socket",
            ));
        }
        if !self.is_ipv4 && addr.is_ipv4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot bind IPv4 address to IPv6 socket",
            ));
        }

        *self
            .local_addr
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = Some(addr);
        Ok(())
    }

    /// Returns the local address if one has been bound.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket not bound"))
    }

    /// Sets the send buffer size.
    pub fn set_send_buffer_size(&self, size: u32) -> io::Result<()> {
        *self
            .tx_buffer_size
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = size as usize;
        Ok(())
    }

    /// Returns the send buffer size.
    pub fn send_buffer_size(&self) -> io::Result<u32> {
        Ok(*self
            .tx_buffer_size
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? as u32)
    }

    /// Sets the receive buffer size.
    pub fn set_recv_buffer_size(&self, size: u32) -> io::Result<()> {
        *self
            .rx_buffer_size
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = size as usize;
        Ok(())
    }

    /// Returns the receive buffer size.
    pub fn recv_buffer_size(&self) -> io::Result<u32> {
        Ok(*self
            .rx_buffer_size
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? as u32)
    }

    /// Sets the TCP_NODELAY option.
    ///
    /// When enabled, Nagle's algorithm is disabled, reducing latency at the
    /// cost of potentially more small packets.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        *self
            .nodelay
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = nodelay;
        Ok(())
    }

    /// Returns the current TCP_NODELAY setting.
    pub fn nodelay(&self) -> io::Result<bool> {
        Ok(*self
            .nodelay
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?)
    }

    /// Establishes a TCP connection to the specified address.
    ///
    /// This consumes the socket and returns a connected `TcpDpdkStream`.
    /// If `bind` was called, the connection will use the bound local address.
    ///
    /// Note: Buffer size configuration is currently a hint and may not be
    /// fully applied due to smoltcp's buffer pool architecture.
    pub async fn connect(self, addr: SocketAddr) -> io::Result<TcpDpdkStream> {
        // Validate address family
        if self.is_ipv4 && !addr.is_ipv4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot connect to IPv6 address from IPv4 socket",
            ));
        }
        if !self.is_ipv4 && addr.is_ipv4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot connect to IPv4 address from IPv6 socket",
            ));
        }

        // Get local address if bound
        let local_addr = self
            .local_addr
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?
            .clone();

        // Connect using appropriate method based on whether local address was bound
        let stream = if let Some(local) = local_addr {
            TcpDpdkStream::connect_with_local(addr, local).await?
        } else {
            TcpDpdkStream::connect(addr).await?
        };

        // Apply post-connection options that can be modified
        let nodelay = *self
            .nodelay
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?;
        stream.set_nodelay(nodelay)?;

        let hop_limit = *self
            .hop_limit
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?;
        if let Some(ttl) = hop_limit {
            stream.set_ttl(ttl as u32)?;
        }

        Ok(stream)
    }

    /// Starts listening for incoming connections.
    ///
    /// The `backlog` parameter is currently ignored as smoltcp uses a
    /// single-socket model for listening.
    ///
    /// Note: This requires that `bind` was called first.
    pub fn listen(self, _backlog: u32) -> io::Result<TcpDpdkListener> {
        let local_addr = self.local_addr()?;

        // Use synchronous bind - no DNS resolution needed since local_addr is already SocketAddr
        let listener = TcpDpdkListener::bind_socket_addr(local_addr)?;

        // Apply post-bind options
        let hop_limit = *self
            .hop_limit
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?;
        if let Some(ttl) = hop_limit {
            listener.set_ttl(ttl as u32)?;
        }

        Ok(listener)
    }

    /// Sets the IP_TTL option (hop limit).
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        *self
            .hop_limit
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? =
            Some(ttl.min(255) as u8);
        Ok(())
    }

    /// Returns the current IP_TTL setting.
    pub fn ttl(&self) -> io::Result<Option<u32>> {
        Ok(self
            .hop_limit
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?
            .map(|v| v as u32))
    }

    /// Sets the SO_KEEPALIVE option.
    ///
    /// When enabled, the socket will send keep-alive probes to detect dead connections.
    /// The interval is determined by the OS default (typically 2 hours) unless
    /// `set_keepalive_interval` is also called.
    pub fn set_keepalive(&self, keepalive: bool) -> io::Result<()> {
        *self
            .keepalive
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = keepalive;
        Ok(())
    }

    /// Returns the current SO_KEEPALIVE setting.
    pub fn keepalive(&self) -> io::Result<bool> {
        Ok(*self
            .keepalive
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?)
    }

    /// Sets the keep-alive probe interval.
    ///
    /// This sets how long to wait after the connection becomes idle before
    /// sending the first keep-alive probe.
    ///
    /// Note: This also implicitly enables keep-alive.
    pub fn set_keepalive_interval(&self, interval: Duration) -> io::Result<()> {
        *self
            .keepalive_interval
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = Some(interval);
        // Also enable keepalive
        *self
            .keepalive
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))? = true;
        Ok(())
    }

    /// Returns the current keep-alive interval setting.
    pub fn keepalive_interval(&self) -> io::Result<Option<Duration>> {
        Ok(*self
            .keepalive_interval
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?)
    }
}

impl std::fmt::Debug for TcpDpdkSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpDpdkSocket")
            .field("is_ipv4", &self.is_ipv4)
            .field("local_addr", &self.local_addr)
            .field("rx_buffer_size", &self.rx_buffer_size)
            .field("tx_buffer_size", &self.tx_buffer_size)
            .field("nodelay", &self.nodelay)
            .field("hop_limit", &self.hop_limit)
            .finish()
    }
}

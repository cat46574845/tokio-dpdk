//! TcpDpdkStream implementation.
//!
//! A DPDK-backed TCP stream using smoltcp for protocol processing.

use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;

use crate::io::{AsyncRead, AsyncWrite, Interest, ReadBuf, Ready};
use crate::net::{to_socket_addrs, ToSocketAddrs};

// Import ScheduledIo and worker context from dpdk scheduler
use crate::runtime::scheduler::dpdk::dpdk_driver::ScheduledIo;
use crate::runtime::scheduler::dpdk::{current_worker_index, with_current_driver};

// =============================================================================
// TcpDpdkStream
// =============================================================================

/// A DPDK-backed TCP stream.
///
/// This struct provides a TCP connection that uses DPDK for packet I/O
/// and smoltcp for TCP protocol processing, bypassing the kernel network
/// stack for ultra-low latency.
///
/// # Usage
///
/// ```ignore
/// use tokio::net::TcpDpdkStream;
///
/// let mut stream = TcpDpdkStream::connect("1.2.3.4:8080").await?;
///
/// // Read and write using standard AsyncRead/AsyncWrite traits
/// stream.write_all(b"hello").await?;
/// let mut buf = [0u8; 1024];
/// let n = stream.read(&mut buf).await?;
/// ```
///
/// # Thread Affinity
///
/// Each `TcpDpdkStream` is bound to a specific DPDK worker core.
/// **Operations MUST be performed on the same core**. Attempting to use
/// this stream from a different worker will cause a panic.
///
/// Use `core_id()` to check which worker this stream belongs to.
pub struct TcpDpdkStream {
    /// smoltcp socket handle
    handle: SocketHandle,
    /// Readiness tracking for async wakeups
    scheduled_io: Arc<ScheduledIo>,
    /// Worker core this stream is bound to
    core_id: usize,
    /// Local address (cached)
    local_addr: Option<SocketAddr>,
    /// Peer address (cached)
    peer_addr: Option<SocketAddr>,
    /// Read shutdown flag (smoltcp doesn't support half-close on read side)
    read_shutdown: std::sync::atomic::AtomicBool,
    /// Last error encountered
    last_error: std::sync::Mutex<Option<io::Error>>,
}

impl TcpDpdkStream {
    /// Create a TcpDpdkStream from an existing socket handle.
    ///
    /// This is typically called internally after connection establishment.
    pub(crate) fn from_handle(
        handle: SocketHandle,
        scheduled_io: Arc<ScheduledIo>,
        core_id: usize,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
    ) -> Self {
        Self {
            handle,
            scheduled_io,
            core_id,
            local_addr,
            peer_addr,
            read_shutdown: std::sync::atomic::AtomicBool::new(false),
            last_error: std::sync::Mutex::new(None),
        }
    }

    /// Connect to a remote address.
    ///
    /// This method supports any address type that implements `ToSocketAddrs`,
    /// including hostnames that need DNS resolution.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tokio::net::TcpDpdkStream;
    ///
    /// // Connect with &str
    /// let stream = TcpDpdkStream::connect("1.2.3.4:8080").await?;
    ///
    /// // Or with SocketAddr
    /// use std::net::SocketAddr;
    /// let addr: SocketAddr = "1.2.3.4:8080".parse().unwrap();
    /// let stream = TcpDpdkStream::connect(addr).await?;
    /// ```
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addrs = to_socket_addrs(addr).await?;
        let mut last_err = None;

        for addr in addrs {
            match Self::connect_addr(addr).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved")
        }))
    }

    /// Connect to a specific SocketAddr.
    async fn connect_addr(addr: SocketAddr) -> io::Result<Self> {
        // Get current worker index (must be on DPDK worker thread)
        let core_id = current_worker_index().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "TcpDpdkStream::connect must be called from a DPDK worker thread",
            )
        })?;

        // Create socket and initiate connection via worker's DpdkDriver
        let (handle, scheduled_io, local_endpoint) = with_current_driver(|driver| {
            // Create new TCP socket from pool
            let handle = driver.create_tcp_socket().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "No buffers available in TCP socket pool",
                )
            })?;

            // Register socket for readiness tracking
            let scheduled_io = driver.register_socket(handle);

            // Pick a local ephemeral port that's not in use
            let local_port = driver.allocate_ephemeral_port().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "No available ephemeral ports",
                )
            })?;

            // Build local and remote endpoints based on address family
            let (local_endpoint, remote_endpoint) = match addr {
                SocketAddr::V4(v4) => {
                    // Get the local IPv4 address from the driver
                    let local_ip = driver.get_ipv4_address().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "No IPv4 address configured on DPDK interface",
                        )
                    })?;
                    let local_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv4(local_ip),
                        local_port,
                    );
                    let octets = v4.ip().octets();
                    let remote_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address(octets)),
                        v4.port(),
                    );
                    (local_ep, remote_ep)
                }
                SocketAddr::V6(v6) => {
                    // Get the local IPv6 address from the driver
                    let local_ip = driver.get_ipv6_address().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "No global unicast IPv6 address configured on DPDK interface",
                        )
                    })?;
                    let local_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv6(local_ip),
                        local_port,
                    );
                    let octets = v6.ip().octets();
                    let remote_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address(octets)),
                        v6.port(),
                    );
                    (local_ep, remote_ep)
                }
            };

            // Initiate TCP connection
            driver
                .tcp_connect(handle, remote_endpoint, local_endpoint)
                .map_err(|e| {
                    io::Error::new(io::ErrorKind::ConnectionRefused, format!("{:?}", e))
                })?;

            Ok::<_, io::Error>((handle, scheduled_io, local_endpoint))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver (not on worker thread or driver busy)",
            )
        })??;

        // Wait for connection establishment by polling until socket is connected
        // TCP handshake typically completes in < 1 second, use 30 second timeout
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        let mut last_state_log = std::time::Instant::now();

        loop {
            if start_time.elapsed() > timeout {
                // Get final socket state for error message
                let state_str = with_current_driver(|driver| {
                    let socket = driver.get_tcp_socket_mut(handle);
                    format!("{:?}", socket.state())
                })
                .unwrap_or_else(|| "unknown".to_string());

                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "Connection to {} timed out after {} seconds (socket state: {})",
                        addr,
                        timeout.as_secs(),
                        state_str
                    ),
                ));
            }

            // Poll the driver to process network I/O (including SYN-ACK)
            let connected = with_current_driver(|driver| {
                driver.poll(std::time::Instant::now());

                // Check socket state
                let socket = driver.get_tcp_socket_mut(handle);
                let state = socket.state();

                // Log state periodically (every 5 seconds)
                if last_state_log.elapsed() > std::time::Duration::from_secs(5) {
                    eprintln!("[DPDK] Connect to {} - socket state: {:?}", addr, state);
                    last_state_log = std::time::Instant::now();
                }

                if socket.is_active() && socket.may_send() {
                    // Socket is established and can send data
                    return Ok(true);
                } else if state == smoltcp::socket::tcp::State::Closed
                    || state == smoltcp::socket::tcp::State::TimeWait
                {
                    // Connection failed
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("Connection failed, socket state: {:?}", state),
                    ));
                }
                Ok(false)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "Failed to access DPDK driver during connection",
                )
            })??;

            if connected {
                break;
            }

            // Yield to allow other tasks to run and driver to poll network I/O
            crate::task::yield_now().await;
        }

        let local_addr = Some(SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            local_endpoint.port,
        ));

        Ok(Self {
            handle,
            scheduled_io,
            core_id,
            local_addr,
            peer_addr: Some(addr),
            read_shutdown: std::sync::atomic::AtomicBool::new(false),
            last_error: std::sync::Mutex::new(None),
        })
    }

    /// Connect to a remote address from a specific local address.
    ///
    /// This is used by TcpDpdkSocket::connect when a local address has been bound.
    pub(crate) async fn connect_with_local(
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
    ) -> io::Result<Self> {
        // Get current worker index (must be on DPDK worker thread)
        let core_id = current_worker_index().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "TcpDpdkStream::connect must be called from a DPDK worker thread",
            )
        })?;

        // Create socket and initiate connection via worker's DpdkDriver
        let (handle, scheduled_io, local_endpoint) = with_current_driver(|driver| {
            // Create new TCP socket from pool
            let handle = driver.create_tcp_socket().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "No buffers available in TCP socket pool",
                )
            })?;

            // Register socket for readiness tracking
            let scheduled_io = driver.register_socket(handle);

            // Use the provided local address
            let local_port = if local_addr.port() == 0 {
                // Generate ephemeral port that's not in use
                driver.allocate_ephemeral_port().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "No available ephemeral ports",
                    )
                })?
            } else {
                local_addr.port()
            };

            // Build local and remote endpoints based on address family
            let (local_endpoint, remote_endpoint) = match remote_addr {
                SocketAddr::V4(v4) => {
                    // Use provided local IP or get from driver if unspecified
                    let local_ip = if local_addr.ip().is_unspecified() {
                        driver.get_ipv4_address().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::AddrNotAvailable,
                                "No IPv4 address configured on DPDK interface",
                            )
                        })?
                    } else {
                        match local_addr.ip() {
                            std::net::IpAddr::V4(v4) => smoltcp::wire::Ipv4Address(v4.octets()),
                            _ => {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "Address family mismatch",
                                ))
                            }
                        }
                    };

                    let local_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv4(local_ip),
                        local_port,
                    );
                    let octets = v4.ip().octets();
                    let remote_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address(octets)),
                        v4.port(),
                    );
                    (local_ep, remote_ep)
                }
                SocketAddr::V6(v6) => {
                    // Use provided local IP or get from driver if unspecified
                    let local_ip = if local_addr.ip().is_unspecified() {
                        driver.get_ipv6_address().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::AddrNotAvailable,
                                "No global unicast IPv6 address configured on DPDK interface",
                            )
                        })?
                    } else {
                        match local_addr.ip() {
                            std::net::IpAddr::V6(v6) => smoltcp::wire::Ipv6Address(v6.octets()),
                            _ => {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "Address family mismatch",
                                ))
                            }
                        }
                    };

                    let local_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv6(local_ip),
                        local_port,
                    );
                    let octets = v6.ip().octets();
                    let remote_ep = smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address(octets)),
                        v6.port(),
                    );
                    (local_ep, remote_ep)
                }
            };

            // Initiate TCP connection
            driver
                .tcp_connect(handle, remote_endpoint, local_endpoint)
                .map_err(|e| {
                    io::Error::new(io::ErrorKind::ConnectionRefused, format!("{:?}", e))
                })?;

            Ok::<_, io::Error>((handle, scheduled_io, local_endpoint))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver (not on worker thread or driver busy)",
            )
        })??;

        // Wait for connection establishment
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);

        loop {
            if start_time.elapsed() > timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("Connection to {} timed out", remote_addr),
                ));
            }

            let connected = with_current_driver(|driver| {
                driver.poll(std::time::Instant::now());
                let socket = driver.get_tcp_socket_mut(handle);
                let state = socket.state();

                if socket.is_active() && socket.may_send() {
                    Ok(true)
                } else if state == smoltcp::socket::tcp::State::Closed
                    || state == smoltcp::socket::tcp::State::TimeWait
                {
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("Connection failed, socket state: {:?}", state),
                    ))
                } else {
                    Ok(false)
                }
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "Failed to access DPDK driver during connection",
                )
            })??;

            if connected {
                break;
            }

            crate::task::yield_now().await;
        }

        let result_local_addr = Some(SocketAddr::new(local_addr.ip(), local_endpoint.port));

        Ok(Self {
            handle,
            scheduled_io,
            core_id,
            local_addr: result_local_addr,
            peer_addr: Some(remote_addr),
            read_shutdown: std::sync::atomic::AtomicBool::new(false),
            last_error: std::sync::Mutex::new(None),
        })
    }

    /// Returns the local address of this stream.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket not connected"))
    }

    /// Returns the remote address of this stream.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer_addr
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket not connected"))
    }

    /// Returns the socket handle for internal use.
    pub(crate) fn handle(&self) -> SocketHandle {
        self.handle
    }

    /// Returns the core ID this stream is bound to.
    pub fn core_id(&self) -> usize {
        self.core_id
    }

    /// Assert that we are on the correct worker thread.
    /// Panics if called from a different worker than where the stream was created.
    #[inline]
    fn assert_on_correct_worker(&self) {
        let current =
            current_worker_index().expect("TcpDpdkStream used outside of DPDK worker thread");
        if current != self.core_id {
            panic!(
                "TcpDpdkStream worker affinity violation: stream created on worker {} \
                 but used on worker {}. DPDK streams must be used on the same worker \
                 where they were created.",
                self.core_id, current
            );
        }
    }

    /// Set the TCP_NODELAY option.
    ///
    /// When enabled, disables Nagle's algorithm, reducing latency at the cost
    /// of potentially more small packets.
    ///
    /// Note: smoltcp uses `nagle_enabled` with opposite semantics:
    /// - `nodelay = true` → `nagle_enabled = false`
    /// - `nodelay = false` → `nagle_enabled = true`
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.assert_on_correct_worker();
        let handle = self.handle;

        with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            // smoltcp: nagle_enabled = !nodelay
            socket.set_nagle_enabled(!nodelay);
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver for set_nodelay",
            )
        })
    }

    /// Get the TCP_NODELAY option.
    ///
    /// Returns `true` if Nagle's algorithm is disabled (low latency mode).
    pub fn nodelay(&self) -> io::Result<bool> {
        self.assert_on_correct_worker();
        let handle = self.handle;

        with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            // smoltcp: nodelay = !nagle_enabled
            !socket.nagle_enabled()
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver for nodelay",
            )
        })
    }

    /// Split the stream into read and write halves.
    pub fn split(&mut self) -> (super::ReadHalf<'_>, super::WriteHalf<'_>) {
        (super::ReadHalf::new(self), super::WriteHalf::new(self))
    }

    /// Split the stream into owned read and write halves.
    pub fn into_split(self) -> (super::OwnedReadHalf, super::OwnedWriteHalf) {
        let arc = Arc::new(self);
        (
            super::OwnedReadHalf::new(arc.clone()),
            super::OwnedWriteHalf::new(arc),
        )
    }

    /// Poll for read readiness.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.scheduled_io.poll_read_ready(cx) {
            Poll::Ready(()) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Poll for write readiness.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.scheduled_io.poll_write_ready(cx) {
            Poll::Ready(()) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Clear read readiness (for use after WouldBlock).
    pub(crate) fn clear_read_ready(&self) {
        self.scheduled_io.clear_read_ready();
    }

    /// Clear write readiness (for use after WouldBlock).
    pub(crate) fn clear_write_ready(&self) {
        self.scheduled_io.clear_write_ready();
    }

    // =========================================================================
    // New API parity methods
    // =========================================================================

    /// Waits for any of the requested ready states.
    ///
    /// This function is usually paired with `try_read()` or `try_write()`.
    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        let mut ready = Ready::EMPTY;

        if interest.is_readable() {
            poll_fn(|cx| self.poll_read_ready(cx)).await?;
            ready |= Ready::READABLE;
        }

        if interest.is_writable() {
            poll_fn(|cx| self.poll_write_ready(cx)).await?;
            ready |= Ready::WRITABLE;
        }

        Ok(ready)
    }

    /// Waits for the socket to become readable.
    ///
    /// This function is equivalent to `ready(Interest::READABLE)`.
    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_read_ready(cx)).await
    }

    /// Waits for the socket to become writable.
    ///
    /// This function is equivalent to `ready(Interest::WRITABLE)`.
    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_write_ready(cx)).await
    }

    /// Tries to read data from the stream into the provided buffer.
    ///
    /// Returns the number of bytes read. Returns `WouldBlock` if not ready.
    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        // Check read shutdown flag
        if self
            .read_shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(0); // EOF
        }

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);

            if !socket.can_recv() {
                if socket.state() == smoltcp::socket::tcp::State::CloseWait
                    || socket.state() == smoltcp::socket::tcp::State::Closed
                {
                    return Ok(0);
                }
                return Err(io::ErrorKind::WouldBlock);
            }

            match socket.recv_slice(buf) {
                Ok(n) => Ok(n),
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(n)) => Ok(n),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.clear_read_ready();
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Some(Err(kind)) => Err(io::Error::new(kind, "socket read error")),
            None => Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            )),
        }
    }

    /// Tries to write data to the stream.
    ///
    /// Returns the number of bytes written. Returns `WouldBlock` if not ready.
    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);

            if !socket.can_send() {
                if socket.state() == smoltcp::socket::tcp::State::Closed
                    || socket.state() == smoltcp::socket::tcp::State::Closing
                {
                    return Err(io::ErrorKind::BrokenPipe);
                }
                return Err(io::ErrorKind::WouldBlock);
            }

            match socket.send_slice(buf) {
                Ok(n) => Ok(n),
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(n)) => Ok(n),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.clear_write_ready();
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Some(Err(kind)) => Err(io::Error::new(kind, "socket write error")),
            None => Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            )),
        }
    }

    /// Tries to read data into multiple buffers (vectored I/O).
    ///
    /// smoltcp doesn't support native vectored I/O, so we iterate over buffers.
    pub fn try_read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        // Check read shutdown
        if self
            .read_shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(0);
        }

        let mut total = 0;
        for buf in bufs {
            match self.try_read(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if total > 0 {
                        break;
                    }
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    /// Tries to write data from multiple buffers (vectored I/O).
    ///
    /// smoltcp doesn't support native vectored I/O, so we iterate over buffers.
    pub fn try_write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        let mut total = 0;
        for buf in bufs {
            match self.try_write(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if total > 0 {
                        break;
                    }
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    /// Receives data on the socket from the remote address without removing it from the queue.
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.readable().await?;

            match self.try_peek(buf) {
                Ok(n) => return Ok(n),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Try to peek data without blocking.
    fn try_peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        if self
            .read_shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(0);
        }

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);

            if !socket.can_recv() {
                return Err(io::ErrorKind::WouldBlock);
            }

            // smoltcp's peek returns a slice reference
            match socket.peek(buf.len()) {
                Ok(data) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(n)) => Ok(n),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.clear_read_ready();
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Some(Err(kind)) => Err(io::Error::new(kind, "socket peek error")),
            None => Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            )),
        }
    }

    /// Poll for peek readiness.
    pub fn poll_peek(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<usize>> {
        self.assert_on_correct_worker();

        if self
            .read_shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Poll::Ready(Ok(0));
        }

        match self.scheduled_io.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
        }

        match self.try_peek(buf.initialize_unfilled()) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Clear readiness and re-register waker before returning Pending
                self.scheduled_io.clear_read_ready();
                let _ = self.scheduled_io.poll_read_ready(cx); // Re-register waker
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Gets the value of the IP_TTL option (hop limit).
    pub fn ttl(&self) -> io::Result<u32> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket.hop_limit().unwrap_or(64) as u32
        });

        result.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Cannot access DPDK driver"))
    }

    /// Sets the value of the IP_TTL option (hop limit).
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        let ttl_u8 = ttl.min(255) as u8;

        with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket.set_hop_limit(Some(ttl_u8));
        });
        Ok(())
    }

    /// Gets the value of TCP_QUICKACK (Linux-specific).
    ///
    /// Returns true if quick ACK mode is enabled (delayed ACK disabled).
    #[cfg(target_os = "linux")]
    pub fn quickack(&self) -> io::Result<bool> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            // ack_delay() == None means immediate ACK (quickack)
            socket.ack_delay().is_none()
        });

        result.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Cannot access DPDK driver"))
    }

    /// Sets the value of TCP_QUICKACK (Linux-specific).
    ///
    /// When enabled, ACKs are sent immediately rather than delayed.
    #[cfg(target_os = "linux")]
    pub fn set_quickack(&self, quickack: bool) -> io::Result<()> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            if quickack {
                socket.set_ack_delay(None);
            } else {
                // Default delayed ACK (40ms is typical)
                socket.set_ack_delay(Some(smoltcp::time::Duration::from_millis(40)));
            }
        });
        Ok(())
    }

    /// Returns the value of the SO_ERROR option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let mut guard = self
            .last_error
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poisoned"))?;
        Ok(guard.take())
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O on the specified
    /// portions to return immediately with an appropriate value (see the
    /// documentation of `Shutdown`).
    ///
    /// - `Shutdown::Write`: Sends FIN to close the write half
    /// - `Shutdown::Read`: Sets internal flag to return EOF on subsequent reads
    /// - `Shutdown::Both`: Does both
    pub(super) fn shutdown_std(&self, how: Shutdown) -> io::Result<()> {
        self.assert_on_correct_worker();

        match how {
            Shutdown::Read => {
                self.read_shutdown
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            Shutdown::Write => {
                let handle = self.handle;
                with_current_driver(|driver| {
                    let socket = driver.get_tcp_socket_mut(handle);
                    socket.close();
                });
            }
            Shutdown::Both => {
                self.read_shutdown
                    .store(true, std::sync::atomic::Ordering::Release);
                let handle = self.handle;
                with_current_driver(|driver| {
                    let socket = driver.get_tcp_socket_mut(handle);
                    socket.close();
                });
            }
        }
        Ok(())
    }

    /// Try to perform I/O with the given interest.
    ///
    /// Clears readiness on WouldBlock.
    pub fn try_io<R>(
        &self,
        interest: Interest,
        f: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<R> {
        // Just run the closure and handle WouldBlock
        match f() {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                if interest.is_readable() {
                    self.clear_read_ready();
                }
                if interest.is_writable() {
                    self.clear_write_ready();
                }
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            other => other,
        }
    }

    /// Performs async I/O by waiting for readiness and retrying on WouldBlock.
    pub async fn async_io<R>(
        &self,
        interest: Interest,
        mut f: impl FnMut() -> io::Result<R>,
    ) -> io::Result<R> {
        loop {
            self.ready(interest).await?;

            match self.try_io(interest, &mut f) {
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }
}

// =============================================================================
// AsyncRead implementation
// =============================================================================

impl AsyncRead for TcpDpdkStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Verify worker affinity
        self.assert_on_correct_worker();

        // Wait for read readiness
        match self.scheduled_io.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
        }

        // Read from smoltcp socket via worker context
        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);

            // Check if socket can receive
            if !socket.can_recv() {
                if socket.state() == smoltcp::socket::tcp::State::CloseWait
                    || socket.state() == smoltcp::socket::tcp::State::Closed
                {
                    // Connection closed
                    return Ok(0);
                }
                // Not ready yet
                return Err(io::ErrorKind::WouldBlock);
            }

            // Read data into buffer
            match socket.recv_slice(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    Ok(n)
                }
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(0)) => Poll::Ready(Ok(())), // EOF
            Some(Ok(_n)) => Poll::Ready(Ok(())),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                // Not ready, clear readiness and re-register waker
                self.scheduled_io.clear_read_ready();
                let _ = self.scheduled_io.poll_read_ready(cx); // Re-register waker
                Poll::Pending
            }
            Some(Err(kind)) => Poll::Ready(Err(io::Error::new(kind, "socket read error"))),
            None => {
                // Not on worker thread or driver busy
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Cannot access DPDK driver",
                )))
            }
        }
    }
}

// =============================================================================
// AsyncWrite implementation
// =============================================================================

impl AsyncWrite for TcpDpdkStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Verify worker affinity
        self.assert_on_correct_worker();

        // Wait for write readiness
        match self.scheduled_io.poll_write_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
        }

        // Write to smoltcp socket via worker context
        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);

            // Check if socket can send
            if !socket.can_send() {
                if socket.state() == smoltcp::socket::tcp::State::Closed
                    || socket.state() == smoltcp::socket::tcp::State::Closing
                {
                    return Err(io::ErrorKind::BrokenPipe);
                }
                return Err(io::ErrorKind::WouldBlock);
            }

            // Write data to socket
            match socket.send_slice(buf) {
                Ok(n) => Ok(n),
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(n)) => Poll::Ready(Ok(n)),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                // Not ready, clear readiness and re-register waker
                self.scheduled_io.clear_write_ready();
                let _ = self.scheduled_io.poll_write_ready(cx); // Re-register waker
                Poll::Pending
            }
            Some(Err(kind)) => Poll::Ready(Err(io::Error::new(kind, "socket write error"))),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // DPDK/smoltcp handles flushing via the driver poll loop
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdown_std(Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// Drop implementation
// =============================================================================

impl Drop for TcpDpdkStream {
    fn drop(&mut self) {
        // Verify worker affinity - but only warn, don't panic in destructor
        if let Some(current) = current_worker_index() {
            if current != self.core_id {
                // Log error but don't panic - panicking in Drop can cause issues
                eprintln!(
                    "WARNING: TcpDpdkStream dropped on wrong worker: created on {} but dropped on {}",
                    self.core_id, current
                );
            }
        }

        // Remove socket from DpdkDriver via worker context.
        // This releases the socket handle and returns buffers to the pool.
        let handle = self.handle;
        with_current_driver(|driver| {
            driver.remove_socket(handle);
        });
    }
}

impl std::fmt::Debug for TcpDpdkStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpDpdkStream")
            .field("handle", &format_args!("{:?}", self.handle))
            .field("core_id", &self.core_id)
            .field("local_addr", &self.local_addr)
            .field("peer_addr", &self.peer_addr)
            .finish()
    }
}

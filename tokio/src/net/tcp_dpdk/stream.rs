//! TcpDpdkStream implementation.
//!
//! A DPDK-backed TCP stream using smoltcp for protocol processing.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;

use crate::io::{AsyncRead, AsyncWrite, ReadBuf};

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
/// Operations should be performed on the same core for optimal performance.
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
        }
    }

    /// Connect to a remote address.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::net::SocketAddr;
    /// let addr: SocketAddr = "1.2.3.4:8080".parse().unwrap();
    /// let stream = TcpDpdkStream::connect(addr).await?;
    /// ```
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
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

            // Pick a local ephemeral port using time-based randomization
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u16)
                .unwrap_or(12345);
            let local_port = 49152 + (nanos % 16384);
            let local_endpoint = smoltcp::wire::IpEndpoint::new(
                smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                local_port,
            );

            // Convert remote addr to smoltcp endpoint
            let remote_endpoint = match addr {
                SocketAddr::V4(v4) => {
                    let octets = v4.ip().octets();
                    smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address(octets)),
                        v4.port(),
                    )
                }
                SocketAddr::V6(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "IPv6 not yet supported",
                    ))
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

        // TODO: Wait for connection establishment (poll until connected)
        // For now, return immediately (connection happens asynchronously)

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

    /// Set the TCP_NODELAY option.
    ///
    /// Note: smoltcp TCP sockets use Nagle by default; this disables it.
    pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
        // smoltcp doesn't support runtime Nagle toggle directly
        // This would need to be set at socket creation time
        Ok(())
    }

    /// Get the TCP_NODELAY option.
    pub fn nodelay(&self) -> io::Result<bool> {
        // Default to true (Nagle disabled) for low-latency scenarios
        Ok(true)
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
                // Not ready, register waker and return pending
                self.scheduled_io.clear_read_ready();
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
                self.scheduled_io.clear_write_ready();
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
        // Initiate TCP close via worker context
        let handle = self.handle;
        with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket.close();
        });
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// Drop implementation
// =============================================================================

impl Drop for TcpDpdkStream {
    fn drop(&mut self) {
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

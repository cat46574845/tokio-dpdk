//! TcpDpdkListener implementation.
//!
//! A DPDK-backed TCP listener using smoltcp for protocol processing.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;

use super::stream::TcpDpdkStream;
use crate::runtime::scheduler::dpdk::dpdk_driver::ScheduledIo;
use crate::runtime::scheduler::dpdk::{current_worker_index, with_current_driver};

/// A DPDK-backed TCP listener.
///
/// This struct provides a TCP listener that uses DPDK for packet I/O
/// and smoltcp for TCP protocol processing, bypassing the kernel network
/// stack for ultra-low latency.
///
/// # Usage
///
/// ```ignore
/// use tokio::net::TcpDpdkListener;
/// use std::net::SocketAddr;
///
/// let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
/// let listener = TcpDpdkListener::bind(addr).await?;
///
/// loop {
///     let (stream, peer_addr) = listener.accept().await?;
///     // Handle connection...
/// }
/// ```
///
/// # Thread Affinity
///
/// Each `TcpDpdkListener` is bound to a specific DPDK worker core.
/// Operations should be performed on the same core for optimal performance.
///
/// # Note on smoltcp's Single-Socket Model
///
/// Unlike traditional BSD sockets, smoltcp uses a single-socket model for listening.
/// When a connection is established, the listening socket transitions to the
/// connected state, and a new socket must be created forward.
pub struct TcpDpdkListener {
    /// smoltcp socket handle
    handle: SocketHandle,
    /// Readiness tracking for async wakeups
    scheduled_io: Arc<ScheduledIo>,
    /// Worker core this listener is bound to
    core_id: usize,
    /// Local address
    local_addr: SocketAddr,
}

impl TcpDpdkListener {
    /// Binds to the specified address.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::net::SocketAddr;
    /// let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    /// let listener = TcpDpdkListener::bind(addr).await?;
    /// ```
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        // Get current worker index (must be on DPDK worker thread)
        let core_id = current_worker_index().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "TcpDpdkListener::bind must be called from a DPDK worker thread",
            )
        })?;

        // Create socket and bind via worker's DpdkDriver
        let (handle, scheduled_io) = with_current_driver(|driver| {
            // Create new TCP socket from pool
            let handle = driver.create_tcp_socket().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "No buffers available in TCP socket pool",
                )
            })?;

            // Register socket for readiness tracking
            let scheduled_io = driver.register_socket(handle);

            // Bind and listen via smoltcp
            let socket = driver.get_tcp_socket_mut(handle);
            let endpoint = match addr {
                SocketAddr::V4(v4) => smoltcp::wire::IpListenEndpoint {
                    addr: if v4.ip().is_unspecified() {
                        None // Listen on all addresses
                    } else {
                        Some(smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address(
                            v4.ip().octets(),
                        )))
                    },
                    port: v4.port(),
                },
                SocketAddr::V6(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "IPv6 not yet supported",
                    ))
                }
            };

            socket
                .listen(endpoint)
                .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("{:?}", e)))?;

            Ok::<_, io::Error>((handle, scheduled_io))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver (not on worker thread or driver busy)",
            )
        })??;

        Ok(Self {
            handle,
            scheduled_io,
            core_id,
            local_addr: addr,
        })
    }

    /// Accepts a new connection.
    ///
    /// This method waits until a new connection is established, then returns
    /// the connected stream and peer address.
    ///
    /// # Note
    ///
    /// Due to smoltcp's single-socket model, after a connection is accepted,
    /// the internal socket transitions to the connected state. To accept
    /// another connection, a new listening socket is automatically created.
    pub async fn accept(&self) -> io::Result<(TcpDpdkStream, SocketAddr)> {
        // Poll until a connection is accepted
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpDpdkStream, SocketAddr)>> {
        // Wait for read readiness (indicates connection activity)
        match self.scheduled_io.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
        }

        // Check for new connection via smoltcp
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(self.handle);

            // Check if connection is established
            if socket.is_active() && socket.state() == smoltcp::socket::tcp::State::Established {
                // Connection established - get peer address
                let peer = socket.remote_endpoint().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "No remote endpoint")
                })?;

                let peer_addr = SocketAddr::new(
                    match peer.addr {
                        smoltcp::wire::IpAddress::Ipv4(v4) => {
                            std::net::IpAddr::V4(std::net::Ipv4Addr::from(v4.0))
                        }
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "IPv6 not supported",
                            ))
                        }
                    },
                    peer.port,
                );

                Ok(peer_addr)
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "Not ready"))
            }
        });

        match result {
            Some(Ok(peer_addr)) => {
                // Create stream from accepted connection
                let stream = TcpDpdkStream::from_handle(
                    self.handle,
                    self.scheduled_io.clone(),
                    self.core_id,
                    Some(self.local_addr),
                    Some(peer_addr),
                );
                Poll::Ready(Ok((stream, peer_addr)))
            }
            Some(Err(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                self.scheduled_io.clear_read_ready();
                Poll::Pending
            }
            Some(Err(e)) => Poll::Ready(Err(e)),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            ))),
        }
    }

    /// Returns the local address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    /// Returns the core ID this listener is bound to.
    pub fn core_id(&self) -> usize {
        self.core_id
    }
}

impl Drop for TcpDpdkListener {
    fn drop(&mut self) {
        // Remove socket from DpdkDriver via worker context.
        let handle = self.handle;
        with_current_driver(|driver| {
            driver.remove_socket(handle);
        });
    }
}

impl std::fmt::Debug for TcpDpdkListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpDpdkListener")
            .field("handle", &format_args!("{:?}", self.handle))
            .field("core_id", &self.core_id)
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

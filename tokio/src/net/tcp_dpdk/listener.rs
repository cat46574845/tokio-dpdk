//! TcpDpdkListener implementation.
//!
//! A DPDK-backed TCP listener using smoltcp for protocol processing.
//! Implements a socket pool to support multiple concurrent accepts,
//! working around smoltcp's single-socket-per-connection model.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;
use smoltcp::wire::IpListenEndpoint;

use super::stream::TcpDpdkStream;
use crate::net::{to_socket_addrs, ToSocketAddrs};
use crate::runtime::scheduler::dpdk::dpdk_driver::TcpBufferWaiterHandle;
use crate::runtime::scheduler::dpdk::{current_worker_index, with_current_driver};

/// Default number of listening sockets in the pool.
const DEFAULT_BACKLOG: usize = 128;

/// A listening socket in the pool.
struct ListenSocket {
    handle: SocketHandle,
}

/// Internal state for the listener (uses interior mutability).
struct ListenerInner {
    /// Pool of listening sockets
    listen_pool: VecDeque<ListenSocket>,
    /// Backlog size (number of listening sockets to maintain)
    backlog: usize,
    /// Fixed driver waiter slot held only while this listener has observed
    /// buffer-pool exhaustion.
    buffer_waiter: Option<TcpBufferWaiterHandle>,
}

struct ListenerBindGuard {
    listen_pool: VecDeque<ListenSocket>,
    endpoint: IpListenEndpoint,
    core_id: usize,
    armed: bool,
}

impl ListenerBindGuard {
    fn new(
        listen_pool: VecDeque<ListenSocket>,
        endpoint: IpListenEndpoint,
        core_id: usize,
    ) -> Self {
        Self {
            listen_pool,
            endpoint,
            core_id,
            armed: true,
        }
    }

    fn disarm(mut self) -> VecDeque<ListenSocket> {
        self.armed = false;
        std::mem::take(&mut self.listen_pool)
    }
}

impl Drop for ListenerBindGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if current_worker_index() != Some(self.core_id) {
            eprintln!(
                "[tokio-dpdk] ERROR listener bind cleanup outside owner worker owner={:?} current={:?}",
                self.core_id,
                current_worker_index()
            );
            return;
        }
        let endpoint = self.endpoint;
        let listen_pool = &mut self.listen_pool;
        if with_current_driver(|driver| {
            for listen_socket in listen_pool.drain(..) {
                if let Err(error) = driver.remove_socket(listen_socket.handle) {
                    eprintln!(
                        "[tokio-dpdk] ERROR listener bind cleanup failed handle={:?} error={}",
                        listen_socket.handle, error
                    );
                }
            }
            driver.release_listener_binding(endpoint);
        })
        .is_none()
        {
            eprintln!(
                "[tokio-dpdk] ERROR DPDK driver unavailable during listener bind cleanup owner={}",
                self.core_id
            );
        }
    }
}

/// A DPDK-backed TCP listener with connection pool support.
///
/// This struct provides a TCP listener that uses DPDK for packet I/O
/// and smoltcp for TCP protocol processing, bypassing the kernel network
/// stack for ultra-low latency.
///
/// # Connection Pool
///
/// Unlike traditional socket implementations, smoltcp uses a single-socket
/// model where a listening socket becomes a connected socket after accepting
/// a connection. This listener implements a socket pool to support multiple
/// concurrent connections:
///
/// 1. Multiple sockets are created in listen state (backlog)
/// 2. When a connection is accepted, the socket is transferred to TcpDpdkStream
/// 3. A new listening socket is automatically created to replenish the pool
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
pub struct TcpDpdkListener {
    /// Interior mutable state
    inner: RefCell<ListenerInner>,
    /// Worker core this listener is bound to
    core_id: usize,
    /// Local address
    local_addr: SocketAddr,
}

impl TcpDpdkListener {
    /// Binds to the specified address.
    ///
    /// This method supports any address type that implements `ToSocketAddrs`,
    /// including hostnames that need DNS resolution.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tokio::net::TcpDpdkListener;
    ///
    /// // Bind with &str
    /// let listener = TcpDpdkListener::bind("0.0.0.0:8080").await?;
    ///
    /// // Or with SocketAddr
    /// use std::net::SocketAddr;
    /// let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    /// let listener = TcpDpdkListener::bind(addr).await?;
    /// ```
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addrs = to_socket_addrs(addr).await?;
        let mut last_err = None;

        for addr in addrs {
            match Self::bind_socket_addr(addr) {
                Ok(listener) => return Ok(listener),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved")
        }))
    }

    /// Binds to a specific SocketAddr.
    ///
    /// This is a synchronous method that takes an already-resolved SocketAddr.
    /// Used by `TcpDpdkSocket::listen` which doesn't do DNS resolution.
    pub fn bind_socket_addr(addr: SocketAddr) -> io::Result<Self> {
        Self::bind_socket_addr_with_backlog(addr, DEFAULT_BACKLOG)
    }

    /// Binds to a specific SocketAddr with custom backlog size.
    pub fn bind_socket_addr_with_backlog(addr: SocketAddr, backlog: usize) -> io::Result<Self> {
        // Get current worker index (must be on DPDK worker thread)
        let core_id = current_worker_index().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "TcpDpdkListener::bind must be called from a DPDK worker thread",
            )
        })?;

        // Allocate listener-owned metadata before acquiring the worker-local
        // endpoint so an allocation failure cannot strand that binding.
        let mut listen_pool = VecDeque::new();
        listen_pool.try_reserve_exact(backlog).map_err(|error| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "failed to allocate TCP listener backlog capacity={} error={}",
                    backlog, error
                ),
            )
        })?;

        let endpoint = Self::listen_endpoint(addr);
        with_current_driver(|driver| {
            driver.reserve_listener_binding(endpoint)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Failed to access DPDK driver"))??;

        // From this point every early return removes all sockets and releases
        // the listener endpoint on this same worker.
        let mut setup = ListenerBindGuard::new(listen_pool, endpoint, core_id);

        // Create at least one listen socket, up to backlog size
        let initial_count = backlog.min(8); // Start with 8, grow on demand
        for _ in 0..initial_count {
            match Self::try_create_listen_socket(addr)? {
                Some(socket) => setup.listen_pool.push_back(socket),
                None => {
                    if setup.listen_pool.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::OutOfMemory,
                            "No buffers available in fixed TCP socket pool",
                        ));
                    }
                    break;
                }
            }
        }
        let listen_pool = setup.disarm();

        Ok(Self {
            inner: RefCell::new(ListenerInner {
                listen_pool,
                backlog,
                buffer_waiter: None,
            }),
            core_id,
            local_addr: addr,
        })
    }

    fn listen_endpoint(addr: SocketAddr) -> smoltcp::wire::IpListenEndpoint {
        match addr {
            SocketAddr::V4(v4) => smoltcp::wire::IpListenEndpoint {
                addr: if v4.ip().is_unspecified() {
                    None
                } else {
                    Some(smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from_octets(
                        v4.ip().octets(),
                    )))
                },
                port: v4.port(),
            },
            SocketAddr::V6(v6) => smoltcp::wire::IpListenEndpoint {
                addr: if v6.ip().is_unspecified() {
                    None
                } else {
                    Some(smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address::from_octets(
                        v6.ip().octets(),
                    )))
                },
                port: v6.port(),
            },
        }
    }

    /// `Ok(None)` is the expected fixed-pool exhaustion state. The caller
    /// either fails setup or installs a pool waiter before returning Pending.
    fn try_create_listen_socket(addr: SocketAddr) -> io::Result<Option<ListenSocket>> {
        let endpoint = Self::listen_endpoint(addr);
        with_current_driver(|driver| {
            driver
                .try_create_tcp_listen_socket(endpoint)
                .map(|handle| handle.map(|handle| ListenSocket { handle }))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver (not on worker thread or driver busy)",
            )
        })?
    }

    /// Replenish only from the existing fixed pool. Exhaustion registers this
    /// listener's waker in a fixed driver slot before returning Pending.
    fn replenish_pool(&self, cx: &Context<'_>) -> io::Result<()> {
        let mut inner = self.inner.borrow_mut();
        let endpoint = Self::listen_endpoint(self.local_addr);
        with_current_driver(|driver| -> io::Result<()> {
            while inner.listen_pool.len() < inner.backlog {
                match driver.try_create_tcp_listen_socket(endpoint) {
                    Ok(Some(handle)) => inner.listen_pool.push_back(ListenSocket { handle }),
                    Ok(None) => {
                        let waiter = match inner.buffer_waiter {
                            Some(waiter) => waiter,
                            None => {
                                let waiter = driver.allocate_tcp_buffer_waiter()?;
                                inner.buffer_waiter = Some(waiter);
                                waiter
                            }
                        };
                        if let Err(error) = driver.register_tcp_buffer_waiter(waiter, cx.waker()) {
                            match driver.release_tcp_buffer_waiter(waiter) {
                                Ok(()) => inner.buffer_waiter = None,
                                Err(release_error) => {
                                    eprintln!(
                                        "[tokio-dpdk] ERROR failed to release listener buffer waiter after registration error waiter={:?} error={}",
                                        waiter, release_error
                                    );
                                }
                            }
                            return Err(error);
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        if let Some(waiter) = inner.buffer_waiter {
                            match driver.release_tcp_buffer_waiter(waiter) {
                                Ok(()) => inner.buffer_waiter = None,
                                Err(release_error) => {
                                    eprintln!(
                                        "[tokio-dpdk] ERROR failed to release listener buffer waiter after listen error waiter={:?} error={}",
                                        waiter, release_error
                                    );
                                }
                            }
                        }
                        return Err(error);
                    }
                }
            }
            if let Some(waiter) = inner.buffer_waiter {
                driver.release_tcp_buffer_waiter(waiter)?;
                inner.buffer_waiter = None;
            }
            Ok(())
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver while replenishing listener pool",
            )
        })?
    }

    /// Accepts a new connection.
    ///
    /// This method waits until a new connection is established, then returns
    /// the connected stream and peer address.
    ///
    /// # Connection Pool Behavior
    ///
    /// When a connection is accepted:
    /// 1. The connected socket is transferred to the returned TcpDpdkStream
    /// 2. A new listening socket is created to replace it in the pool
    /// 3. The listener remains ready to accept more connections
    pub async fn accept(&self) -> io::Result<(TcpDpdkStream, SocketAddr)> {
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpDpdkStream, SocketAddr)>> {
        match current_worker_index() {
            Some(current) if current == self.core_id => {}
            Some(current) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "TcpDpdkListener polled on wrong worker owner={} current={}",
                        self.core_id, current
                    ),
                )));
            }
            None => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "TcpDpdkListener polled outside DPDK worker owner={}",
                        self.core_id
                    ),
                )));
            }
        }

        // Try to replenish pool if it's getting low
        {
            let inner = self.inner.borrow();
            if inner.buffer_waiter.is_some() || inner.listen_pool.len() < inner.backlog / 2 {
                drop(inner);
                if let Err(error) = self.replenish_pool(cx) {
                    return Poll::Ready(Err(error));
                }
            }
        }

        // Check each socket in the pool for an established connection
        let mut found_idx = None;
        let waker = cx.waker();

        {
            let inner = self.inner.borrow();
            for (idx, listen_socket) in inner.listen_pool.iter().enumerate() {
                // Check socket state and register waker
                let is_established = with_current_driver(|driver| {
                    let socket = driver.get_tcp_socket_mut(listen_socket.handle);

                    // Register waker with smoltcp - will be woken when connection established
                    socket.register_recv_waker(waker);

                    socket.is_active() && socket.state() == smoltcp::socket::tcp::State::Established
                });

                if is_established == Some(true) {
                    found_idx = Some(idx);
                    break;
                }
            }
        }

        if let Some(idx) = found_idx {
            // Resolve every fallible property while the listen pool still owns
            // the socket. An early return therefore cannot orphan the handle.
            let connected_handle = self
                .inner
                .borrow()
                .listen_pool
                .get(idx)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("established listener socket index disappeared index={}", idx),
                    )
                })?
                .handle;
            let peer_addr = with_current_driver(|driver| {
                let socket = driver.get_tcp_socket_mut(connected_handle);
                socket.remote_endpoint().map(|ep| {
                    SocketAddr::new(
                        match ep.addr {
                            smoltcp::wire::IpAddress::Ipv4(v4) => {
                                std::net::IpAddr::V4(std::net::Ipv4Addr::from(v4.octets()))
                            }
                            smoltcp::wire::IpAddress::Ipv6(v6) => {
                                std::net::IpAddr::V6(std::net::Ipv6Addr::from(v6.octets()))
                            }
                        },
                        ep.port,
                    )
                })
            })
            .flatten()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No remote endpoint"))?;
            with_current_driver(|driver| {
                driver.claim_established_tcp_flow(connected_handle)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "Failed to access DPDK driver while claiming accepted TCP flow",
                )
            })??;

            // No fallible operation remains before ownership transfers to the
            // stream, so removing the pool entry cannot leak the handle.
            let connected = self
                .inner
                .borrow_mut()
                .listen_pool
                .remove(idx)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("established listener socket index disappeared index={}", idx),
                    )
                })?;

            // Create stream from accepted connection
            let stream = TcpDpdkStream::from_handle(
                connected.handle,
                self.core_id,
                Some(self.local_addr),
                Some(peer_addr),
            );

            // Replenish the pool
            if let Err(error) = self.replenish_pool(cx) {
                drop(stream);
                return Poll::Ready(Err(error));
            }

            Poll::Ready(Ok((stream, peer_addr)))
        } else {
            // No connection ready, return Pending
            // Wakers were registered with smoltcp above
            Poll::Pending
        }
    }

    /// Returns the local address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    /// Returns the worker core this listener is bound to.
    pub fn core_id(&self) -> usize {
        self.core_id
    }

    /// Returns the current number of listening sockets in the pool.
    pub fn pool_size(&self) -> usize {
        self.inner.borrow().listen_pool.len()
    }

    /// Returns the backlog configuration (target pool size).
    pub fn backlog(&self) -> usize {
        self.inner.borrow().backlog
    }

    /// Gets the value of the IP_TTL option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        // Get from first socket in pool
        let inner = self.inner.borrow();
        let ttl = inner.listen_pool.front().and_then(|s| {
            with_current_driver(|driver| {
                let socket = driver.get_tcp_socket_mut(s.handle);
                socket.hop_limit().map(|h| h as u32)
            })
        });

        ttl.flatten()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "No listening sockets available"))
    }

    /// Sets the value of the IP_TTL option (hop limit).
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        let ttl_u8 = ttl.min(255) as u8;

        // Set on all sockets in pool
        let inner = self.inner.borrow();
        for listen_socket in &inner.listen_pool {
            with_current_driver(|driver| {
                let socket = driver.get_tcp_socket_mut(listen_socket.handle);
                socket.set_hop_limit(Some(ttl_u8));
            });
        }
        Ok(())
    }
}

impl Drop for TcpDpdkListener {
    fn drop(&mut self) {
        let current = current_worker_index();
        if current != Some(self.core_id) {
            eprintln!(
                "[tokio-dpdk] ERROR listener cleanup outside owner worker owner={} current={:?}",
                self.core_id, current
            );
            return;
        }

        let endpoint = Self::listen_endpoint(self.local_addr);
        let mut inner = self.inner.borrow_mut();
        if with_current_driver(|driver| {
            if let Some(waiter) = inner.buffer_waiter.take() {
                if let Err(error) = driver.release_tcp_buffer_waiter(waiter) {
                    eprintln!(
                        "[tokio-dpdk] ERROR listener buffer waiter cleanup failed waiter={:?} error={}",
                        waiter, error
                    );
                }
            }
            for listen_socket in inner.listen_pool.drain(..) {
                if let Err(error) = driver.remove_socket(listen_socket.handle) {
                    eprintln!(
                        "[tokio-dpdk] ERROR listener socket cleanup failed handle={:?} error={}",
                        listen_socket.handle, error
                    );
                }
            }
            driver.release_listener_binding(endpoint);
        })
        .is_none()
        {
            eprintln!(
                "[tokio-dpdk] ERROR DPDK driver unavailable during listener cleanup owner={}",
                self.core_id
            );
        }
    }
}

impl std::fmt::Debug for TcpDpdkListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("TcpDpdkListener")
            .field("local_addr", &self.local_addr)
            .field("core_id", &self.core_id)
            .field("pool_size", &inner.listen_pool.len())
            .field("backlog", &inner.backlog)
            .finish()
    }
}

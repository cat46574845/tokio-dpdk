//! TcpDpdkStream implementation.
//!
//! A DPDK-backed TCP stream using smoltcp for protocol processing.

use std::cell::Cell;
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

// Import worker context from dpdk scheduler
use crate::runtime::scheduler::dpdk::{
    current_driver_cleanup, current_worker_identity, with_current_driver, DriverCleanup,
    WorkerIdentity,
};
use crate::runtime::scheduler::dpdk::{RawTailHandle, RawTailParserBinding};

use std::marker::PhantomData;

// =============================================================================
// PeekGuard - Zero-copy access to smoltcp receive buffer
// =============================================================================

/// RAII guard for zero-copy access to smoltcp receive buffer.
///
/// # Safety
/// This guard uses unsafe lifetime extension. The borrowed slice is valid
/// as long as:
/// 1. The guard is not dropped
/// 2. No mutable operations are performed on the socket
/// 3. The stream remains on the same worker thread
///
/// # Important: LinearBuffer Compaction
/// When using LinearBuffer, the peek slice is guaranteed stable because:
/// - `get_allocated(&self, ...)` takes &self (immutable) and never triggers compaction
/// - Compaction only happens on mutating operations (recv, dequeue, etc.)
/// - As long as PeekGuard exists, no mutating operations can be called
pub struct PeekGuard<'a> {
    stream: &'a TcpDpdkStream,
    /// Pointer to data in smoltcp buffer (lifetime-extended)
    data: *const u8,
    len: usize,
    /// Makes this type !Send + !Sync via PhantomData<*const ()>
    /// Note: Explicit `impl !Send` requires nightly, so we use PhantomData instead
    _marker: PhantomData<*const ()>,
}

impl<'a> PeekGuard<'a> {
    /// Returns the peeked data as a byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        if self.data.is_null() || self.len == 0 {
            return &[];
        }
        // SAFETY: Lifetime extended from smoltcp buffer, valid while guard alive
        // LinearBuffer.get_allocated() is &self so no compaction can occur
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }

    /// Returns the length of available data.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if no data is available.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for PeekGuard<'_> {
    fn drop(&mut self) {
        // No-op: data remains in buffer until explicitly consumed
    }
}

// Note: PeekGuard is !Send + !Sync via PhantomData<*const ()>
// The *const () is inherently !Send + !Sync
// =============================================================================

struct OutboundConnectGuard {
    handle: SocketHandle,
    local_port: u16,
    owner: WorkerIdentity,
    cleanup: Option<DriverCleanup>,
}

impl OutboundConnectGuard {
    fn new(
        handle: SocketHandle,
        local_port: u16,
        owner: WorkerIdentity,
        cleanup: DriverCleanup,
    ) -> Self {
        Self {
            handle,
            local_port,
            owner,
            cleanup: Some(cleanup),
        }
    }

    fn handle(&self) -> SocketHandle {
        self.handle
    }

    fn disarm(mut self) -> (SocketHandle, u16) {
        self.cleanup = None;
        (self.handle, self.local_port)
    }
}

impl Drop for OutboundConnectGuard {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup_socket_on_owner(
                self.handle,
                Some(self.local_port),
                self.owner,
                &cleanup,
            );
        }
    }
}

fn cleanup_socket_on_owner(
    handle: SocketHandle,
    outbound_local_port: Option<u16>,
    owner: WorkerIdentity,
    cleanup: &DriverCleanup,
) {
    if cleanup.identity() != owner {
        eprintln!(
            "[tokio-dpdk] ERROR socket cleanup capability mismatch handle={:?} owner={:?} capability={:?}",
            handle,
            owner,
            cleanup.identity()
        );
        return;
    }

    match cleanup.with_driver(|driver| {
        let remove_result = driver.remove_socket(handle);
        if let Some(port) = outbound_local_port {
            driver.release_port(port);
        }
        remove_result
    }) {
        Ok(Some(Ok(()))) | Ok(None) => {}
        Ok(Some(Err(error))) => eprintln!(
            "[tokio-dpdk] ERROR socket cleanup failed handle={:?} error={}",
            handle, error
        ),
        Err(error) => eprintln!(
            "[tokio-dpdk] ERROR socket cleanup owner access failed handle={:?} error={}",
            handle, error
        ),
    }
}

#[inline]
fn validate_worker_owner(
    owner: WorkerIdentity,
    current: Option<WorkerIdentity>,
    operation: &'static str,
) -> io::Result<()> {
    match current {
        Some(current) if current == owner => Ok(()),
        Some(current) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} owner violation: owner {:?}, current worker {:?}",
                operation, owner, current
            ),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{} used outside DPDK worker owner {:?}", operation, owner),
        )),
    }
}

#[inline]
fn ensure_current_worker_owner(
    owner: WorkerIdentity,
    operation: &'static str,
) -> io::Result<()> {
    validate_worker_owner(owner, current_worker_identity(), operation)
}

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
    /// Worker core this stream is bound to
    owner: WorkerIdentity,
    /// Weak, owner-thread-checked capability used only for resource Drop.
    cleanup: DriverCleanup,
    /// Local address (cached)
    local_addr: Option<SocketAddr>,
    /// Peer address (cached)
    peer_addr: Option<SocketAddr>,
    /// Local port owned by an outbound connect; accepted streams leave this empty.
    outbound_local_port: Option<u16>,
    /// Read shutdown flag (smoltcp doesn't support half-close on read side)
    read_shutdown: Cell<bool>,
    /// Write shutdown flag — tracks whether we initiated close (FIN sent)
    /// Used by take_error() to distinguish RST from clean close.
    write_shutdown: Cell<bool>,
    /// Last error encountered (stored as ErrorKind since io::Error doesn't impl Clone)
    last_error: Cell<Option<io::ErrorKind>>,
}

// SAFETY: The three `Cell` fields are worker-local state. Every production
// operation that reads or writes them first validates `owner` against the DPDK
// worker identity, and one WorkerIdentity is permanently bound to one OS thread.
// That worker polls tasks serially, so validated operations cannot execute the
// cell accesses concurrently. Owned split halves may move through `Arc`, but
// their I/O paths perform the same validation before reaching this state. Drop
// may run on another thread after the runtime is reclaimed, but it never reads
// these cells and uses only DriverCleanup. Sync is retained solely for the
// public stream/owned-half type contract; it does not authorize state access
// away from the owning worker.
unsafe impl Sync for TcpDpdkStream {}

impl TcpDpdkStream {
    /// Create a TcpDpdkStream from an existing socket handle.
    ///
    /// This is typically called internally after connection establishment.
    pub(crate) fn from_handle(
        handle: SocketHandle,
        owner: WorkerIdentity,
        cleanup: DriverCleanup,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
    ) -> Self {
        Self {
            handle,
            owner,
            cleanup,
            local_addr,
            peer_addr,
            outbound_local_port: None,
            read_shutdown: Cell::new(false),
            write_shutdown: Cell::new(false),
            last_error: Cell::new(None),
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
        let owner = current_worker_identity().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "TcpDpdkStream::connect must be called from a DPDK worker thread",
            )
        })?;
        let cleanup = current_driver_cleanup().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "DPDK driver cleanup owner unavailable")
        })?;
        assert_eq!(cleanup.identity(), owner, "DPDK stream cleanup owner mismatch");

        let (connect_guard, local_addr) = with_current_driver(|driver| {
            driver.start_tcp_connect(addr, None).map(|started| {
                (
                    OutboundConnectGuard::new(
                        started.handle,
                        started.local_addr.port(),
                        owner,
                        cleanup.clone(),
                    ),
                    started.local_addr,
                )
            })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver (not on worker thread or driver busy)",
            )
        })??;
        let handle = connect_guard.handle();

        // Wait for connection establishment by polling until socket is connected
        // TCP handshake typically completes in < 1 second, use 30 second timeout
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        let mut last_state_log = std::time::Instant::now();

        loop {
            ensure_current_worker_owner(owner, "TcpDpdkStream::connect")?;

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

        let (handle, outbound_local_port) = connect_guard.disarm();

        Ok(Self {
            handle,
            owner,
            cleanup,
            local_addr: Some(local_addr),
            peer_addr: Some(addr),
            outbound_local_port: Some(outbound_local_port),
            read_shutdown: Cell::new(false),
            write_shutdown: Cell::new(false),
            last_error: Cell::new(None),
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
        let owner = current_worker_identity().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "TcpDpdkStream::connect must be called from a DPDK worker thread",
            )
        })?;
        let cleanup = current_driver_cleanup().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "DPDK driver cleanup owner unavailable")
        })?;
        assert_eq!(cleanup.identity(), owner, "DPDK stream cleanup owner mismatch");

        let (connect_guard, result_local_addr) = with_current_driver(|driver| {
            driver
                .start_tcp_connect(remote_addr, Some(local_addr))
                .map(|started| {
                    (
                        OutboundConnectGuard::new(
                            started.handle,
                            started.local_addr.port(),
                            owner,
                            cleanup.clone(),
                        ),
                        started.local_addr,
                    )
                })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to access DPDK driver (not on worker thread or driver busy)",
            )
        })??;
        let handle = connect_guard.handle();

        // Wait for connection establishment
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);

        loop {
            ensure_current_worker_owner(owner, "TcpDpdkStream::connect_with_local")?;

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

        let (handle, outbound_local_port) = connect_guard.disarm();

        Ok(Self {
            handle,
            owner,
            cleanup,
            local_addr: Some(result_local_addr),
            peer_addr: Some(remote_addr),
            outbound_local_port: Some(outbound_local_port),
            read_shutdown: Cell::new(false),
            write_shutdown: Cell::new(false),
            last_error: Cell::new(None),
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
        self.owner.worker_index
    }

    /// Assert that we are on the correct worker thread.
    /// Panics if called from a different worker than where the stream was created.
    #[inline]
    fn assert_on_correct_worker(&self) {
        self.assert_on_worker_identity(current_worker_identity());
    }

    #[inline]
    fn assert_on_worker_identity(&self, current: Option<WorkerIdentity>) {
        if let Err(error) = validate_worker_owner(self.owner, current, "TcpDpdkStream") {
            panic!("{}", error);
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
            driver.mark_socket_egress_pending(handle);
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
        self.assert_on_correct_worker();
        let handle = self.handle;
        let waker = cx.waker();

        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket.register_recv_waker(waker);
            socket.can_recv()
                || socket.state() == smoltcp::socket::tcp::State::CloseWait
                || socket.state() == smoltcp::socket::tcp::State::Closed
        });

        match result {
            Some(true) => Poll::Ready(Ok(())),
            Some(false) => Poll::Pending,
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            ))),
        }
    }

    /// Poll for write readiness.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.assert_on_correct_worker();
        let handle = self.handle;
        let waker = cx.waker();

        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket.register_send_waker(waker);
            socket.can_send()
        });

        match result {
            Some(true) => Poll::Ready(Ok(())),
            Some(false) => Poll::Pending,
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            ))),
        }
    }

    /// Clear read readiness (for use after WouldBlock).
    /// Note: With smoltcp native wakers, this is now a no-op.
    pub(crate) fn clear_read_ready(&self) {
        // No-op with smoltcp native wakers
    }

    /// Clear write readiness (for use after WouldBlock).
    /// Note: With smoltcp native wakers, this is now a no-op.
    pub(crate) fn clear_write_ready(&self) {
        // No-op with smoltcp native wakers
    }

    // =========================================================================
    // New API parity methods
    // =========================================================================

    /// Waits for any of the requested ready states.
    ///
    /// Returns as soon as **any** of the requested interests becomes ready,
    /// matching tokio's `TcpStream::ready()` semantics. The returned `Ready`
    /// value indicates which interests are ready — it may be a subset of
    /// what was requested.
    ///
    /// This function is usually paired with `try_read()` or `try_write()`.
    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        poll_fn(|cx| {
            let mut ready = Ready::EMPTY;

            if interest.is_readable() {
                if let Poll::Ready(result) = self.poll_read_ready(cx) {
                    result?;
                    ready |= Ready::READABLE;
                }
            }

            if interest.is_writable() {
                if let Poll::Ready(result) = self.poll_write_ready(cx) {
                    result?;
                    ready |= Ready::WRITABLE;
                }
            }

            if ready.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(ready))
            }
        })
        .await
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

    // =========================================================================
    // Zero-copy receive API
    // =========================================================================

    /// Wait for receive buffer data size to change.
    ///
    /// This method blocks until NEW data arrives (buffer size increases),
    /// not just when data is present. This is critical for partial message
    /// processing where the caller needs to wait for additional bytes.
    ///
    /// Returns the NEW total number of bytes available for reading.
    /// Returns `Ok(0)` on EOF (connection closed by peer).
    pub async fn wait_recv(&self) -> io::Result<usize> {
        self.assert_on_correct_worker();

        // Capture initial buffer size
        let initial_size = with_current_driver(|driver| {
            driver.get_tcp_socket_mut(self.handle).recv_queue()
        }).unwrap_or(0);

        poll_fn(|cx| {
            self.assert_on_correct_worker();

            let handle = self.handle;
            let waker = cx.waker();

            let result = with_current_driver(|driver| {
                let socket = driver.get_tcp_socket_mut(handle);
                socket.register_recv_waker(waker);

                let current_size = socket.recv_queue();

                // Check for EOF
                if socket.state() == smoltcp::socket::tcp::State::CloseWait
                    || socket.state() == smoltcp::socket::tcp::State::Closed {
                    return Ok(0); // EOF
                }

                // Wait for size CHANGE, not just "has data"
                if current_size > initial_size {
                    Ok(current_size)
                } else {
                    Err(io::ErrorKind::WouldBlock)
                }
            });

            match result {
                Some(Ok(n)) => Poll::Ready(Ok(n)),
                Some(Err(io::ErrorKind::WouldBlock)) => Poll::Pending,
                Some(Err(kind)) => Poll::Ready(Err(io::Error::new(kind, "socket error"))),
                None => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, "driver unavailable"))),
            }
        }).await
    }

    /// Try to peek at receive buffer without consuming data (zero-copy).
    ///
    /// Returns a guard providing zero-copy access to the buffer.
    /// The guard borrows from smoltcp's internal buffer using unsafe lifetime extension.
    ///
    /// # Safety
    /// The returned `PeekGuard` contains a pointer to smoltcp's internal buffer.
    /// This is safe because:
    /// 1. LinearBuffer's `get_allocated` is `&self` and never triggers compaction
    /// 2. The guard holds `&TcpDpdkStream` preventing concurrent mutable access
    /// 3. Data remains valid until `consume_recv()` is called
    pub fn try_peek_zero_copy(&self) -> io::Result<PeekGuard<'_>> {
        self.assert_on_correct_worker();

        if self.read_shutdown.get() {
            return Ok(PeekGuard {
                stream: self,
                data: std::ptr::null(),
                len: 0,
                _marker: PhantomData,
            });
        }

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);

            if !socket.can_recv() {
                if socket.state() == smoltcp::socket::tcp::State::CloseWait
                    || socket.state() == smoltcp::socket::tcp::State::Closed {
                    // EOF: return empty guard
                    return Ok((std::ptr::null(), 0));
                }
                return Err(io::ErrorKind::WouldBlock);
            }

            // Peek at all available data
            // Note: smoltcp socket.peek(&mut self, size) returns Result<&[u8], RecvError>
            // The returned slice lifetime is tied to the &mut self borrow
            // We use unsafe to extend this lifetime to the PeekGuard
            let queue_len = socket.recv_queue();
            match socket.peek(queue_len) {
                Ok(data) => {
                    // SAFETY: Extend lifetime - data is valid because:
                    // 1. LinearBuffer.get_allocated() is &self (no compaction during peek)
                    // 2. Data remains valid until recv()/dequeue operations
                    // 3. PeekGuard holds &TcpDpdkStream preventing concurrent mutable access
                    let ptr = data.as_ptr();
                    let len = data.len();
                    Ok((ptr, len))
                }
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok((ptr, len))) => Ok(PeekGuard {
                stream: self,
                data: ptr,
                len,
                _marker: PhantomData,
            }),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Some(Err(kind)) => Err(io::Error::new(kind, "socket peek error")),
            None => Err(io::Error::new(io::ErrorKind::Other, "driver unavailable")),
        }
    }

    /// Peek at receive buffer (zero-copy), waiting if necessary.
    pub async fn peek_zero_copy(&self) -> io::Result<PeekGuard<'_>> {
        loop {
            self.readable().await?;
            match self.try_peek_zero_copy() {
                Ok(guard) => return Ok(guard),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Consume (discard) bytes from the receive buffer.
    ///
    /// Call this after processing data obtained via `try_peek_zero_copy()`.
    pub fn consume_recv(&self, n: usize) -> io::Result<()> {
        self.assert_on_correct_worker();

        if n == 0 {
            return Ok(());
        }

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let result = {
                let socket = driver.get_tcp_socket_mut(handle);

                // recv() with a closure that just discards the data
                match socket.recv(|data| {
                    let consume = n.min(data.len());
                    (consume, consume)
                }) {
                    Ok(consumed) => Ok(consumed),
                    Err(_) => Err(io::ErrorKind::Other),
                }
            };
            if matches!(result, Ok(consumed) if consumed > 0) {
                driver.mark_socket_egress_pending(handle);
            }
            result.map(|_| ())
        });

        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(kind)) => Err(io::Error::new(kind, "consume error")),
            None => Err(io::Error::new(io::ErrorKind::Other, "driver unavailable")),
        }
    }

    /// Returns the number of bytes currently queued in the TCP receive buffer.
    pub fn recv_queue_len(&self) -> io::Result<usize> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        with_current_driver(|driver| driver.get_tcp_socket_mut(handle).recv_queue())
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "driver unavailable"))
    }

    /// Return the next TCP sequence number expected from the remote peer.
    pub fn recv_next_seq(&self) -> io::Result<u32> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        with_current_driver(|driver| driver.get_tcp_socket_mut(handle).recv_next_seq())
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "driver unavailable"))
    }

    pub(crate) unsafe fn activate_reserved_raw_tail_parser(
        &self,
        handle: RawTailHandle,
        parser: RawTailParserBinding,
    ) -> io::Result<()> {
        // The public unsafe wrapper requires this stream/socket to outlive the
        // raw-tail binding, preventing SocketHandle index reuse before parser
        // removal.
        let current = current_worker_identity().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "raw-tail parser activation requires its DPDK worker thread",
            )
        })?;
        if current != self.owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "TcpDpdkStream belongs to a different DPDK runtime or worker",
            ));
        }
        if handle.owner() != self.owner {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail reservation belongs to a different DPDK runtime or worker",
            ));
        }
        with_current_driver(|driver| {
            driver.activate_raw_tail_parser(handle, self.handle, parser)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "DPDK driver unavailable"))?
    }

    /// Tries to read data from the stream into the provided buffer.
    ///
    /// Returns the number of bytes read. Returns `WouldBlock` if not ready.
    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        // Check read shutdown flag
        if self.read_shutdown.get() {
            return Ok(0); // EOF
        }

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let result = {
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
            };
            if matches!(result, Ok(n) if n > 0) {
                driver.mark_socket_egress_pending(handle);
            }
            result
        });

        match result {
            Some(Ok(n)) => Ok(n),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.clear_read_ready();
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Some(Err(kind)) => {
                self.store_error(kind);
                Err(io::Error::new(kind, "socket read error"))
            }
            None => {
                self.store_error(io::ErrorKind::Other);
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Cannot access DPDK driver",
                ))
            }
        }
    }

    /// Tries to write data to the stream.
    ///
    /// Returns the number of bytes written. Returns `WouldBlock` if not ready.
    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        let handle = self.handle;
        let result = with_current_driver(|driver| {
            let result = {
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
            };
            if matches!(result, Ok(n) if n > 0) {
                driver.mark_socket_egress_pending(handle);
            }
            result
        });

        match result {
            Some(Ok(n)) => Ok(n),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.clear_write_ready();
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Some(Err(kind)) => {
                self.store_error(kind);
                Err(io::Error::new(kind, "socket write error"))
            }
            None => {
                self.store_error(io::ErrorKind::Other);
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Cannot access DPDK driver",
                ))
            }
        }
    }

    /// Tries to flush pending DPDK TCP egress without registering a waker.
    pub fn try_flush(&self) -> io::Result<()> {
        self.assert_on_correct_worker();
        self.flush_std()
    }

    /// Tries to read data into multiple buffers (vectored I/O).
    ///
    /// smoltcp doesn't support native vectored I/O, so we iterate over buffers.
    pub fn try_read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.assert_on_correct_worker();

        // Check read shutdown
        if self.read_shutdown.get() {
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

        if self.read_shutdown.get() {
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

        if self.read_shutdown.get() {
            return Poll::Ready(Ok(0));
        }

        // Register waker and check readiness via poll_read_ready
        match self.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }

        match self.try_peek(buf.initialize_unfilled()) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Waker already registered via poll_read_ready
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

    /// Gets the TCP keep-alive interval.
    ///
    /// Returns `Some(duration)` if keep-alive is enabled, `None` if disabled.
    /// In smoltcp, keep-alive is controlled by a single `Option<Duration>` where
    /// `None` means disabled and `Some(interval)` means enabled with that interval.
    pub fn keep_alive(&self) -> io::Result<Option<std::time::Duration>> {
        self.assert_on_correct_worker();
        let handle = self.handle;

        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket
                .keep_alive()
                .map(|d| std::time::Duration::from_millis(d.total_millis() as u64))
        });

        result.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Cannot access DPDK driver"))
    }

    /// Sets the TCP keep-alive interval.
    ///
    /// Pass `Some(duration)` to enable keep-alive with the specified interval,
    /// or `None` to disable keep-alive.
    pub fn set_keep_alive(&self, interval: Option<std::time::Duration>) -> io::Result<()> {
        self.assert_on_correct_worker();
        let handle = self.handle;

        with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            socket.set_keep_alive(
                interval.map(|d| smoltcp::time::Duration::from_millis(d.as_millis() as u64)),
            );
            driver.mark_socket_egress_pending(handle);
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Cannot access DPDK driver"))
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
            driver.mark_socket_egress_pending(handle);
        });
        Ok(())
    }

    /// Returns the value of the SO_ERROR option.
    ///
    /// Checks two sources of errors:
    /// 1. **Stored errors** from I/O operations (e.g., `BrokenPipe` from writing
    ///    to a closed connection). These are returned with take semantics — the
    ///    stored error is cleared after retrieval.
    /// 2. **Socket state** — if the smoltcp socket has transitioned to `Closed`
    ///    without our side initiating shutdown, this indicates a connection reset
    ///    (RST received from peer).
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.assert_on_correct_worker();

        // 1. Check stored error first (take semantics)
        if let Some(kind) = self.last_error.take() {
            return Ok(Some(io::Error::from(kind)));
        }

        // 2. Actively probe socket state for async errors (RST received, etc.)
        //    If socket is Closed and we never initiated write shutdown,
        //    the peer sent RST → ConnectionReset.
        let we_closed = self.write_shutdown.get();

        if we_closed {
            // We initiated close — Closed state is expected, not an error
            return Ok(None);
        }

        let handle = self.handle;
        let state_error = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(handle);
            match socket.state() {
                smoltcp::socket::tcp::State::Closed => {
                    // Socket is closed but we didn't initiate → RST from peer
                    Some(io::ErrorKind::ConnectionReset)
                }
                _ => None,
            }
        })
        .flatten();

        Ok(state_error.map(io::Error::from))
    }

    /// Store an error kind for later retrieval via `take_error()`.
    fn store_error(&self, kind: io::ErrorKind) {
        self.assert_on_correct_worker();
        self.last_error.set(Some(kind));
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
                self.read_shutdown.set(true);
            }
            Shutdown::Write => {
                self.write_shutdown.set(true);
                let handle = self.handle;
                with_current_driver(|driver| {
                    let socket = driver.get_tcp_socket_mut(handle);
                    socket.close();
                    driver.mark_socket_egress_pending(handle);
                });
            }
            Shutdown::Both => {
                self.read_shutdown.set(true);
                self.write_shutdown.set(true);
                let handle = self.handle;
                with_current_driver(|driver| {
                    let socket = driver.get_tcp_socket_mut(handle);
                    socket.close();
                    driver.mark_socket_egress_pending(handle);
                });
            }
        }
        Ok(())
    }

    pub(super) fn flush_std(&self) -> io::Result<()> {
        self.assert_on_correct_worker();
        let handle = self.handle;
        with_current_driver(|driver| driver.flush_socket_egress(handle))
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Cannot access DPDK driver"))?
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

        // Read from smoltcp socket via worker context
        let handle = self.handle;
        let waker = cx.waker();

        let result = with_current_driver(|driver| {
            let result = {
                let socket = driver.get_tcp_socket_mut(handle);

                // Register waker with smoltcp BEFORE checking state
                // smoltcp will call wake() when socket becomes readable
                socket.register_recv_waker(waker);

                // Check if socket can receive
                if !socket.can_recv() {
                    if socket.state() == smoltcp::socket::tcp::State::CloseWait
                        || socket.state() == smoltcp::socket::tcp::State::Closed
                    {
                        // Connection closed
                        return Ok(0);
                    }
                    // Not ready yet - waker is registered, smoltcp will wake us
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
            };
            if matches!(result, Ok(n) if n > 0) {
                driver.mark_socket_egress_pending(handle);
            }
            result
        });

        match result {
            Some(Ok(0)) => Poll::Ready(Ok(())), // EOF
            Some(Ok(_n)) => Poll::Ready(Ok(())),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                // Not ready - waker already registered with smoltcp
                Poll::Pending
            }
            Some(Err(kind)) => {
                self.store_error(kind);
                Poll::Ready(Err(io::Error::new(kind, "socket read error")))
            }
            None => {
                self.store_error(io::ErrorKind::Other);
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

        // Write to smoltcp socket via worker context
        let handle = self.handle;
        let waker = cx.waker();

        let result = with_current_driver(|driver| {
            let result = {
                let socket = driver.get_tcp_socket_mut(handle);

                // Register waker with smoltcp BEFORE checking state
                // smoltcp will call wake() when socket becomes writable
                socket.register_send_waker(waker);

                // Check if socket can send
                if !socket.can_send() {
                    let state = socket.state();
                    if state == smoltcp::socket::tcp::State::Closed
                        || state == smoltcp::socket::tcp::State::Closing
                    {
                        return Err(io::ErrorKind::BrokenPipe);
                    }
                    // Not ready yet - waker is registered, smoltcp will wake us
                    return Err(io::ErrorKind::WouldBlock);
                }

                // Write data to socket
                match socket.send_slice(buf) {
                    Ok(n) => Ok(n),
                    Err(_) => Err(io::ErrorKind::WouldBlock),
                }
            };
            if matches!(result, Ok(n) if n > 0) {
                driver.mark_socket_egress_pending(handle);
            }
            result
        });

        match result {
            Some(Ok(n)) => Poll::Ready(Ok(n)),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                // Not ready - waker already registered with smoltcp
                Poll::Pending
            }
            Some(Err(kind)) => {
                self.store_error(kind);
                Poll::Ready(Err(io::Error::new(kind, "socket write error")))
            }
            None => {
                self.store_error(io::ErrorKind::Other);
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Cannot access DPDK driver",
                )))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.flush_std())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdown_std(Shutdown::Write)?;
        self.flush_std()?;
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// Drop implementation
// =============================================================================

impl Drop for TcpDpdkStream {
    fn drop(&mut self) {
        cleanup_socket_on_owner(
            self.handle,
            self.outbound_local_port.take(),
            self.owner,
            &self.cleanup,
        );
    }
}

impl std::fmt::Debug for TcpDpdkStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpDpdkStream")
            .field("handle", &format_args!("{:?}", self.handle))
            .field("owner", &self.owner)
            .field("local_addr", &self.local_addr)
            .field("peer_addr", &self.peer_addr)
            .field("outbound_local_port", &self.outbound_local_port)
            .finish()
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::mem::ManuallyDrop;

    fn test_owner(worker_index: usize) -> WorkerIdentity {
        WorkerIdentity::new(
            crate::runtime::scheduler::dpdk::DpdkRuntimeId::allocate()
                .expect("test runtime id must allocate"),
            worker_index,
        )
    }

    trait AmbiguousIfSend<Marker> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfSend<()> for T {}

    struct ImplementsSend;

    impl<T: ?Sized + Send> AmbiguousIfSend<ImplementsSend> for T {}

    #[test]
    fn socket_wrappers_restore_public_auto_traits() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<OutboundConnectGuard>();
        assert_send_sync::<TcpDpdkStream>();
        assert_send_sync::<crate::net::tcp_dpdk::ReadHalf<'static>>();
        assert_send_sync::<crate::net::tcp_dpdk::WriteHalf<'static>>();
        assert_send_sync::<crate::net::tcp_dpdk::OwnedReadHalf>();
        assert_send_sync::<crate::net::tcp_dpdk::OwnedWriteHalf>();
        assert_send::<crate::net::tcp_dpdk::TcpDpdkListener>();
    }

    #[test]
    fn zero_copy_peek_guard_remains_worker_local() {
        let _ = <PeekGuard<'static> as AmbiguousIfSend<_>>::marker;
    }

    #[test]
    fn connect_and_wait_futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}

        let connect = TcpDpdkStream::connect(SocketAddr::from(([127, 0, 0, 1], 9)));
        assert_send(&connect);
        let socket_connect = crate::net::TcpDpdkSocket::new_v4()
            .expect("test socket blueprint must construct")
            .connect(SocketAddr::from(([127, 0, 0, 1], 9)));
        assert_send(&socket_connect);

        let owner = test_owner(7);
        let stream = ManuallyDrop::new(TcpDpdkStream::from_handle(
            SocketHandle::default(),
            owner,
            DriverCleanup::dead_for_test(owner),
            None,
            None,
        ));
        let wait = stream.wait_recv();
        assert_send(&wait);
        assert_send(&stream.readable());
        assert_send(&stream.writable());
        assert_send(&stream.ready(Interest::READABLE | Interest::WRITABLE));
    }

    #[test]
    fn owner_validation_rejects_other_worker_and_outside_runtime() {
        let owner = test_owner(8);
        assert!(validate_worker_owner(owner, Some(owner), "test operation").is_ok());

        let other_worker = WorkerIdentity::new(owner.runtime_id, owner.worker_index + 1);
        let wrong_worker = validate_worker_owner(owner, Some(other_worker), "test operation")
            .expect_err("another worker must not access the socket handle");
        assert_eq!(wrong_worker.kind(), io::ErrorKind::PermissionDenied);
        assert!(wrong_worker.to_string().contains("current worker"));

        let outside = validate_worker_owner(owner, None, "test operation")
            .expect_err("a non-DPDK thread must not access the socket handle");
        assert_eq!(outside.kind(), io::ErrorKind::Other);
        assert!(outside.to_string().contains("outside DPDK worker"));
    }

    #[test]
    fn owner_rejection_precedes_worker_local_cell_access() {
        let owner = test_owner(10);
        let stream = ManuallyDrop::new(TcpDpdkStream::from_handle(
            SocketHandle::default(),
            owner,
            DriverCleanup::dead_for_test(owner),
            None,
            None,
        ));
        stream.last_error.set(Some(io::ErrorKind::BrokenPipe));

        let other_worker = WorkerIdentity::new(owner.runtime_id, owner.worker_index + 1);
        let wrong_worker = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stream.assert_on_worker_identity(Some(other_worker));
        }));
        assert!(wrong_worker.is_err(), "wrong worker must be rejected");
        assert_eq!(
            stream.last_error.get(),
            Some(io::ErrorKind::BrokenPipe),
            "wrong-worker rejection must precede worker-local state access"
        );

        let take_error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = stream.take_error();
        }));
        assert!(take_error.is_err(), "outside-owner take_error must fail first");
        assert_eq!(
            stream.last_error.get(),
            Some(io::ErrorKind::BrokenPipe),
            "owner rejection must leave the worker-local error cell untouched"
        );

        let shutdown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = stream.shutdown_std(Shutdown::Both);
        }));
        assert!(shutdown.is_err(), "outside-owner shutdown must fail first");
        assert!(!stream.read_shutdown.get());
        assert!(!stream.write_shutdown.get());
    }

    #[test]
    fn reclaimed_owner_stream_can_move_and_drop_on_another_thread() {
        let owner = test_owner(9);
        let stream = TcpDpdkStream::from_handle(
            SocketHandle::default(),
            owner,
            DriverCleanup::dead_for_test(owner),
            None,
            None,
        );

        std::thread::spawn(move || drop(stream))
            .join()
            .expect("late stream Drop after owner reclamation must remain safe");
    }

    #[test]
    fn reclaimed_owner_split_halves_can_move_and_drop_on_other_threads() {
        let owner = test_owner(11);
        let stream = TcpDpdkStream::from_handle(
            SocketHandle::default(),
            owner,
            DriverCleanup::dead_for_test(owner),
            None,
            None,
        );
        let (read_half, write_half) = stream.into_split();

        let read_drop = std::thread::spawn(move || drop(read_half));
        let write_drop = std::thread::spawn(move || drop(write_half));
        read_drop
            .join()
            .expect("owned read half must remain movable after owner reclamation");
        write_drop
            .join()
            .expect("owned write half must remain movable after owner reclamation");
    }

    #[test]
    fn pending_connect_guard_retains_owner_cleanup_capability() {
        let owner = test_owner(3);
        let guard = ManuallyDrop::new(OutboundConnectGuard::new(
            SocketHandle::default(),
            50_000,
            owner,
            DriverCleanup::dead_for_test(owner),
        ));
        assert_eq!(
            guard.cleanup.as_ref().map(DriverCleanup::identity),
            Some(owner)
        );
    }

    #[test]
    fn successful_connect_transfers_guard_ownership_without_cleanup() {
        let owner = test_owner(4);
        let guard = OutboundConnectGuard::new(
            SocketHandle::default(),
            50_001,
            owner,
            DriverCleanup::dead_for_test(owner),
        );
        let (handle, port) = guard.disarm();
        assert_eq!(handle, SocketHandle::default());
        assert_eq!(port, 50_001);
    }

    #[test]
    fn accepted_stream_does_not_own_listener_port() {
        let owner = test_owner(0);
        let stream = ManuallyDrop::new(TcpDpdkStream::from_handle(
            SocketHandle::default(),
            owner,
            DriverCleanup::dead_for_test(owner),
            None,
            None,
        ));
        assert!(stream.outbound_local_port.is_none());
    }
}

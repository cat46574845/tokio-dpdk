//! Split functionality for TcpDpdkStream.
//!
//! Provides borrowed and owned split halves for concurrent read/write.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;

use crate::io::{AsyncRead, AsyncWrite, ReadBuf};
use crate::runtime::scheduler::dpdk::with_current_driver;
use std::net::Shutdown;

use super::stream::TcpDpdkStream;

// =============================================================================
// Borrowed halves
// =============================================================================

/// Borrowed read half of a TcpDpdkStream.
#[derive(Debug)]
pub struct ReadHalf<'a> {
    stream: &'a TcpDpdkStream,
}

impl<'a> ReadHalf<'a> {
    pub(super) fn new(stream: &'a TcpDpdkStream) -> Self {
        Self { stream }
    }

    /// Returns the local address of this stream.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.local_addr()
    }

    /// Returns the peer address of this stream.
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }

    /// Returns the socket handle.
    fn handle(&self) -> SocketHandle {
        self.stream.handle()
    }
}

impl AsyncRead for ReadHalf<'_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Wait for read readiness
        match self.stream.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
        }

        // Read from smoltcp socket via worker context
        let handle = self.handle();
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

            match socket.recv_slice(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    Ok(n)
                }
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(0)) => Poll::Ready(Ok(())),
            Some(Ok(_n)) => Poll::Ready(Ok(())),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.stream.clear_read_ready();
                Poll::Pending
            }
            Some(Err(kind)) => Poll::Ready(Err(io::Error::new(kind, "socket read error"))),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            ))),
        }
    }
}

/// Borrowed write half of a TcpDpdkStream.
#[derive(Debug)]
pub struct WriteHalf<'a> {
    stream: &'a TcpDpdkStream,
}

impl<'a> WriteHalf<'a> {
    pub(super) fn new(stream: &'a TcpDpdkStream) -> Self {
        Self { stream }
    }

    /// Returns the local address of this stream.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.local_addr()
    }

    /// Returns the peer address of this stream.
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }

    /// Returns the socket handle.
    fn handle(&self) -> SocketHandle {
        self.stream.handle()
    }
}

impl AsyncWrite for WriteHalf<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Wait for write readiness
        match self.stream.poll_write_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
        }

        // Write to smoltcp socket via worker context
        let handle = self.handle();
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
            Some(Ok(n)) => Poll::Ready(Ok(n)),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.stream.clear_write_ready();
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
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream.shutdown_std(Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// Owned halves
// =============================================================================

/// Owned read half of a TcpDpdkStream.
#[derive(Debug)]
pub struct OwnedReadHalf {
    stream: Arc<TcpDpdkStream>,
}

impl OwnedReadHalf {
    pub(super) fn new(stream: Arc<TcpDpdkStream>) -> Self {
        Self { stream }
    }

    /// Returns the local address of this stream.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.local_addr()
    }

    /// Returns the peer address of this stream.
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }

    /// Returns the socket handle.
    fn handle(&self) -> SocketHandle {
        self.stream.handle()
    }
}

impl AsyncRead for OwnedReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Wait for read readiness
        match self.stream.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
        }

        // Read from smoltcp socket via worker context
        let handle = self.handle();
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

            match socket.recv_slice(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    Ok(n)
                }
                Err(_) => Err(io::ErrorKind::WouldBlock),
            }
        });

        match result {
            Some(Ok(0)) => Poll::Ready(Ok(())),
            Some(Ok(_n)) => Poll::Ready(Ok(())),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.stream.clear_read_ready();
                Poll::Pending
            }
            Some(Err(kind)) => Poll::Ready(Err(io::Error::new(kind, "socket read error"))),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot access DPDK driver",
            ))),
        }
    }
}

/// Owned write half of a TcpDpdkStream.
#[derive(Debug)]
pub struct OwnedWriteHalf {
    stream: Arc<TcpDpdkStream>,
}

impl OwnedWriteHalf {
    pub(super) fn new(stream: Arc<TcpDpdkStream>) -> Self {
        Self { stream }
    }

    /// Returns the local address of this stream.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.local_addr()
    }

    /// Returns the peer address of this stream.
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }

    /// Returns the socket handle.
    fn handle(&self) -> SocketHandle {
        self.stream.handle()
    }
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Wait for write readiness
        match self.stream.poll_write_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
        }

        // Write to smoltcp socket via worker context
        let handle = self.handle();
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
            Some(Ok(n)) => Poll::Ready(Ok(n)),
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.stream.clear_write_ready();
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
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream.shutdown_std(Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

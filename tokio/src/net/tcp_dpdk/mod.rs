//! DPDK-based TCP implementation.
//!
//! This module provides TCP stream and listener that uses DPDK/smoltcp for packet I/O,
//! bypassing the kernel network stack for ultra-low latency networking.
//!
//! # Usage
//!
//! ```ignore
//! use tokio::net::{TcpDpdkStream, TcpDpdkListener, TcpDpdkSocket};
//!
//! // Client connection with hostname (DNS resolution via ToSocketAddrs)
//! let stream = TcpDpdkStream::connect("example.com:8080").await?;
//!
//! // Client connection with IP string
//! let stream = TcpDpdkStream::connect("1.2.3.4:8080").await?;
//!
//! // Server listener with string address
//! let listener = TcpDpdkListener::bind("0.0.0.0:8080").await?;
//! let (stream, addr) = listener.accept().await?;
//!
//! // Server listener with tuple
//! let listener = TcpDpdkListener::bind(("0.0.0.0", 8080)).await?;
//!
//! // Pre-configured socket
//! let socket = TcpDpdkSocket::new_v4()?;
//! socket.set_nodelay(true)?;
//! let stream = socket.connect("1.2.3.4:8080".parse()?).await?;
//! ```

mod listener;
mod socket;
mod split;
mod stream;

pub use listener::TcpDpdkListener;
pub use socket::TcpDpdkSocket;
pub use split::{OwnedReadHalf, OwnedWriteHalf, ReadHalf, WriteHalf};
pub use stream::TcpDpdkStream;

//! DPDK-based TCP implementation.
//!
//! This module provides TCP stream and listener that uses DPDK/smoltcp for packet I/O,
//! bypassing the kernel network stack for ultra-low latency networking.
//!
//! # Usage
//!
//! ```ignore
//! use tokio::net::{TcpDpdkStream, TcpDpdkListener};
//!
//! // Client connection
//! let stream = TcpDpdkStream::connect("127.0.0.1:8080").await?;
//!
//! // Server listener
//! let listener = TcpDpdkListener::bind("0.0.0.0:8080".parse().unwrap()).await?;
//! let (stream, addr) = listener.accept().await?;
//! ```

mod listener;
mod split;
mod stream;

pub use listener::TcpDpdkListener;
pub use split::{OwnedReadHalf, OwnedWriteHalf, ReadHalf, WriteHalf};
pub use stream::TcpDpdkStream;

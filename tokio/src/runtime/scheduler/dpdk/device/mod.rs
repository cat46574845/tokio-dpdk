//! DPDK device implementation for smoltcp integration.
//!
//! This module provides `DpdkDevice` which implements `smoltcp::phy::Device`,
//! allowing DPDK to be used as the network backend for smoltcp's TCP/IP stack.

use std::ptr;

use smoltcp::phy::{Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant as SmolInstant;

use super::ffi;
use super::raw_tail::{ParsedTcpPacket, RawTailTable};

// =============================================================================
// Configuration constants
// =============================================================================

/// Default packets requested per rx_burst call.
const RX_BURST_SIZE_DEFAULT: u16 = 256;

/// Maximum packets per tx_burst call
const TX_BURST_SIZE: u16 = 32;

/// Default preallocated mbuf pointers retained for one full driver poll.
const RX_DRAIN_BATCH_CAP_DEFAULT: usize = 262_144;

/// Default MTU (Maximum Transmission Unit)
const DEFAULT_MTU: usize = 1500;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DrainRxStats {
    pub(crate) received: usize,
    pub(crate) raw_tail_captured: usize,
    pub(crate) smoltcp_pending: usize,
}

impl DrainRxStats {
    #[inline(always)]
    pub(crate) fn received_any(self) -> bool {
        self.received != 0
    }
}

fn configured_rx_burst_size() -> u16 {
    let Some(value) = std::env::var_os("TOKIO_DPDK_RX_BURST_SIZE") else {
        return RX_BURST_SIZE_DEFAULT;
    };
    let value = value
        .into_string()
        .unwrap_or_else(|_| panic!("TOKIO_DPDK_RX_BURST_SIZE is not valid UTF-8"));
    let parsed = value
        .parse::<u16>()
        .unwrap_or_else(|e| panic!("TOKIO_DPDK_RX_BURST_SIZE parse failed value={} error={}", value, e));
    if parsed == 0 {
        panic!("TOKIO_DPDK_RX_BURST_SIZE must be greater than zero");
    }
    parsed
}

fn configured_rx_drain_batch_cap() -> usize {
    let Some(value) = std::env::var_os("TOKIO_DPDK_RX_DRAIN_BATCH_CAP") else {
        return RX_DRAIN_BATCH_CAP_DEFAULT;
    };
    let value = value
        .into_string()
        .unwrap_or_else(|_| panic!("TOKIO_DPDK_RX_DRAIN_BATCH_CAP is not valid UTF-8"));
    let parsed = value
        .parse::<usize>()
        .unwrap_or_else(|e| panic!("TOKIO_DPDK_RX_DRAIN_BATCH_CAP parse failed value={} error={}", value, e));
    if parsed == 0 {
        panic!("TOKIO_DPDK_RX_DRAIN_BATCH_CAP must be greater than zero");
    }
    parsed
}

// =============================================================================
// DPDK wrapper functions
// =============================================================================

mod dpdk_wrappers {
    use super::ffi;

    /// Receive packets from DPDK port.
    #[inline(always)]
    pub(crate) unsafe fn rx_burst(
        port_id: u16,
        queue_id: u16,
        rx_pkts: *mut *mut ffi::rte_mbuf,
        nb_pkts: u16,
    ) -> u16 {
        // SAFETY: Caller guarantees valid port/queue and buffer
        unsafe { ffi::dpdk_wrap_rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts) }
    }

    /// Transmit packets to DPDK port.
    #[inline(always)]
    pub(crate) unsafe fn tx_burst(
        port_id: u16,
        queue_id: u16,
        tx_pkts: *mut *mut ffi::rte_mbuf,
        nb_pkts: u16,
    ) -> u16 {
        // SAFETY: Caller guarantees valid port/queue and buffer
        unsafe { ffi::dpdk_wrap_rte_eth_tx_burst(port_id, queue_id, tx_pkts, nb_pkts) }
    }

    /// Allocate an mbuf from pool.
    #[inline(always)]
    pub(crate) unsafe fn pktmbuf_alloc(pool: *mut ffi::rte_mempool) -> *mut ffi::rte_mbuf {
        // SAFETY: Caller guarantees valid mempool pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_alloc(pool) }
    }

    /// Free an mbuf.
    #[inline(always)]
    pub(crate) unsafe fn pktmbuf_free(mbuf: *mut ffi::rte_mbuf) {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_free(mbuf) }
    }

    /// Get mbuf data pointer.
    #[inline(always)]
    pub(crate) unsafe fn pktmbuf_mtod(mbuf: *mut ffi::rte_mbuf) -> *mut u8 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_mtod(mbuf) as *mut u8 }
    }

    /// Get mbuf data length (per-segment, for chained mbufs).
    /// Required for jumbo frame and scatter-gather I/O support.
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) unsafe fn pktmbuf_data_len(mbuf: *const ffi::rte_mbuf) -> u16 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_data_len(mbuf) }
    }

    /// Get mbuf packet length.
    #[inline(always)]
    pub(crate) unsafe fn pktmbuf_pkt_len(mbuf: *const ffi::rte_mbuf) -> u32 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_pkt_len(mbuf) }
    }

    /// Append data to mbuf and return pointer to the appended area.
    /// This properly sets pkt_len and data_len on the mbuf.
    #[inline(always)]
    pub(crate) unsafe fn pktmbuf_append(mbuf: *mut ffi::rte_mbuf, len: u16) -> *mut u8 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_append(mbuf, len) as *mut u8 }
    }
}

// =============================================================================
// DPDK Device
// =============================================================================

/// DPDK device for network I/O.
///
/// Implements `smoltcp::phy::Device` to use DPDK for packet transmission
/// and reception.
pub(crate) struct DpdkDevice {
    /// DPDK port ID
    port_id: u16,
    /// DPDK queue ID
    queue_id: u16,
    /// mbuf memory pool pointer
    mempool: *mut ffi::rte_mempool,
    /// Pending transmit mbufs (buffered for batching)
    tx_buffer: Vec<*mut ffi::rte_mbuf>,
    /// Received mbufs pending processing
    rx_pending: Vec<*mut ffi::rte_mbuf>,
    /// Current index into rx_pending
    rx_index: usize,
    /// Preallocated pointer buffer passed to rte_eth_rx_burst.
    rx_burst_buf: Vec<*mut ffi::rte_mbuf>,
    /// Number of mbuf pointers requested from one rx_burst call.
    rx_burst_size: u16,
    /// Raw mbufs drained during the current driver poll before reverse dispatch.
    rx_drain_batch: Vec<*mut ffi::rte_mbuf>,
}

// DPDK mbufs are thread-safe when used correctly
unsafe impl Send for DpdkDevice {}

impl DpdkDevice {
    /// Create a new DPDK device.
    ///
    /// # Safety
    ///
    /// `mempool` must be a valid DPDK memory pool pointer that outlives
    /// this device.
    pub(crate) unsafe fn new(port_id: u16, queue_id: u16, mempool: *mut ffi::rte_mempool) -> Self {
        let rx_burst_size = configured_rx_burst_size();
        let rx_burst_len = rx_burst_size as usize;
        let rx_drain_batch_cap = configured_rx_drain_batch_cap();
        Self {
            port_id,
            queue_id,
            mempool,
            tx_buffer: Vec::with_capacity(TX_BURST_SIZE as usize),
            rx_pending: Vec::with_capacity(rx_burst_len),
            rx_index: 0,
            rx_burst_buf: vec![ptr::null_mut(); rx_burst_len],
            rx_burst_size,
            rx_drain_batch: Vec::with_capacity(rx_drain_batch_cap.max(rx_burst_len)),
        }
    }

    /// Get the port ID.
    /// Used for statistics collection, link monitoring, and shutdown cleanup.
    pub(crate) fn port_id(&self) -> u16 {
        self.port_id
    }

    /// Get the queue ID.
    /// Used for multi-queue diagnostics and hardware queue identification.
    #[allow(dead_code)]
    pub(crate) fn queue_id(&self) -> u16 {
        self.queue_id
    }

    /// Try to receive packets from DPDK.
    pub(crate) fn drain_rx(&mut self, raw_tail: &mut RawTailTable) -> DrainRxStats {
        // Only receive if we've consumed all pending packets
        if self.rx_index >= self.rx_pending.len() {
            self.rx_pending.clear();
            self.rx_index = 0;
        }
        self.rx_drain_batch.clear();
        let mut stats = DrainRxStats::default();

        loop {
            let n = unsafe {
                dpdk_wrappers::rx_burst(
                    self.port_id,
                    self.queue_id,
                    self.rx_burst_buf.as_mut_ptr(),
                    self.rx_burst_size,
                )
            };

            if n > 0 {
                stats.received += n as usize;
                for mbuf in &self.rx_burst_buf[..n as usize] {
                    self.rx_drain_batch.push(*mbuf);
                }
            }

            if n == 0 {
                break;
            }
        }
        for mbuf in self.rx_drain_batch.iter().rev() {
            if !raw_tail.is_empty() && raw_tail.capture_mbuf(*mbuf) {
                stats.raw_tail_captured += 1;
                continue;
            }
            self.rx_pending.push(*mbuf);
            stats.smoltcp_pending += 1;
        }
        self.rx_drain_batch.clear();
        stats
    }

    /// Flush the transmit buffer.
    ///
    /// This should be called periodically to ensure packets are sent.
    pub(crate) fn flush_tx(&mut self) -> std::io::Result<()> {
        if self.tx_buffer.is_empty() {
            return Ok(());
        }

        let mut sent_count = 0usize;
        let total = self.tx_buffer.len();
        let mut retry_count = 0;
        const MAX_RETRIES: usize = 10;

        // Try to send all buffered packets with retry
        while sent_count < total {
            let n = unsafe {
                dpdk_wrappers::tx_burst(
                    self.port_id,
                    self.queue_id,
                    self.tx_buffer[sent_count..].as_mut_ptr(),
                    (total - sent_count) as u16,
                )
            };

            if n == 0 {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    // Failed to send after max retries, log and free remaining mbufs
                    eprintln!(
                        "[DPDK TX WARNING] Failed to send {} packets after {} retries, dropping",
                        total - sent_count,
                        MAX_RETRIES
                    );
                    for mbuf in &self.tx_buffer[sent_count..] {
                        unsafe {
                            dpdk_wrappers::pktmbuf_free(*mbuf);
                        }
                    }
                    self.tx_buffer.clear();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!(
                            "DPDK TX burst failed to send {} packets after {} retries",
                            total - sent_count,
                            MAX_RETRIES
                        ),
                    ));
                }
                // Brief pause before retry (yield CPU)
                std::hint::spin_loop();
                continue;
            }

            sent_count += n as usize;
            retry_count = 0; // Reset retry count on successful send
        }

        self.tx_buffer.clear();
        Ok(())
    }

    pub(crate) fn drop_unprocessed_rx_pending(&mut self) {
        if self.rx_index < self.rx_pending.len() {
            for mbuf in &self.rx_pending[self.rx_index..] {
                unsafe {
                    dpdk_wrappers::pktmbuf_free(*mbuf);
                }
            }
        }
        self.rx_pending.clear();
        self.rx_index = 0;
    }

    #[inline(always)]
    pub(crate) fn has_unprocessed_rx_pending(&self) -> bool {
        self.rx_index < self.rx_pending.len()
    }

    #[inline(always)]
    pub(crate) unsafe fn free_mbuf(mbuf: *mut ffi::rte_mbuf) {
        unsafe { dpdk_wrappers::pktmbuf_free(mbuf) }
    }

    #[inline(always)]
    pub(crate) unsafe fn mbuf_data_ptr(mbuf: *mut ffi::rte_mbuf) -> *mut u8 {
        unsafe { dpdk_wrappers::pktmbuf_mtod(mbuf) }
    }

    pub(crate) fn send_raw_tcp_ack(&mut self, pkt: &ParsedTcpPacket<'_>, seq: u32, ack: u32) {
        const ACK_PACKET_LEN: usize = 14 + 20 + 20;
        let mbuf = unsafe { dpdk_wrappers::pktmbuf_alloc(self.mempool) };
        if mbuf.is_null() {
            panic!("Failed to allocate mbuf for raw-tail ACK");
        }

        let data = unsafe {
            let ptr = dpdk_wrappers::pktmbuf_append(mbuf, ACK_PACKET_LEN as u16);
            if ptr.is_null() {
                dpdk_wrappers::pktmbuf_free(mbuf);
                panic!("Failed to append raw-tail ACK bytes to mbuf");
            }
            std::slice::from_raw_parts_mut(ptr as *mut u8, ACK_PACKET_LEN)
        };

        data[..6].copy_from_slice(&pkt.eth_src);
        data[6..12].copy_from_slice(&pkt.eth_dst);
        data[12..14].copy_from_slice(&0x0800u16.to_be_bytes());

        let ip = &mut data[14..34];
        ip[0] = 0x45;
        ip[1] = 0;
        ip[2..4].copy_from_slice(&(40u16).to_be_bytes());
        ip[4..6].copy_from_slice(&0u16.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[10..12].copy_from_slice(&0u16.to_be_bytes());
        ip[12..16].copy_from_slice(&pkt.local_ip.octets());
        ip[16..20].copy_from_slice(&pkt.remote_ip.octets());
        let ip_sum = internet_checksum(ip);
        ip[10..12].copy_from_slice(&ip_sum.to_be_bytes());

        let tcp = &mut data[34..54];
        tcp[0..2].copy_from_slice(&pkt.local_port.to_be_bytes());
        tcp[2..4].copy_from_slice(&pkt.remote_port.to_be_bytes());
        tcp[4..8].copy_from_slice(&seq.to_be_bytes());
        tcp[8..12].copy_from_slice(&ack.to_be_bytes());
        tcp[12] = 5u8 << 4;
        tcp[13] = 0x10;
        tcp[14..16].copy_from_slice(&65535u16.to_be_bytes());
        tcp[16..18].copy_from_slice(&0u16.to_be_bytes());
        tcp[18..20].copy_from_slice(&0u16.to_be_bytes());
        let tcp_sum = tcp_checksum(pkt.local_ip.octets(), pkt.remote_ip.octets(), tcp);
        tcp[16..18].copy_from_slice(&tcp_sum.to_be_bytes());

        self.tx_buffer.push(mbuf);
    }
}

fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum = sum.wrapping_add(u16::from_be_bytes([chunk[0], chunk[1]]) as u32);
    }
    if let Some(&byte) = chunks.remainder().first() {
        sum = sum.wrapping_add((byte as u32) << 8);
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp: &[u8]) -> u16 {
    let mut pseudo = [0u8; 12 + 20];
    pseudo[..4].copy_from_slice(&src_ip);
    pseudo[4..8].copy_from_slice(&dst_ip);
    pseudo[8] = 0;
    pseudo[9] = 6;
    pseudo[10..12].copy_from_slice(&(tcp.len() as u16).to_be_bytes());
    pseudo[12..].copy_from_slice(tcp);
    internet_checksum(&pseudo)
}

impl Drop for DpdkDevice {
    fn drop(&mut self) {
        // Free any pending RX mbufs
        for mbuf in &self.rx_pending {
            unsafe {
                dpdk_wrappers::pktmbuf_free(*mbuf);
            }
        }

        // Free any pending TX mbufs
        for mbuf in &self.tx_buffer {
            unsafe {
                dpdk_wrappers::pktmbuf_free(*mbuf);
            }
        }
    }
}

// =============================================================================
// smoltcp Device trait implementation
// =============================================================================

/// Receive token for smoltcp Device trait.
pub(crate) struct DpdkRxToken {
    mbuf: *mut ffi::rte_mbuf,
}

/// Transmit token for smoltcp Device trait.
pub(crate) struct DpdkTxToken<'a> {
    device: &'a mut DpdkDevice,
}

impl smoltcp::phy::RxToken for DpdkRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        // Get mbuf data pointer and length
        let (data_ptr, data_len) = unsafe {
            let ptr = dpdk_wrappers::pktmbuf_mtod(self.mbuf);
            let len = dpdk_wrappers::pktmbuf_pkt_len(self.mbuf) as usize;
            (ptr, len)
        };

        let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

        let result = f(data);

        // Free the mbuf after consumption
        unsafe {
            dpdk_wrappers::pktmbuf_free(self.mbuf);
        }

        result
    }
}

impl<'a> smoltcp::phy::TxToken for DpdkTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Allocate new mbuf
        let mbuf = unsafe { dpdk_wrappers::pktmbuf_alloc(self.device.mempool) };

        if mbuf.is_null() {
            panic!("Failed to allocate mbuf for transmission");
        }

        // Use pktmbuf_append to properly allocate space and set pkt_len/data_len
        // This is the correct DPDK way to prepare an mbuf for transmission
        let data = unsafe {
            let ptr = dpdk_wrappers::pktmbuf_append(mbuf, len as u16);
            if ptr.is_null() {
                dpdk_wrappers::pktmbuf_free(mbuf);
                panic!(
                    "Failed to append {} bytes to mbuf (insufficient space)",
                    len
                );
            }
            std::slice::from_raw_parts_mut(ptr, len)
        };

        let result = f(data);

        // Add to transmit buffer
        self.device.tx_buffer.push(mbuf);

        result
    }
}

impl Device for DpdkDevice {
    type RxToken<'a>
        = DpdkRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = DpdkTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_index < self.rx_pending.len() {
            let mbuf = self.rx_pending[self.rx_index];
            self.rx_index += 1;

            Some((DpdkRxToken { mbuf }, DpdkTxToken { device: self }))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(DpdkTxToken { device: self })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = DEFAULT_MTU;
        caps.max_burst_size = Some(self.rx_burst_size as usize);
        caps.medium = Medium::Ethernet;

        // Configure smoltcp to calculate checksums for TX packets in software
        // Checksum::Tx = smoltcp calculates checksums for outgoing packets
        // Checksum::Rx = smoltcp verifies checksums for incoming packets
        // Checksum::Both = smoltcp does both
        // Checksum::None = smoltcp ignores checksums completely (DANGEROUS!)
        caps.checksum = ChecksumCapabilities::default();
        caps.checksum.ipv4 = Checksum::Tx; // smoltcp calculates TX, hardware may strip RX
        caps.checksum.udp = Checksum::Tx; // smoltcp calculates TX
        caps.checksum.tcp = Checksum::Tx; // smoltcp calculates TX

        caps
    }
}

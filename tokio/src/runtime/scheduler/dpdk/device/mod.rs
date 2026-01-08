//! DPDK device implementation for smoltcp integration.
//!
//! This module provides `DpdkDevice` which implements `smoltcp::phy::Device`,
//! allowing DPDK to be used as the network backend for smoltcp's TCP/IP stack.

use std::ptr;

use smoltcp::phy::{Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant as SmolInstant;

use super::ffi;

// =============================================================================
// Configuration constants
// =============================================================================

/// Maximum packets per rx_burst call
const RX_BURST_SIZE: u16 = 32;

/// Maximum packets per tx_burst call
const TX_BURST_SIZE: u16 = 32;

/// Default MTU (Maximum Transmission Unit)
const DEFAULT_MTU: usize = 1500;

// =============================================================================
// DPDK wrapper functions
// =============================================================================

mod dpdk_wrappers {
    use super::ffi;

    /// Receive packets from DPDK port.
    #[inline(always)]
    pub unsafe fn rx_burst(
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
    pub unsafe fn tx_burst(
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
    pub unsafe fn pktmbuf_alloc(pool: *mut ffi::rte_mempool) -> *mut ffi::rte_mbuf {
        // SAFETY: Caller guarantees valid mempool pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_alloc(pool) }
    }

    /// Free an mbuf.
    #[inline(always)]
    pub unsafe fn pktmbuf_free(mbuf: *mut ffi::rte_mbuf) {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_free(mbuf) }
    }

    /// Get mbuf data pointer.
    #[inline(always)]
    pub unsafe fn pktmbuf_mtod(mbuf: *mut ffi::rte_mbuf) -> *mut u8 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_mtod(mbuf) as *mut u8 }
    }

    /// Get mbuf data length.
    #[inline(always)]
    pub unsafe fn pktmbuf_data_len(mbuf: *const ffi::rte_mbuf) -> u16 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_data_len(mbuf) }
    }

    /// Get mbuf packet length.
    #[inline(always)]
    pub unsafe fn pktmbuf_pkt_len(mbuf: *const ffi::rte_mbuf) -> u32 {
        // SAFETY: Caller guarantees valid mbuf pointer
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_pkt_len(mbuf) }
    }
}

// =============================================================================
// DPDK Device
// =============================================================================

/// DPDK device for network I/O.
///
/// Implements `smoltcp::phy::Device` to use DPDK for packet transmission
/// and reception.
pub struct DpdkDevice {
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
    pub unsafe fn new(port_id: u16, queue_id: u16, mempool: *mut ffi::rte_mempool) -> Self {
        Self {
            port_id,
            queue_id,
            mempool,
            tx_buffer: Vec::with_capacity(TX_BURST_SIZE as usize),
            rx_pending: Vec::with_capacity(RX_BURST_SIZE as usize),
            rx_index: 0,
        }
    }

    /// Get the port ID.
    pub fn port_id(&self) -> u16 {
        self.port_id
    }

    /// Get the queue ID.
    pub fn queue_id(&self) -> u16 {
        self.queue_id
    }

    /// Try to receive packets from DPDK.
    fn try_receive(&mut self) {
        // Only receive if we've consumed all pending packets
        if self.rx_index >= self.rx_pending.len() {
            self.rx_pending.clear();
            self.rx_index = 0;

            let mut bufs: [*mut ffi::rte_mbuf; RX_BURST_SIZE as usize] =
                [ptr::null_mut(); RX_BURST_SIZE as usize];

            let n = unsafe {
                dpdk_wrappers::rx_burst(
                    self.port_id,
                    self.queue_id,
                    bufs.as_mut_ptr(),
                    RX_BURST_SIZE,
                )
            };

            if n > 0 {
                self.rx_pending.extend_from_slice(&bufs[0..n as usize]);
            }
        }
    }

    /// Flush the transmit buffer.
    ///
    /// This should be called periodically to ensure packets are sent.
    pub fn flush_tx(&mut self) {
        if self.tx_buffer.is_empty() {
            return;
        }

        let mut sent_count = 0usize;
        let total = self.tx_buffer.len();

        // Try to send all buffered packets
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
                // Failed to send, free remaining mbufs
                for mbuf in &self.tx_buffer[sent_count..] {
                    unsafe {
                        dpdk_wrappers::pktmbuf_free(*mbuf);
                    }
                }
                break;
            }

            sent_count += n as usize;
        }

        self.tx_buffer.clear();
    }
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
pub struct DpdkRxToken {
    mbuf: *mut ffi::rte_mbuf,
}

/// Transmit token for smoltcp Device trait.
pub struct DpdkTxToken<'a> {
    device: &'a mut DpdkDevice,
}

impl smoltcp::phy::RxToken for DpdkRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Get mbuf data pointer and length
        let (data_ptr, data_len) = unsafe {
            let ptr = dpdk_wrappers::pktmbuf_mtod(self.mbuf);
            let len = dpdk_wrappers::pktmbuf_pkt_len(self.mbuf) as usize;
            (ptr, len)
        };

        let data = unsafe { std::slice::from_raw_parts_mut(data_ptr, data_len) };
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

        // Get data pointer and set the packet length
        let data = unsafe {
            let ptr = dpdk_wrappers::pktmbuf_mtod(mbuf);
            // Note: We need to set pkt_len and data_len on the mbuf
            // This assumes the mbuf struct has these accessible fields
            // In real implementation, this would use proper DPDK APIs
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
        self.try_receive();

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
        caps.max_burst_size = Some(RX_BURST_SIZE as usize);
        caps.medium = Medium::Ethernet;

        // Enable hardware checksum offload
        caps.checksum = ChecksumCapabilities::default();
        caps.checksum.ipv4 = Checksum::Both;
        caps.checksum.udp = Checksum::Both;
        caps.checksum.tcp = Checksum::Both;

        caps
    }
}

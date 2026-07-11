//! DPDK device implementation for smoltcp integration.
//!
//! This module provides `DpdkDevice` which implements `smoltcp::phy::Device`,
//! allowing DPDK to be used as the network backend for smoltcp's TCP/IP stack.

use std::{io, ptr, ptr::NonNull};

use smoltcp::phy::{Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant as SmolInstant;

use super::ffi;
use super::raw_tail::{RawTailTable, RAW_TAIL_REQUIRED_RSS_HF};

// =============================================================================
// Configuration constants
// =============================================================================

/// Default packets requested per rx_burst call.
const RX_BURST_SIZE_DEFAULT: u16 = 256;

/// Strict upper bound for application-owned mbufs awaiting one TX burst.
const TX_PENDING_CAP: usize = 32;

/// Default MTU (Maximum Transmission Unit)
const DEFAULT_MTU: usize = 1500;

// DPDK rte_mbuf_core.h: hash.rss is valid only with this RX offload flag.
const RTE_MBUF_F_RX_RSS_HASH: u64 = 1u64 << 1;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DrainRxStats {
    pub(crate) received: usize,
    pub(crate) raw_tail_captured: usize,
    pub(crate) smoltcp_pending: usize,
    #[cfg(feature = "market-trace")]
    pub(crate) trace_start_ns: u64,
    #[cfg(feature = "market-trace")]
    pub(crate) trace_dur_ns: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TxFlushStats {
    pub(crate) total_packets: usize,
    pub(crate) burst_calls: usize,
    pub(crate) zero_retries: usize,
    pub(crate) sent_packets: usize,
    pub(crate) remaining_packets: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeviceErrorCounters {
    pub(crate) tx_mbuf_exhausted: u64,
    pub(crate) tx_pending_full: u64,
    pub(crate) tx_append_failed: u64,
    pub(crate) tx_burst_invalid: u64,
    pub(crate) rx_mbuf_invalid: u64,
}

impl DeviceErrorCounters {
    #[inline(always)]
    pub(crate) fn is_empty(self) -> bool {
        self.tx_mbuf_exhausted == 0
            && self.tx_pending_full == 0
            && self.tx_append_failed == 0
            && self.tx_burst_invalid == 0
            && self.rx_mbuf_invalid == 0
    }
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

    /// Return writable bytes remaining in a fresh mbuf segment.
    #[inline(always)]
    pub(crate) unsafe fn pktmbuf_tailroom(mbuf: *const ffi::rte_mbuf) -> u16 {
        // SAFETY: Caller guarantees a valid mbuf pointer.
        unsafe { ffi::dpdk_wrap_rte_pktmbuf_tailroom(mbuf) }
    }

    pub(crate) unsafe fn rss_hash_conf_get(
        port_id: u16,
        rss_conf: *mut ffi::rte_eth_rss_conf,
    ) -> i32 {
        unsafe { ffi::dpdk_wrap_rte_eth_dev_rss_hash_conf_get(port_id, rss_conf) }
    }
}

fn load_actual_rss_key(port_id: u16) -> io::Result<Box<[u8]>> {
    let mut dev_info: ffi::rte_eth_dev_info = unsafe { std::mem::zeroed() };
    let ret = unsafe { ffi::rte_eth_dev_info_get(port_id, &mut dev_info) };
    if ret != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("rte_eth_dev_info_get({}) failed: {}", port_id, ret),
        ));
    }
    let key_len = dev_info.hash_key_size as usize;
    if key_len < 16 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "DPDK port {} RSS key length {} is shorter than the 16-byte Toeplitz prefix",
                port_id, key_len
            ),
        ));
    }
    let mut key = vec![0u8; key_len];
    let mut rss_conf = ffi::rte_eth_rss_conf {
        rss_key: key.as_mut_ptr(),
        rss_key_len: dev_info.hash_key_size,
        rss_hf: 0,
        algorithm: ffi::rte_eth_hash_function_RTE_ETH_HASH_FUNCTION_DEFAULT,
    };
    let ret = unsafe { dpdk_wrappers::rss_hash_conf_get(port_id, &mut rss_conf) };
    if ret != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("rte_eth_dev_rss_hash_conf_get({}) failed: {}", port_id, ret),
        ));
    }
    if (rss_conf.rss_hf & RAW_TAIL_REQUIRED_RSS_HF) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "DPDK port {} RSS hf {:#x} does not include raw-tail required IPv4 TCP hf {:#x}",
                port_id, rss_conf.rss_hf, RAW_TAIL_REQUIRED_RSS_HF
            ),
        ));
    }
    Ok(key.into_boxed_slice())
}

/// Exclusive application-side ownership of one DPDK mbuf.
///
/// The owner is deliberately retained while a smoltcp token closure executes,
/// so unwinding frees the mbuf instead of leaking it. `into_raw` is used only
/// when ownership moves to the device TX pending vector.
pub(crate) struct OwnedMbuf {
    ptr: NonNull<ffi::rte_mbuf>,
}

impl OwnedMbuf {
    #[inline(always)]
    unsafe fn try_alloc(pool: *mut ffi::rte_mempool) -> Option<Self> {
        NonNull::new(unsafe { dpdk_wrappers::pktmbuf_alloc(pool) }).map(|ptr| Self { ptr })
    }

    #[inline(always)]
    pub(crate) unsafe fn from_received(ptr: *mut ffi::rte_mbuf) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr })
    }

    #[inline(always)]
    pub(crate) fn as_ptr(&self) -> *mut ffi::rte_mbuf {
        self.ptr.as_ptr()
    }

    /// Read only the NIC-provided RSS metadata. The raw-tail drain uses this
    /// before deciding whether this is the one retained packet for a flow.
    #[inline(always)]
    pub(crate) fn rss_hash(&self) -> Option<u32> {
        let mbuf = self.ptr.as_ptr();
        if !rss_hash_metadata_is_valid(unsafe { (*mbuf).ol_flags }) {
            return None;
        }
        Some(unsafe { (*mbuf).__bindgen_anon_2.hash.rss })
    }

    /// Borrow one contiguous received frame for the selected-tail parser.
    /// Superseded mbufs are dropped without calling this method.
    #[inline(always)]
    pub(crate) fn data(&self) -> Option<&[u8]> {
        let mbuf = self.ptr.as_ptr();
        if unsafe { (*mbuf).nb_segs } != 1 {
            return None;
        }
        let ptr = unsafe { dpdk_wrappers::pktmbuf_mtod(mbuf) };
        let len = unsafe { (*mbuf).data_len as usize };
        if ptr.is_null() || len == 0 {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(ptr, len) })
    }

    #[inline(always)]
    fn append(&mut self, len: usize) -> Option<&mut [u8]> {
        let len = u16::try_from(len).ok()?;
        let ptr = unsafe { dpdk_wrappers::pktmbuf_append(self.as_ptr(), len) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts_mut(ptr, len as usize) })
    }

    #[inline(always)]
    pub(crate) fn into_raw(self) -> *mut ffi::rte_mbuf {
        let ptr = self.ptr.as_ptr();
        std::mem::forget(self);
        ptr
    }
}

#[inline(always)]
fn rss_hash_metadata_is_valid(ol_flags: u64) -> bool {
    ol_flags & RTE_MBUF_F_RX_RSS_HASH != 0
}

impl Drop for OwnedMbuf {
    fn drop(&mut self) {
        unsafe { dpdk_wrappers::pktmbuf_free(self.ptr.as_ptr()) };
    }
}

#[inline(always)]
fn retain_unsent_suffix<T: Copy>(pending: &mut Vec<T>, sent: usize) {
    debug_assert!(sent <= pending.len());
    if sent == 0 {
        return;
    }
    if sent == pending.len() {
        pending.clear();
        return;
    }
    pending.copy_within(sent.., 0);
    pending.truncate(pending.len() - sent);
}

#[inline(always)]
fn compact_consumed_prefix<T: Copy>(pending: &mut Vec<T>, consumed: &mut usize) {
    if *consumed >= pending.len() {
        pending.clear();
        *consumed = 0;
        return;
    }
    if *consumed == 0 {
        return;
    }
    let remaining = pending.len() - *consumed;
    pending.copy_within(*consumed.., 0);
    pending.truncate(remaining);
    *consumed = 0;
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
    raw_tail_rss_key: Box<[u8]>,
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
    /// Fixed counters consumed by the driver's rate-limited ERROR reporter.
    error_counters: DeviceErrorCounters,
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
    pub(crate) unsafe fn new(
        port_id: u16,
        queue_id: u16,
        mempool: *mut ffi::rte_mempool,
    ) -> io::Result<Self> {
        let raw_tail_rss_key = load_actual_rss_key(port_id)?;
        let rx_burst_size = configured_rx_burst_size();
        let rx_burst_len = rx_burst_size as usize;

        let tailroom_probe = unsafe { OwnedMbuf::try_alloc(mempool) }.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "DPDK mempool has no mbuf available for startup tailroom validation",
            )
        })?;
        let tailroom = unsafe { dpdk_wrappers::pktmbuf_tailroom(tailroom_probe.as_ptr()) } as usize;
        if tailroom < DEFAULT_MTU {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "DPDK fresh mbuf tailroom {} is smaller than device MTU {}",
                    tailroom, DEFAULT_MTU
                ),
            ));
        }
        drop(tailroom_probe);

        Ok(Self {
            port_id,
            queue_id,
            mempool,
            raw_tail_rss_key,
            tx_buffer: Vec::with_capacity(TX_PENDING_CAP),
            rx_pending: Vec::with_capacity(rx_burst_len),
            rx_index: 0,
            rx_burst_buf: vec![ptr::null_mut(); rx_burst_len],
            rx_burst_size,
            error_counters: DeviceErrorCounters::default(),
        })
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

    pub(crate) fn raw_tail_rss_key(&self) -> &[u8] {
        &self.raw_tail_rss_key
    }

    /// Try to receive packets from DPDK.
    pub(crate) fn drain_rx(&mut self, raw_tail: &mut RawTailTable) -> DrainRxStats {
        // Preserve a suffix which could not be consumed because a paired TX
        // token was unavailable. Compaction is a cold backpressure path and
        // prevents already-consumed pointer history from growing indefinitely.
        compact_consumed_prefix(&mut self.rx_pending, &mut self.rx_index);
        let mut stats = DrainRxStats::default();
        #[cfg(feature = "market-trace")]
        let mut trace_start_ns = 0u64;

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
                #[cfg(feature = "market-trace")]
                if trace_start_ns == 0 {
                    trace_start_ns = crate::runtime::market_trace::now_ns();
                }
                stats.received += n as usize;
                // Classify this NIC burst immediately in its natural order.
                // In particular, a superseded raw-tail mbuf is freed by
                // capture_mbuf before the next burst is requested.
                for &raw_mbuf in &self.rx_burst_buf[..n as usize] {
                    let Some(mbuf) = (unsafe { OwnedMbuf::from_received(raw_mbuf) }) else {
                        self.error_counters.rx_mbuf_invalid = self
                            .error_counters
                            .rx_mbuf_invalid
                            .saturating_add(1);
                        continue;
                    };
                    let mbuf = if raw_tail.is_empty() {
                        mbuf
                    } else {
                        match raw_tail.capture_mbuf(mbuf) {
                            Ok(()) => {
                                stats.raw_tail_captured += 1;
                                continue;
                            }
                            Err(mbuf) => mbuf,
                        }
                    };
                    self.rx_pending.push(mbuf.into_raw());
                    stats.smoltcp_pending += 1;
                }
            }

            if n == 0 {
                break;
            }
        }
        #[cfg(feature = "market-trace")]
        if trace_start_ns != 0 {
            stats.trace_start_ns = trace_start_ns;
            stats.trace_dur_ns = crate::runtime::market_trace::now_ns().saturating_sub(trace_start_ns);
        }
        stats
    }

    /// Flush the transmit buffer.
    ///
    /// This should be called periodically to ensure packets are sent.
    pub(crate) fn flush_tx(&mut self) -> std::io::Result<()> {
        let stats = self.flush_tx_with_stats();
        if stats.remaining_packets == 0 {
            Ok(())
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    pub(crate) fn flush_tx_with_stats(&mut self) -> TxFlushStats {
        let mut stats = TxFlushStats::default();
        if self.tx_buffer.is_empty() {
            return stats;
        }

        let total = self.tx_buffer.len();
        stats.total_packets = total;
        stats.burst_calls = 1;
        // TX_PENDING_CAP is strictly below u16::MAX.
        let sent = unsafe {
            dpdk_wrappers::tx_burst(
                self.port_id,
                self.queue_id,
                self.tx_buffer.as_mut_ptr(),
                total as u16,
            )
        } as usize;
        if sent > total {
            self.error_counters.tx_burst_invalid = self
                .error_counters
                .tx_burst_invalid
                .saturating_add(1);
            stats.remaining_packets = total;
            return stats;
        }
        stats.sent_packets = sent;
        stats.zero_retries = usize::from(sent == 0);
        retain_unsent_suffix(&mut self.tx_buffer, sent);
        stats.remaining_packets = self.tx_buffer.len();
        stats
    }

    #[inline(always)]
    pub(crate) fn has_pending_tx(&self) -> bool {
        !self.tx_buffer.is_empty()
    }

    pub(crate) fn error_counters(&self) -> DeviceErrorCounters {
        self.error_counters
    }

    pub(crate) fn take_error_counters(&mut self) -> DeviceErrorCounters {
        std::mem::take(&mut self.error_counters)
    }

    #[inline(always)]
    pub(crate) fn has_unprocessed_rx_pending(&self) -> bool {
        self.rx_index < self.rx_pending.len()
    }

    #[inline(always)]
    fn try_reserve_tx_mbuf(&mut self) -> Option<OwnedMbuf> {
        if self.tx_buffer.len() >= TX_PENDING_CAP {
            self.error_counters.tx_pending_full = self
                .error_counters
                .tx_pending_full
                .saturating_add(1);
            return None;
        }
        let mbuf = unsafe { OwnedMbuf::try_alloc(self.mempool) };
        if mbuf.is_none() {
            self.error_counters.tx_mbuf_exhausted = self
                .error_counters
                .tx_mbuf_exhausted
                .saturating_add(1);
        }
        mbuf
    }

}

impl Drop for DpdkDevice {
    fn drop(&mut self) {
        // The consumed prefix has already been freed by DpdkRxToken.
        for mbuf in &self.rx_pending[self.rx_index..] {
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
    mbuf: OwnedMbuf,
}

/// Transmit token for smoltcp Device trait.
pub(crate) struct DpdkTxToken<'a> {
    device: &'a mut DpdkDevice,
    mbuf: Option<OwnedMbuf>,
}

impl smoltcp::phy::RxToken for DpdkRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        // Get mbuf data pointer and length
        let (data_ptr, data_len) = unsafe {
            let ptr = dpdk_wrappers::pktmbuf_mtod(self.mbuf.as_ptr());
            let len = dpdk_wrappers::pktmbuf_pkt_len(self.mbuf.as_ptr()) as usize;
            (ptr, len)
        };

        let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

        f(data)
    }
}

impl<'a> smoltcp::phy::TxToken for DpdkTxToken<'a> {
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            len <= DEFAULT_MTU,
            "smoltcp Device capability guarantees TX length does not exceed the validated MTU"
        );
        let data = match self.mbuf.as_mut().and_then(|mbuf| mbuf.append(len)) {
            Some(data) => data,
            None => {
                self.device.error_counters.tx_append_failed = self
                    .device
                    .error_counters
                    .tx_append_failed
                    .saturating_add(1);
                panic!("validated fresh DPDK mbuf tailroom must accept a legal smoltcp frame");
            }
        };

        let result = f(data);

        let mbuf = self
            .mbuf
            .take()
            .expect("DPDK TX token owns exactly one mbuf until successful consume");
        debug_assert!(self.device.tx_buffer.len() < TX_PENDING_CAP);
        self.device.tx_buffer.push(mbuf.into_raw());

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
        let raw_rx = *self.rx_pending.get(self.rx_index)?;
        let tx_mbuf = self.try_reserve_tx_mbuf()?;
        let Some(rx_mbuf) = (unsafe { OwnedMbuf::from_received(raw_rx) }) else {
            self.error_counters.rx_mbuf_invalid = self
                .error_counters
                .rx_mbuf_invalid
                .saturating_add(1);
            return None;
        };
        self.rx_index += 1;
        Some((
            DpdkRxToken { mbuf: rx_mbuf },
            DpdkTxToken {
                device: self,
                mbuf: Some(tx_mbuf),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        let mbuf = self.try_reserve_tx_mbuf()?;
        Some(DpdkTxToken {
            device: self,
            mbuf: Some(mbuf),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_tx_acceptance_preserves_every_pending_pointer() {
        let mut pending = vec![1usize, 2, 3];
        retain_unsent_suffix(&mut pending, 0);
        assert_eq!(pending, vec![1, 2, 3]);
    }

    #[test]
    fn partial_tx_acceptance_retains_fifo_suffix() {
        let mut pending = vec![1usize, 2, 3, 4];
        retain_unsent_suffix(&mut pending, 2);
        assert_eq!(pending, vec![3, 4]);
    }

    #[test]
    fn full_tx_acceptance_forgets_every_nic_owned_pointer() {
        let mut pending = vec![1usize, 2, 3];
        retain_unsent_suffix(&mut pending, 3);
        assert!(pending.is_empty());
    }

    #[test]
    fn rx_backpressure_compaction_preserves_unconsumed_order() {
        let mut pending = vec![10usize, 20, 30, 40];
        let mut consumed = 2;
        compact_consumed_prefix(&mut pending, &mut consumed);
        assert_eq!(pending, vec![30, 40]);
        assert_eq!(consumed, 0);
    }

    #[test]
    fn complete_rx_consumption_clears_pointer_history() {
        let mut pending = vec![10usize, 20];
        let mut consumed = 2;
        compact_consumed_prefix(&mut pending, &mut consumed);
        assert!(pending.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn stale_rss_union_is_ignored_without_the_dpdk_validity_flag() {
        assert!(!rss_hash_metadata_is_valid(0));
        assert!(rss_hash_metadata_is_valid(RTE_MBUF_F_RX_RSS_HASH));
    }
}

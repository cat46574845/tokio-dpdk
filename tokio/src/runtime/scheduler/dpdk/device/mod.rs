//! DPDK device implementation for smoltcp integration.
//!
//! This module provides `DpdkDevice` which implements `smoltcp::phy::Device`,
//! allowing DPDK to be used as the network backend for smoltcp's TCP/IP stack.

use std::{io, ptr, ptr::NonNull};

#[cfg(feature = "dpdk-raw-mbuf-capture")]
use std::fs::{self, File};
#[cfg(feature = "dpdk-raw-mbuf-capture")]
use std::io::{BufWriter, Write};
#[cfg(feature = "dpdk-raw-mbuf-capture")]
use std::path::PathBuf;

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

/// Shared mempool and per-worker pointer capacity for the diagnostic capture.
/// 2^21 - 1 is a DPDK-optimal mempool size and consumes about 4.5 GiB of mbuf
/// storage with the configured 2048-byte data room.
#[cfg(feature = "dpdk-raw-mbuf-capture")]
pub(crate) const RAW_MBUF_CAPTURE_MEMPOOL_SIZE: u32 = 2_097_151;

#[cfg(feature = "dpdk-raw-mbuf-capture")]
const RAW_MBUF_CAPTURE_DIR_ENV: &str = "TOKIO_DPDK_RAW_MBUF_CAPTURE_DIR";

#[cfg(feature = "dpdk-raw-mbuf-capture")]
fn raw_mbuf_capture_output_dir() -> io::Result<PathBuf> {
    std::env::var_os(RAW_MBUF_CAPTURE_DIR_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is required by dpdk-raw-mbuf-capture", RAW_MBUF_CAPTURE_DIR_ENV),
            )
        })
}

#[cfg(feature = "dpdk-raw-mbuf-capture")]
pub(crate) fn preflight_raw_mbuf_capture() -> io::Result<()> {
    let output_dir = raw_mbuf_capture_output_dir()?;
    fs::create_dir_all(&output_dir)?;
    let probe = output_dir.join(".tokio-dpdk-raw-mbuf-capture-preflight");
    File::create(&probe)?;
    fs::remove_file(probe)
}

#[cfg(feature = "dpdk-raw-mbuf-capture")]
struct RawMbufCapture {
    retained: Box<[*mut ffi::rte_mbuf]>,
    retained_len: usize,
    output: PathBuf,
    output_file: File,
    port_id: u16,
    queue_id: u16,
}

#[cfg(feature = "dpdk-raw-mbuf-capture")]
impl RawMbufCapture {
    fn new(port_id: u16, queue_id: u16) -> io::Result<Self> {
        let output_dir = raw_mbuf_capture_output_dir()?;
        let output = output_dir.join(format!("rx-port{}-queue{}.mbuf", port_id, queue_id));
        let output_file = File::create(&output)?;
        let mut retained =
            vec![ptr::null_mut(); RAW_MBUF_CAPTURE_MEMPOOL_SIZE as usize].into_boxed_slice();
        let pointers_per_page = 4096 / std::mem::size_of::<*mut ffi::rte_mbuf>();
        for index in (0..retained.len()).step_by(pointers_per_page) {
            unsafe { ptr::write_volatile(&mut retained[index], ptr::null_mut()) };
        }
        Ok(Self {
            retained,
            retained_len: 0,
            output,
            output_file,
            port_id,
            queue_id,
        })
    }

    #[inline(always)]
    unsafe fn retain(&mut self, mbuf: *mut ffi::rte_mbuf) {
        debug_assert!(self.retained_len < self.retained.len());
        unsafe {
            *self.retained.get_unchecked_mut(self.retained_len) = mbuf;
        }
        self.retained_len += 1;
    }

    fn write_capture(&mut self) -> io::Result<u64> {
        self.output_file.set_len(0)?;
        let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, &mut self.output_file);
        writer.write_all(b"TDPDKRX1")?;
        writer.write_all(&1u32.to_le_bytes())?;
        writer.write_all(&self.port_id.to_le_bytes())?;
        writer.write_all(&self.queue_id.to_le_bytes())?;
        writer.write_all(&(self.retained_len as u64).to_le_bytes())?;

        let mut frame_bytes = 0u64;
        for &head in &self.retained[..self.retained_len] {
            let rss_hash = unsafe { (*head).__bindgen_anon_2.hash.rss };
            let ol_flags = unsafe { (*head).ol_flags };
            let pkt_len = unsafe { (*head).pkt_len };
            let nb_segs = unsafe { (*head).nb_segs };
            let ingress_port = unsafe { (*head).port };
            writer.write_all(&rss_hash.to_le_bytes())?;
            writer.write_all(&ol_flags.to_le_bytes())?;
            writer.write_all(&pkt_len.to_le_bytes())?;
            writer.write_all(&nb_segs.to_le_bytes())?;
            writer.write_all(&ingress_port.to_le_bytes())?;

            let mut segment = head;
            for _ in 0..nb_segs {
                let data_len = unsafe { (*segment).data_len };
                writer.write_all(&data_len.to_le_bytes())?;
                let data = unsafe { dpdk_wrappers::pktmbuf_mtod(segment) };
                let bytes = unsafe { std::slice::from_raw_parts(data, data_len as usize) };
                writer.write_all(bytes)?;
                frame_bytes += u64::from(data_len);
                segment = unsafe { (*segment).next };
            }
        }
        writer.flush()?;
        Ok(frame_bytes)
    }

    fn dump_and_release(&mut self) -> io::Result<(usize, u64)> {
        let packet_count = self.retained_len;
        let write_result = self.write_capture();
        for &mbuf in &self.retained[..self.retained_len] {
            unsafe { dpdk_wrappers::pktmbuf_free(mbuf) };
        }
        self.retained_len = 0;
        write_result.map(|frame_bytes| (packet_count, frame_bytes))
    }
}

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
    dirty: bool,
}

impl DeviceErrorCounters {
    #[inline(always)]
    fn record_tx_mbuf_exhausted(&mut self) {
        self.tx_mbuf_exhausted = self.tx_mbuf_exhausted.saturating_add(1);
        self.dirty = true;
    }

    #[inline(always)]
    fn record_tx_pending_full(&mut self) {
        self.tx_pending_full = self.tx_pending_full.saturating_add(1);
        self.dirty = true;
    }

    #[inline(always)]
    fn record_tx_append_failed(&mut self) {
        self.tx_append_failed = self.tx_append_failed.saturating_add(1);
        self.dirty = true;
    }

    #[inline(always)]
    fn record_tx_burst_invalid(&mut self) {
        self.tx_burst_invalid = self.tx_burst_invalid.saturating_add(1);
        self.dirty = true;
    }

    #[inline(always)]
    fn record_rx_mbuf_invalid(&mut self) {
        self.rx_mbuf_invalid = self.rx_mbuf_invalid.saturating_add(1);
        self.dirty = true;
    }

    #[inline(always)]
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
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
    #[cfg(feature = "dpdk-raw-mbuf-capture")]
    retained_for_capture: bool,
}

struct PreparedTxMbuf {
    mbuf: OwnedMbuf,
    data: NonNull<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TxPrepareFailure {
    Allocation,
    Append,
}

#[inline(always)]
fn prepare_tx_resource<T>(
    resource: Option<T>,
    append: impl FnOnce(&mut T) -> Option<NonNull<u8>>,
) -> Result<(T, NonNull<u8>), TxPrepareFailure> {
    let mut resource = resource.ok_or(TxPrepareFailure::Allocation)?;
    let data = append(&mut resource).ok_or(TxPrepareFailure::Append)?;
    Ok((resource, data))
}

#[inline(always)]
fn set_single_segment_len_fields(data_len: &mut u16, pkt_len: &mut u32, len: usize) {
    *data_len = len as u16;
    *pkt_len = len as u32;
}

impl OwnedMbuf {
    #[inline(always)]
    unsafe fn try_alloc(pool: *mut ffi::rte_mempool) -> Option<Self> {
        NonNull::new(unsafe { dpdk_wrappers::pktmbuf_alloc(pool) }).map(|ptr| Self {
            ptr,
            #[cfg(feature = "dpdk-raw-mbuf-capture")]
            retained_for_capture: false,
        })
    }

    #[inline(always)]
    unsafe fn from_received(
        ptr: *mut ffi::rte_mbuf,
        #[cfg(feature = "dpdk-raw-mbuf-capture")] retained_for_capture: bool,
    ) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            ptr,
            #[cfg(feature = "dpdk-raw-mbuf-capture")]
            retained_for_capture,
        })
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

    /// The token owns a fresh one-segment mbuf pre-appended to DEFAULT_MTU.
    /// Updating these two fields is the single-segment `rte_pktmbuf_trim`
    /// operation without adding another C FFI call to every transmitted frame.
    #[inline(always)]
    fn shrink_preappended_tx_len(&mut self, len: usize) {
        let mbuf = self.as_ptr();
        debug_assert_eq!(unsafe { (*mbuf).nb_segs }, 1);
        debug_assert_eq!(unsafe { (*mbuf).data_len as usize }, DEFAULT_MTU);
        debug_assert_eq!(unsafe { (*mbuf).pkt_len as usize }, DEFAULT_MTU);
        unsafe {
            set_single_segment_len_fields(
                &mut (*mbuf).data_len,
                &mut (*mbuf).pkt_len,
                len,
            );
        }
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
        #[cfg(feature = "dpdk-raw-mbuf-capture")]
        if self.retained_for_capture {
            return;
        }
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
    #[cfg(feature = "dpdk-raw-mbuf-capture")]
    raw_mbuf_capture: Box<RawMbufCapture>,
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
            #[cfg(feature = "dpdk-raw-mbuf-capture")]
            raw_mbuf_capture: Box::new(RawMbufCapture::new(port_id, queue_id)?),
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
                    let Some(mbuf) = (unsafe {
                        OwnedMbuf::from_received(
                            raw_mbuf,
                            #[cfg(feature = "dpdk-raw-mbuf-capture")]
                            true,
                        )
                    }) else {
                        self.error_counters.record_rx_mbuf_invalid();
                        continue;
                    };
                    #[cfg(feature = "dpdk-raw-mbuf-capture")]
                    unsafe {
                        self.raw_mbuf_capture.retain(raw_mbuf);
                    }
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
            self.error_counters.record_tx_burst_invalid();
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

    #[inline(always)]
    pub(crate) fn errors_dirty(&self) -> bool {
        self.error_counters.is_dirty()
    }

    pub(crate) fn take_error_counters(&mut self) -> DeviceErrorCounters {
        std::mem::take(&mut self.error_counters)
    }

    #[inline(always)]
    pub(crate) fn has_unprocessed_rx_pending(&self) -> bool {
        self.rx_index < self.rx_pending.len()
    }

    #[inline(always)]
    pub(crate) fn is_last_unprocessed_rx_pending(&self) -> bool {
        is_last_unprocessed_index(self.rx_index, self.rx_pending.len())
    }

    #[inline(always)]
    fn try_reserve_tx_mbuf(&mut self) -> Option<PreparedTxMbuf> {
        if self.tx_buffer.len() >= TX_PENDING_CAP {
            self.error_counters.record_tx_pending_full();
            return None;
        }
        let prepared = prepare_tx_resource(
            unsafe { OwnedMbuf::try_alloc(self.mempool) },
            |mbuf| {
                mbuf.append(DEFAULT_MTU)
                    .and_then(|data| NonNull::new(data.as_mut_ptr()))
            },
        );
        match prepared {
            Ok((mbuf, data)) => Some(PreparedTxMbuf { mbuf, data }),
            Err(TxPrepareFailure::Allocation) => {
                self.error_counters.record_tx_mbuf_exhausted();
                None
            }
            Err(TxPrepareFailure::Append) => {
                self.error_counters.record_tx_append_failed();
                None
            }
        }
    }

}

#[inline(always)]
fn is_last_unprocessed_index(rx_index: usize, pending_len: usize) -> bool {
    rx_index < pending_len && rx_index + 1 == pending_len
}

impl Drop for DpdkDevice {
    fn drop(&mut self) {
        // The consumed prefix has already been freed by DpdkRxToken.
        #[cfg(not(feature = "dpdk-raw-mbuf-capture"))]
        for mbuf in &self.rx_pending[self.rx_index..] {
            unsafe {
                dpdk_wrappers::pktmbuf_free(*mbuf);
            }
        }
        #[cfg(feature = "dpdk-raw-mbuf-capture")]
        let captured_packets = self.raw_mbuf_capture.retained_len;
        #[cfg(feature = "dpdk-raw-mbuf-capture")]
        match self.raw_mbuf_capture.dump_and_release() {
            Ok((packets, frame_bytes)) => eprintln!(
                "[tokio-dpdk] raw mbuf capture exported path={} packets={} frame_bytes={}",
                self.raw_mbuf_capture.output.display(),
                packets,
                frame_bytes
            ),
            Err(error) => eprintln!(
                "[tokio-dpdk] ERROR raw mbuf capture export failed path={} packets={} error={}",
                self.raw_mbuf_capture.output.display(),
                captured_packets,
                error
            ),
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
    prepared: PreparedTxMbuf,
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
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            len <= DEFAULT_MTU,
            "smoltcp Device capability guarantees TX length does not exceed the validated MTU"
        );
        let DpdkTxToken {
            device,
            mut prepared,
        } = self;
        prepared.mbuf.shrink_preappended_tx_len(len);
        let data = unsafe { std::slice::from_raw_parts_mut(prepared.data.as_ptr(), len) };

        let result = f(data);

        debug_assert!(device.tx_buffer.len() < TX_PENDING_CAP);
        device.tx_buffer.push(prepared.mbuf.into_raw());

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
        let prepared = self.try_reserve_tx_mbuf()?;
        let Some(rx_mbuf) = (unsafe {
            OwnedMbuf::from_received(
                raw_rx,
                #[cfg(feature = "dpdk-raw-mbuf-capture")]
                true,
            )
        }) else {
            self.error_counters.record_rx_mbuf_invalid();
            return None;
        };
        self.rx_index += 1;
        Some((
            DpdkRxToken { mbuf: rx_mbuf },
            DpdkTxToken {
                device: self,
                prepared,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        let prepared = self.try_reserve_tx_mbuf()?;
        Some(DpdkTxToken {
            device: self,
            prepared,
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
    use std::cell::Cell;

    struct DropProbe<'a>(&'a Cell<usize>);

    impl Drop for DropProbe<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn device_error_counter_dirty_flag_tracks_any_typed_error() {
        let mut counters = DeviceErrorCounters::default();
        assert!(!counters.is_dirty());

        counters.record_tx_mbuf_exhausted();
        counters.record_tx_pending_full();
        counters.record_tx_append_failed();
        counters.record_tx_burst_invalid();
        counters.record_rx_mbuf_invalid();

        assert!(counters.is_dirty());
        assert_eq!(counters.tx_mbuf_exhausted, 1);
        assert_eq!(counters.tx_pending_full, 1);
        assert_eq!(counters.tx_append_failed, 1);
        assert_eq!(counters.tx_burst_invalid, 1);
        assert_eq!(counters.rx_mbuf_invalid, 1);
        assert!(!DeviceErrorCounters::default().is_dirty());
    }

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

    #[test]
    fn last_unprocessed_rx_is_an_o1_index_check() {
        assert!(!is_last_unprocessed_index(0, 0));
        assert!(is_last_unprocessed_index(0, 1));
        assert!(!is_last_unprocessed_index(0, 2));
        assert!(is_last_unprocessed_index(1, 2));
        assert!(!is_last_unprocessed_index(2, 2));
    }

    #[test]
    fn tx_allocation_failure_produces_no_prepared_token() {
        let mut append_called = false;
        let prepared = prepare_tx_resource::<usize>(None, |_| {
            append_called = true;
            Some(NonNull::dangling())
        });
        assert_eq!(prepared, Err(TxPrepareFailure::Allocation));
        assert!(!append_called);
        assert!(prepared.ok().is_none());
    }

    #[test]
    fn tx_append_failure_produces_no_token_and_releases_allocation() {
        let drops = Cell::new(0);
        let prepared = prepare_tx_resource(Some(DropProbe(&drops)), |_| None);
        assert!(matches!(prepared, Err(TxPrepareFailure::Append)));
        assert_eq!(drops.get(), 1);
        assert!(prepared.ok().is_none());
    }

    #[test]
    fn preappended_tx_length_is_shrunk_to_the_smoltcp_frame() {
        let mut data_len = DEFAULT_MTU as u16;
        let mut pkt_len = DEFAULT_MTU as u32;
        set_single_segment_len_fields(&mut data_len, &mut pkt_len, 73);
        assert_eq!(data_len, 73);
        assert_eq!(pkt_len, 73);

        set_single_segment_len_fields(&mut data_len, &mut pkt_len, 0);
        assert_eq!(data_len, 0);
        assert_eq!(pkt_len, 0);

        set_single_segment_len_fields(
            &mut data_len,
            &mut pkt_len,
            DEFAULT_MTU,
        );
        assert_eq!(data_len, DEFAULT_MTU as u16);
        assert_eq!(pkt_len, DEFAULT_MTU as u32);
    }
}

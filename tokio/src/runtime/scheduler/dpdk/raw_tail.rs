use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ptr;
use std::task::{Context, Poll, Waker};

use super::device::DpdkDevice;
use super::ffi;
use super::WorkerIdentity;
#[cfg(test)]
use super::DpdkRuntimeId;

const RAW_TAIL_DIRTY_CAP: usize = 8192;
const RAW_TAIL_RSS_MAP_CAP: usize = RAW_TAIL_DIRTY_CAP * 4;
const RAW_TAIL_PENDING_NONE: usize = usize::MAX;
const RAW_TAIL_DATA_CAP: usize = 1500;
const RAW_TAIL_BOUNDARY_WORDS: usize =
    (RAW_TAIL_DATA_CAP + u64::BITS as usize) / u64::BITS as usize;
const TLS_HEADER_LEN: usize = 5;
const TLS_MAX_PLAINTEXT_LEN: usize = 16_640;
const ETHERNET_HEADER_LEN: usize = 14;
const IPV4_MIN_HEADER_LEN: usize = 20;
const TCP_MIN_HEADER_LEN: usize = 20;
const ETHERTYPE_IPV4: u16 = 0x0800;
const IPPROTO_TCP: u8 = 6;
const TCP_FLAG_ACK: u8 = 0x10;
pub(crate) const RAW_TAIL_REQUIRED_RSS_HF: u64 = 1u64 << 4;

#[cfg(feature = "market-trace")]
const DPDK_TRACE_AUX_FIELD_BITS: u64 = 21;
#[cfg(feature = "market-trace")]
const DPDK_TRACE_AUX_FIELD_MASK: u64 = (1u64 << DPDK_TRACE_AUX_FIELD_BITS) - 1;

pub(crate) const RAW_TAIL_RSS_KEY: [u8; 40] = [
    0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2,
    0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3, 0x8f, 0xb0,
    0xd0, 0xca, 0x2b, 0xcb, 0xae, 0x7b, 0x30, 0xb4,
    0x77, 0xcb, 0x2d, 0xa3, 0x80, 0x30, 0xf2, 0x0c,
    0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac, 0x01, 0xfa,
];

#[cfg(feature = "market-trace")]
#[inline(always)]
fn pack_trace_aux3(a: usize, b: usize, c: usize) -> u64 {
    ((a as u64) & DPDK_TRACE_AUX_FIELD_MASK)
        | (((b as u64) & DPDK_TRACE_AUX_FIELD_MASK) << DPDK_TRACE_AUX_FIELD_BITS)
        | (((c as u64) & DPDK_TRACE_AUX_FIELD_MASK) << (DPDK_TRACE_AUX_FIELD_BITS * 2))
}

#[cfg(feature = "market-trace")]
#[inline(always)]
fn complete_raw_tail_record_detail(
    start_ns: u64,
    handle: RawTailHandle,
    header_checks: usize,
    copy_attempts: usize,
    mbufs_popped: usize,
) {
    crate::runtime::market_trace::complete(
        start_ns,
        crate::runtime::market_trace::now_ns().saturating_sub(start_ns),
        crate::runtime::market_trace::SPAN_DPDK_RAW_TAIL_RECORD_DETAIL,
        crate::runtime::market_trace::dpdk_track(handle.worker_index()),
        pack_trace_aux3(header_checks, copy_attempts, mbufs_popped),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Identifies a TCP flow registered in the DPDK raw-tail receiver.
pub struct RawTailHandle {
    id: u64,
    owner: WorkerIdentity,
}

/// Outcome of releasing a raw-tail binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawTailUnregisterStatus {
    /// The live owner driver removed this binding.
    Unregistered,
    /// Runtime shutdown had already reclaimed every binding. The parser may
    /// drop its local state without retaining the handle.
    OwnerReclaimed,
}

impl RawTailHandle {
    pub(crate) fn new(id: u64, owner: WorkerIdentity) -> Self {
        Self { id, owner }
    }

    /// Runtime-local raw-tail flow id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// DPDK worker that owns this handle.
    pub fn worker_index(&self) -> usize {
        self.owner.worker_index
    }

    pub(crate) fn owner(&self) -> WorkerIdentity {
        self.owner
    }

    #[cfg(test)]
    pub(crate) fn runtime_id(&self) -> DpdkRuntimeId {
        self.owner.runtime_id
    }
}

#[derive(Debug, Clone, Copy)]
/// Tail-aligned TLS records copied from one TCP packet by the DPDK driver.
pub struct RawTailRecord<'a> {
    /// Flow handle that produced this record.
    pub handle: RawTailHandle,
    /// NIC RSS hash for the TCP flow.
    pub rss_hash: u32,
    /// TCP sequence number at the start of `bytes`.
    pub tcp_seq: u32,
    /// TCP sequence immediately after `bytes`.
    pub tcp_seq_after_record: u32,
    /// Complete, tail-aligned TLS records from one TCP payload.
    pub bytes: &'a [u8],
    /// Number of complete TLS records contained in `bytes`.
    pub record_count: u16,
    /// Per-flow count of RSS hits at the packet that produced this slot.
    pub packet_ordinal: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct RawTailTuple {
    local_ip: Ipv4Addr,
    remote_ip: Ipv4Addr,
    local_port: u16,
    remote_port: u16,
}

impl RawTailTuple {
    pub(crate) fn from_addrs(local: SocketAddr, remote: SocketAddr) -> io::Result<Self> {
        let (IpAddr::V4(local_ip), IpAddr::V4(remote_ip)) = (local.ip(), remote.ip()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raw-tail currently supports IPv4 flows only",
            ));
        };
        Ok(Self {
            local_ip,
            remote_ip,
            local_port: local.port(),
            remote_port: remote.port(),
        })
    }

    pub(crate) fn rss_hash(&self, rss_key: &[u8]) -> u32 {
        let mut tuple = [0u8; 12];
        tuple[..4].copy_from_slice(&self.remote_ip.octets());
        tuple[4..8].copy_from_slice(&self.local_ip.octets());
        tuple[8..10].copy_from_slice(&self.remote_port.to_be_bytes());
        tuple[10..12].copy_from_slice(&self.local_port.to_be_bytes());
        toeplitz_hash(&tuple, rss_key)
    }
}

struct TailDataSlot {
    bytes: Box<[u8; RAW_TAIL_DATA_CAP]>,
    len: usize,
    tcp_seq: u32,
    rss_hash: u32,
    record_count: u16,
    packet_ordinal: u64,
    ready: bool,
}

impl TailDataSlot {
    fn new() -> Self {
        Self {
            bytes: Box::new([0; RAW_TAIL_DATA_CAP]),
            len: 0,
            tcp_seq: 0,
            rss_hash: 0,
            record_count: 0,
            packet_ordinal: 0,
            ready: false,
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        self.len = 0;
        self.record_count = 0;
        self.ready = false;
    }

    fn publish(
        &mut self,
        bytes: &[u8],
        tcp_seq: u32,
        rss_hash: u32,
        record_count: u16,
        packet_ordinal: u64,
    ) -> Result<(), ()> {
        self.clear();
        if bytes.len() > self.bytes.len() {
            return Err(());
        }
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        self.tcp_seq = tcp_seq;
        self.rss_hash = rss_hash;
        self.record_count = record_count;
        self.packet_ordinal = packet_ordinal;
        self.ready = true;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RssSlotEntry {
    rss: u32,
    slot: usize,
    state: u8,
}

impl RssSlotEntry {
    const EMPTY: Self = Self {
        rss: 0,
        slot: 0,
        state: 0,
    };
    const TOMBSTONE_STATE: u8 = 1;
    const FULL_STATE: u8 = 2;
}

struct RssSlotMap {
    entries: Vec<RssSlotEntry>,
    len: usize,
}

impl RssSlotMap {
    fn new() -> Self {
        if !RAW_TAIL_RSS_MAP_CAP.is_power_of_two() {
            panic!(
                "raw-tail RSS map cap must be power of two cap={}",
                RAW_TAIL_RSS_MAP_CAP
            );
        }
        Self {
            entries: vec![RssSlotEntry::EMPTY; RAW_TAIL_RSS_MAP_CAP],
            len: 0,
        }
    }

    #[inline(always)]
    fn get(&self, rss: u32) -> Option<usize> {
        let mut idx = (rss as usize) & (RAW_TAIL_RSS_MAP_CAP - 1);
        for _ in 0..RAW_TAIL_RSS_MAP_CAP {
            let entry = self.entries[idx];
            match entry.state {
                RssSlotEntry::FULL_STATE if entry.rss == rss => return Some(entry.slot),
                0 => return None,
                _ => {
                    idx = (idx + 1) & (RAW_TAIL_RSS_MAP_CAP - 1);
                }
            }
        }
        None
    }

    fn insert(&mut self, rss: u32, slot: usize) -> Result<Option<usize>, ()> {
        if self.len >= RAW_TAIL_DIRTY_CAP {
            return Err(());
        }
        let mut idx = (rss as usize) & (RAW_TAIL_RSS_MAP_CAP - 1);
        let mut first_tombstone = None;
        for _ in 0..RAW_TAIL_RSS_MAP_CAP {
            let entry = self.entries[idx];
            match entry.state {
                RssSlotEntry::FULL_STATE if entry.rss == rss => return Ok(Some(entry.slot)),
                RssSlotEntry::TOMBSTONE_STATE => {
                    if first_tombstone.is_none() {
                        first_tombstone = Some(idx);
                    }
                }
                0 => {
                    let insert_idx = match first_tombstone {
                        Some(tombstone_idx) => tombstone_idx,
                        None => idx,
                    };
                    self.entries[insert_idx] = RssSlotEntry {
                        rss,
                        slot,
                        state: RssSlotEntry::FULL_STATE,
                    };
                    self.len += 1;
                    return Ok(None);
                }
                _ => {}
            }
            idx = (idx + 1) & (RAW_TAIL_RSS_MAP_CAP - 1);
        }
        let Some(insert_idx) = first_tombstone else {
            return Err(());
        };
        self.entries[insert_idx] = RssSlotEntry {
            rss,
            slot,
            state: RssSlotEntry::FULL_STATE,
        };
        self.len += 1;
        Ok(None)
    }

    fn remove(&mut self, rss: u32) -> Option<usize> {
        let mut idx = (rss as usize) & (RAW_TAIL_RSS_MAP_CAP - 1);
        for _ in 0..RAW_TAIL_RSS_MAP_CAP {
            let entry = self.entries[idx];
            match entry.state {
                RssSlotEntry::FULL_STATE if entry.rss == rss => {
                    self.entries[idx].state = RssSlotEntry::TOMBSTONE_STATE;
                    self.len = self
                        .len
                        .checked_sub(1)
                        .expect("raw-tail RSS map len underflow");
                    return Some(entry.slot);
                }
                0 => return None,
                _ => {
                    idx = (idx + 1) & (RAW_TAIL_RSS_MAP_CAP - 1);
                }
            }
        }
        None
    }

    fn clear(&mut self) {
        self.entries.fill(RssSlotEntry::EMPTY);
        self.len = 0;
    }
}

struct TailConn {
    handle: RawTailHandle,
    rss_hash: u32,
    pending_mbuf: *mut ffi::rte_mbuf,
    data_slot: TailDataSlot,
    receiver_waker: Option<Waker>,
    pending_slot_pos: usize,
    packet_ordinal: u64,
    active: bool,
}

impl TailConn {
    #[inline(always)]
    fn capture_tail_mbuf(&mut self, mbuf: *mut ffi::rte_mbuf) -> *mut ffi::rte_mbuf {
        if let Some(next) = self.packet_ordinal.checked_add(1) {
            self.packet_ordinal = next;
        } else {
            eprintln!(
                "[tokio-dpdk] ERROR raw-tail packet ordinal overflow handle_id={}",
                self.handle.id
            );
        }
        std::mem::replace(&mut self.pending_mbuf, mbuf)
    }
}

impl Drop for TailConn {
    fn drop(&mut self) {
        if !self.pending_mbuf.is_null() {
            unsafe { DpdkDevice::free_mbuf(self.pending_mbuf) };
            self.pending_mbuf = ptr::null_mut();
        }
    }
}

pub(crate) struct RawTailTable {
    next_id: u64,
    owner: WorkerIdentity,
    rss_key: Box<[u8]>,
    conns: Vec<Option<TailConn>>,
    free_slots: Vec<usize>,
    rss_to_slot: RssSlotMap,
    id_to_slot: HashMap<u64, usize>,
    pending_slots: Vec<usize>,
    active_count: usize,
}

unsafe impl Send for RawTailTable {}

impl RawTailTable {
    pub(crate) fn new(owner: WorkerIdentity, rss_key: &[u8]) -> Self {
        if rss_key.is_empty() {
            panic!("raw-tail RSS key must not be empty");
        }
        Self {
            next_id: 1,
            owner,
            rss_key: rss_key.into(),
            conns: Vec::new(),
            free_slots: Vec::new(),
            rss_to_slot: RssSlotMap::new(),
            id_to_slot: HashMap::new(),
            pending_slots: Vec::with_capacity(RAW_TAIL_DIRTY_CAP),
            active_count: 0,
        }
    }

    pub(crate) fn register(&mut self, tuple: RawTailTuple) -> io::Result<RawTailHandle> {
        let rss_hash = tuple.rss_hash(&self.rss_key);
        if let Some(existing_slot) = self.rss_to_slot.get(rss_hash) {
            let Some(existing_id) = self
                .conns
                .get(existing_slot)
                .and_then(|conn| conn.as_ref())
                .map(|conn| conn.handle.id)
            else {
                panic!(
                    "raw-tail RSS map points to empty slot rss={} slot={}",
                    rss_hash, existing_slot
                );
            };
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "raw-tail RSS hash collision rss={} existing_id={}",
                    rss_hash, existing_id
                ),
            ));
        }
        if self.free_slots.is_empty() && self.conns.len() >= RAW_TAIL_DIRTY_CAP {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "raw-tail connection capacity exceeded cap={}",
                    RAW_TAIL_DIRTY_CAP
                ),
            ));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("raw-tail handle id overflow");
        let handle = RawTailHandle::new(id, self.owner);
        let slot = self.free_slots.last().copied().unwrap_or(self.conns.len());
        match self.rss_to_slot.insert(rss_hash, slot) {
            Ok(None) => {}
            Ok(Some(existing_slot)) => {
                panic!(
                    "raw-tail RSS inserted after duplicate check rss={} existing_slot={}",
                    rss_hash, existing_slot
                );
            }
            Err(()) => {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!(
                        "raw-tail RSS map capacity exceeded cap={}",
                        RAW_TAIL_RSS_MAP_CAP
                    ),
                ));
            }
        }
        self.id_to_slot.insert(handle.id, slot);
        let conn = TailConn {
            handle,
            rss_hash,
            pending_mbuf: ptr::null_mut(),
            data_slot: TailDataSlot::new(),
            receiver_waker: None,
            pending_slot_pos: RAW_TAIL_PENDING_NONE,
            packet_ordinal: 0,
            active: false,
        };
        if slot == self.conns.len() {
            self.conns.push(Some(conn));
        } else {
            let reused = self
                .free_slots
                .pop()
                .expect("raw-tail free slot chosen from non-empty free list");
            debug_assert_eq!(reused, slot);
            debug_assert!(self.conns[slot].is_none());
            self.conns[slot] = Some(conn);
        }
        Ok(handle)
    }

    pub(crate) fn activate(&mut self, handle: RawTailHandle) -> io::Result<()> {
        let Some(slot) = self.slot_for_handle(handle) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle not registered on this worker",
            ));
        };
        let was_inactive = {
            let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "raw-tail handle not registered on this worker",
                ));
            };
            let was_inactive = !conn.active;
            conn.active = true;
            was_inactive
        };
        if was_inactive {
            self.active_count += 1;
        }
        Ok(())
    }

    pub(crate) fn unregister(&mut self, handle: RawTailHandle) -> io::Result<()> {
        let Some(idx) = self.slot_for_handle(handle) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle not registered in its owner runtime and worker",
            ));
        };
        {
            let Some(conn) = self.conns[idx].as_ref() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "raw-tail handle index points to an empty connection slot",
                ));
            };
            let rss = conn.rss_hash;
            let was_active = conn.active;
            let removed_slot = self.rss_to_slot.remove(rss);
            if removed_slot != Some(idx) {
                panic!(
                    "raw-tail RSS map remove mismatch rss={} expected_slot={} removed_slot={:?}",
                    rss, idx, removed_slot
                );
            }
            if was_active {
                self.active_count = self
                    .active_count
                    .checked_sub(1)
                    .expect("raw-tail active_count underflow");
            }
            self.id_to_slot.remove(&handle.id);
            self.remove_pending_slot(idx);
            let removed = self.conns[idx].take();
            drop(removed);
            self.free_slots.push(idx);
        }
        Ok(())
    }

    pub(crate) fn reclaim_all(&mut self) {
        self.pending_slots.clear();
        self.id_to_slot.clear();
        self.rss_to_slot.clear();
        self.free_slots.clear();
        self.active_count = 0;
        self.conns.clear();
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.active_count == 0
    }

    pub(crate) fn capture_mbuf(&mut self, mbuf: *mut ffi::rte_mbuf) -> bool {
        if self.active_count == 0 {
            return false;
        }
        let rss = unsafe { mbuf_rss_hash(mbuf) };
        if let Some(slot) = self.rss_to_slot.get(rss) {
            let old_mbuf = {
                let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                    return false;
                };
                if !conn.active {
                    return false;
                }
                conn.capture_tail_mbuf(mbuf)
            };
            if old_mbuf.is_null() {
                self.push_pending_slot(slot);
            } else {
                unsafe { DpdkDevice::free_mbuf(old_mbuf) };
            }
            return true;
        }
        false
    }

    pub(crate) fn finish_drain(&mut self, device: &mut DpdkDevice) {
        let mut pending_slots = std::mem::take(&mut self.pending_slots);
        for slot in pending_slots.iter().copied() {
            let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                continue;
            };
            conn.pending_slot_pos = RAW_TAIL_PENDING_NONE;
            let mbuf = std::mem::replace(&mut conn.pending_mbuf, ptr::null_mut());
            if mbuf.is_null() {
                eprintln!(
                    "[tokio-dpdk] ERROR raw-tail pending slot has no mbuf slot={} handle_id={}",
                    slot,
                    conn.handle.id
                );
                conn.data_slot.clear();
                continue;
            }

            #[cfg(feature = "market-trace")]
            let trace_start_ns = crate::runtime::market_trace::now_ns();
            let handle = conn.handle;
            let rss_hash = conn.rss_hash;
            let packet_ordinal = conn.packet_ordinal;
            #[cfg(feature = "market-trace")]
            let mut header_checks = 0usize;
            let mut copied = false;

            if let Some(pkt) = parse_tcp_packet(mbuf) {
                if pkt.tcp_ack != 0 && !pkt.payload.is_empty() {
                    let ack = tcp_seq_after_payload(pkt.tcp_seq, pkt.payload.len());
                    device.send_raw_tcp_ack(&pkt, pkt.tcp_ack, ack);
                }
                if let Some(records) = find_tail_aligned_tls_records(pkt.payload) {
                    #[cfg(feature = "market-trace")]
                    {
                        header_checks = records.header_checks;
                    }
                    let record_tcp_seq = pkt.tcp_seq.wrapping_add(records.offset as u32);
                    let record_bytes = &pkt.payload[records.offset..];
                    if conn
                        .data_slot
                        .publish(
                            record_bytes,
                            record_tcp_seq,
                            rss_hash,
                            records.record_count,
                            packet_ordinal,
                        )
                        .is_ok()
                    {
                        copied = true;
                    } else {
                        eprintln!(
                            "[tokio-dpdk] ERROR raw-tail TLS tail exceeds fixed slot handle_id={} bytes={} cap={}",
                            handle.id,
                            record_bytes.len(),
                            RAW_TAIL_DATA_CAP
                        );
                    }
                } else {
                    conn.data_slot.clear();
                    if !pkt.payload.is_empty() {
                        eprintln!(
                            "[tokio-dpdk] ERROR raw-tail payload has no tail-aligned TLS records handle_id={} rss={} tcp_seq={} payload_len={} packet_ordinal={}",
                            handle.id,
                            rss_hash,
                            pkt.tcp_seq,
                            pkt.payload.len(),
                            packet_ordinal
                        );
                    }
                }
            } else {
                conn.data_slot.clear();
                eprintln!(
                    "[tokio-dpdk] ERROR raw-tail tail packet parse failed handle_id={} rss={} packet_ordinal={}",
                    handle.id,
                    rss_hash,
                    packet_ordinal
                );
            }

            unsafe { DpdkDevice::free_mbuf(mbuf) };
            if copied {
                if let Some(waker) = conn.receiver_waker.as_ref() {
                    waker.wake_by_ref();
                }
            }
            #[cfg(feature = "market-trace")]
            complete_raw_tail_record_detail(
                trace_start_ns,
                handle,
                header_checks,
                usize::from(copied),
                1,
            );
        }
        pending_slots.clear();
        self.pending_slots = pending_slots;
    }

    pub(crate) fn poll_record<R>(
        &mut self,
        handle: RawTailHandle,
        cx: &mut Context<'_>,
        consume: impl for<'record> FnOnce(RawTailRecord<'record>) -> R,
    ) -> Poll<io::Result<R>> {
        let Some(conn) = self.conn_for_handle_mut(handle) else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle not registered on this worker",
            )));
        };
        match conn.receiver_waker.as_mut() {
            Some(waker) if !waker.will_wake(cx.waker()) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "raw-tail flow already has a different receiver task",
                )));
            }
            Some(_) => {}
            None => conn.receiver_waker = Some(cx.waker().clone()),
        }
        if !conn.data_slot.ready {
            return Poll::Pending;
        }

        let len = conn.data_slot.len;
        let tcp_seq = conn.data_slot.tcp_seq;
        let rss_hash = conn.data_slot.rss_hash;
        let record_count = conn.data_slot.record_count;
        let packet_ordinal = conn.data_slot.packet_ordinal;
        conn.data_slot.ready = false;
        let record = RawTailRecord {
            handle,
            rss_hash,
            tcp_seq,
            tcp_seq_after_record: tcp_seq.wrapping_add(len as u32),
            bytes: &conn.data_slot.bytes[..len],
            record_count,
            packet_ordinal,
        };
        Poll::Ready(Ok(consume(record)))
    }

    fn push_pending_slot(&mut self, slot: usize) {
        if self.pending_slots.len() >= RAW_TAIL_DIRTY_CAP {
            let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                return;
            };
            let mbuf = std::mem::replace(&mut conn.pending_mbuf, ptr::null_mut());
            conn.data_slot.clear();
            if !mbuf.is_null() {
                unsafe { DpdkDevice::free_mbuf(mbuf) };
            }
            eprintln!(
                "[tokio-dpdk] ERROR raw-tail pending queue capacity exceeded cap={} handle_id={}",
                RAW_TAIL_DIRTY_CAP,
                conn.handle.id
            );
            return;
        }
        let pos = self.pending_slots.len();
        let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
            return;
        };
        if conn.pending_slot_pos != RAW_TAIL_PENDING_NONE {
            eprintln!(
                "[tokio-dpdk] ERROR raw-tail pending slot inserted twice slot={} pos={}",
                slot, conn.pending_slot_pos
            );
            return;
        }
        conn.pending_slot_pos = pos;
        self.pending_slots.push(slot);
    }

    fn remove_pending_slot(&mut self, slot: usize) {
        let Some(pos) = self
            .conns
            .get(slot)
            .and_then(|conn| conn.as_ref())
            .map(|conn| conn.pending_slot_pos)
        else {
            return;
        };
        if pos == RAW_TAIL_PENDING_NONE {
            return;
        }
        if pos >= self.pending_slots.len() || self.pending_slots[pos] != slot {
            panic!(
                "raw-tail pending position mismatch slot={} pos={} len={}",
                slot,
                pos,
                self.pending_slots.len()
            );
        }
        let moved = self.pending_slots.swap_remove(pos);
        debug_assert_eq!(moved, slot);
        if pos < self.pending_slots.len() {
            let moved_slot = self.pending_slots[pos];
            let Some(moved_conn) = self.conns.get_mut(moved_slot).and_then(|conn| conn.as_mut()) else {
                panic!("raw-tail moved pending slot points to empty conn slot={}", moved_slot);
            };
            moved_conn.pending_slot_pos = pos;
        }
        if let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) {
            conn.pending_slot_pos = RAW_TAIL_PENDING_NONE;
        }
    }

    fn conn_for_handle_mut(&mut self, handle: RawTailHandle) -> Option<&mut TailConn> {
        let slot = self.slot_for_handle(handle)?;
        self.conns.get_mut(slot)?.as_mut()
    }

    fn slot_for_handle(&self, handle: RawTailHandle) -> Option<usize> {
        if handle.owner != self.owner {
            return None;
        }
        self.id_to_slot.get(&handle.id).copied()
    }
}

pub(crate) struct ParsedTcpPacket<'a> {
    pub eth_src: [u8; 6],
    pub eth_dst: [u8; 6],
    pub local_ip: Ipv4Addr,
    pub remote_ip: Ipv4Addr,
    pub local_port: u16,
    pub remote_port: u16,
    pub tcp_seq: u32,
    pub tcp_ack: u32,
    pub payload: &'a [u8],
}

pub(crate) fn parse_tcp_packet(mbuf: *mut ffi::rte_mbuf) -> Option<ParsedTcpPacket<'static>> {
    let data = unsafe { mbuf_data(mbuf) }?;
    if data.len() < ETHERNET_HEADER_LEN + IPV4_MIN_HEADER_LEN + TCP_MIN_HEADER_LEN {
        return None;
    }
    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    if ethertype != ETHERTYPE_IPV4 {
        return None;
    }
    let mut eth_dst = [0u8; 6];
    let mut eth_src = [0u8; 6];
    eth_dst.copy_from_slice(&data[..6]);
    eth_src.copy_from_slice(&data[6..12]);

    let ip = &data[ETHERNET_HEADER_LEN..];
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    if ihl < IPV4_MIN_HEADER_LEN || ip.len() < ihl + TCP_MIN_HEADER_LEN {
        return None;
    }
    if ip[9] != IPPROTO_TCP {
        return None;
    }
    let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if total_len < ihl + TCP_MIN_HEADER_LEN || ip.len() < total_len {
        return None;
    }
    let remote_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let local_ip = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    let tcp = &ip[ihl..total_len];
    let data_offset = ((tcp[12] >> 4) as usize) * 4;
    if data_offset < TCP_MIN_HEADER_LEN || tcp.len() < data_offset {
        return None;
    }
    let remote_port = u16::from_be_bytes([tcp[0], tcp[1]]);
    let local_port = u16::from_be_bytes([tcp[2], tcp[3]]);
    let tcp_seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
    let tcp_ack = if tcp[13] & TCP_FLAG_ACK != 0 {
        u32::from_be_bytes([tcp[8], tcp[9], tcp[10], tcp[11]])
    } else {
        0
    };
    Some(ParsedTcpPacket {
        eth_src,
        eth_dst,
        local_ip,
        remote_ip,
        local_port,
        remote_port,
        tcp_seq,
        tcp_ack,
        payload: &tcp[data_offset..],
    })
}

struct TailAlignedTlsRecords {
    offset: usize,
    record_count: u16,
    #[cfg(feature = "market-trace")]
    header_checks: usize,
}

fn toeplitz_hash(input: &[u8], key: &[u8]) -> u32 {
    let required_key_bits = input.len() * 8 + 32;
    if key.len() * 8 < required_key_bits {
        panic!(
            "RSS key too short: key_bits={} required_bits={}",
            key.len() * 8,
            required_key_bits
        );
    }
    let mut hash = 0u32;
    for bit_idx in 0..input.len() * 8 {
        let input_byte = input[bit_idx / 8];
        let input_mask = 0x80u8 >> (bit_idx % 8);
        if input_byte & input_mask == 0 {
            continue;
        }
        let mut key_window = 0u32;
        for key_bit in 0..32 {
            let idx = bit_idx + key_bit;
            let key_byte = key[idx / 8];
            let key_mask = 0x80u8 >> (idx % 8);
            if key_byte & key_mask != 0 {
                key_window |= 1u32 << (31 - key_bit);
            }
        }
        hash ^= key_window;
    }
    hash
}

#[inline(always)]
fn tcp_seq_after_payload(seq: u32, len: usize) -> u32 {
    seq.wrapping_add(len as u32)
}

#[inline(always)]
fn looks_like_tls_header(buf: &[u8]) -> bool {
    if buf.len() < TLS_HEADER_LEN {
        return false;
    }
    if buf[0] != 0x17 || buf[1] != 0x03 || buf[2] != 0x03 {
        return false;
    }
    let len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    len > 0 && len <= TLS_MAX_PLAINTEXT_LEN
}

fn tls_record_len_at(payload: &[u8], offset: usize) -> Option<usize> {
    let header_end = offset.checked_add(TLS_HEADER_LEN)?;
    if header_end > payload.len() {
        return None;
    }
    let header = &payload[offset..header_end];
    if !looks_like_tls_header(header) {
        return None;
    }
    TLS_HEADER_LEN.checked_add(u16::from_be_bytes([header[3], header[4]]) as usize)
}

fn find_tail_aligned_tls_records(payload: &[u8]) -> Option<TailAlignedTlsRecords> {
    if payload.len() < TLS_HEADER_LEN || payload.len() > RAW_TAIL_DATA_CAP {
        return None;
    }
    let len = payload.len();
    let mut reaches_tail = [0u64; RAW_TAIL_BOUNDARY_WORDS];
    reaches_tail[len / u64::BITS as usize] |= 1u64 << (len % u64::BITS as usize);
    let mut earliest = None;
    #[cfg(feature = "market-trace")]
    let mut header_checks = 0usize;
    for offset in (0..=len - TLS_HEADER_LEN).rev() {
        #[cfg(feature = "market-trace")]
        {
            header_checks += 1;
        }
        let Some(record_len) = tls_record_len_at(payload, offset) else {
            continue;
        };
        let Some(end) = offset.checked_add(record_len) else {
            continue;
        };
        if end > len {
            continue;
        }
        if reaches_tail[end / u64::BITS as usize] & (1u64 << (end % u64::BITS as usize)) == 0 {
            continue;
        }
        reaches_tail[offset / u64::BITS as usize] |=
            1u64 << (offset % u64::BITS as usize);
        earliest = Some(offset);
    }

    let offset = earliest?;
    let mut cursor = offset;
    let mut record_count = 0u16;
    while cursor < len {
        #[cfg(feature = "market-trace")]
        {
            header_checks += 1;
        }
        let record_len = tls_record_len_at(payload, cursor)?;
        cursor = cursor.checked_add(record_len)?;
        if cursor > len {
            return None;
        }
        record_count = record_count.checked_add(1)?;
    }
    if cursor != len {
        return None;
    }

    Some(TailAlignedTlsRecords {
        offset,
        record_count,
        #[cfg(feature = "market-trace")]
        header_checks,
    })
}

unsafe fn mbuf_data(mbuf: *mut ffi::rte_mbuf) -> Option<&'static [u8]> {
    if mbuf.is_null() {
        return None;
    }
    if unsafe { (*mbuf).nb_segs } != 1 {
        return None;
    }
    let ptr = unsafe { DpdkDevice::mbuf_data_ptr(mbuf) };
    let len = unsafe { (*mbuf).data_len as usize };
    if ptr.is_null() || len == 0 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

unsafe fn mbuf_rss_hash(mbuf: *mut ffi::rte_mbuf) -> u32 {
    unsafe { (*mbuf).__bindgen_anon_2.hash.rss }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::Wake;

    fn fake_mbuf(id: usize) -> *mut ffi::rte_mbuf {
        id as *mut ffi::rte_mbuf
    }

    fn tls_record(body: &[u8]) -> Vec<u8> {
        let mut record = vec![0x17, 0x03, 0x03];
        record.extend_from_slice(&(body.len() as u16).to_be_bytes());
        record.extend_from_slice(body);
        record
    }

    fn tuple(remote_port: u16) -> RawTailTuple {
        RawTailTuple::from_addrs(
            "10.0.0.2:40000".parse().expect("test local address must parse"),
            format!("10.0.0.3:{}", remote_port)
                .parse()
                .expect("test remote address must parse"),
        )
        .expect("test tuple must be IPv4")
    }

    fn test_owner(worker_index: usize) -> WorkerIdentity {
        WorkerIdentity::new(
            DpdkRuntimeId::allocate().expect("test runtime id must allocate"),
            worker_index,
        )
    }

    fn test_conn() -> TailConn {
        TailConn {
            handle: RawTailHandle::new(1, test_owner(0)),
            rss_hash: 7,
            pending_mbuf: ptr::null_mut(),
            data_slot: TailDataSlot::new(),
            receiver_waker: None,
            pending_slot_pos: RAW_TAIL_PENDING_NONE,
            packet_ordinal: 0,
            active: true,
        }
    }

    struct TestWake;

    impl Wake for TestWake {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn capture_keeps_only_latest_mbuf_and_counts_every_rss_hit() {
        let mut conn = test_conn();
        assert!(conn.capture_tail_mbuf(fake_mbuf(1)).is_null());
        assert_eq!(conn.capture_tail_mbuf(fake_mbuf(2)), fake_mbuf(1));
        assert_eq!(conn.capture_tail_mbuf(fake_mbuf(3)), fake_mbuf(2));
        assert_eq!(conn.pending_mbuf, fake_mbuf(3));
        assert_eq!(conn.packet_ordinal, 3);
        conn.pending_mbuf = ptr::null_mut();
    }

    #[test]
    fn finds_all_tail_aligned_tls_records_inside_one_payload() {
        let first = tls_record(b"first");
        let second = tls_record(b"second");
        let mut payload = vec![0xaa, 0xbb, 0xcc];
        payload.extend_from_slice(&first);
        payload.extend_from_slice(&second);

        let records = find_tail_aligned_tls_records(&payload)
            .expect("tail-aligned TLS chain must be found");
        assert_eq!(records.offset, 3);
        assert_eq!(records.record_count, 2);
        assert_eq!(&payload[records.offset..], [first, second].concat());
    }

    #[test]
    fn rejects_tls_record_split_across_tcp_payloads() {
        let payload = [0x17, 0x03, 0x03, 0x00, 0x08, 1, 2, 3];
        assert!(find_tail_aligned_tls_records(&payload).is_none());
    }

    #[test]
    fn failed_publish_clears_previous_single_slot_value() {
        let mut slot = TailDataSlot::new();
        slot.publish(b"old", 10, 20, 1, 30)
            .expect("small test value must fit");
        assert!(slot.ready);

        let oversized = vec![0u8; RAW_TAIL_DATA_CAP + 1];
        assert!(slot.publish(&oversized, 11, 21, 1, 31).is_err());
        assert!(!slot.ready);
        assert_eq!(slot.len, 0);
        assert_eq!(slot.record_count, 0);
    }

    #[test]
    fn unregister_reuses_connection_slot_with_fresh_receiver_state() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let first = table.register(tuple(443)).expect("first registration must succeed");
        let first_slot = table.slot_for_handle(first).expect("first slot must exist");
        table.unregister(first).expect("registered handle must unregister");

        let second = table.register(tuple(444)).expect("second registration must succeed");
        let second_slot = table.slot_for_handle(second).expect("second slot must exist");
        assert_eq!(second_slot, first_slot);
        assert_eq!(table.conns.len(), 1);
        let conn = table.conns[second_slot].as_ref().expect("reused slot must be occupied");
        assert!(conn.receiver_waker.is_none());
        assert_eq!(conn.packet_ordinal, 0);
    }

    #[test]
    fn runtime_local_handle_cannot_alias_same_id_on_another_runtime() {
        let mut first = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let mut second = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let first_handle = first
            .register(tuple(443))
            .expect("first runtime registration must succeed");
        let second_handle = second
            .register(tuple(443))
            .expect("second runtime registration must succeed");
        assert_eq!(first_handle.id(), second_handle.id());
        assert_eq!(first_handle.worker_index(), second_handle.worker_index());
        assert_ne!(first_handle.runtime_id(), second_handle.runtime_id());

        let activate = second
            .activate(first_handle)
            .expect_err("foreign runtime handle must not activate local flow");
        assert_eq!(activate.kind(), io::ErrorKind::NotFound);
        let unregister = second
            .unregister(first_handle)
            .expect_err("foreign runtime handle must not unregister local flow");
        assert_eq!(unregister.kind(), io::ErrorKind::NotFound);
        assert!(second.slot_for_handle(second_handle).is_some());
    }

    #[test]
    fn shutdown_reclaims_every_raw_tail_binding() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let first = table
            .register(tuple(443))
            .expect("first binding must register");
        let second = table
            .register(tuple(444))
            .expect("second binding must register");
        table.activate(first).expect("first binding must activate");
        table.activate(second).expect("second binding must activate");

        table.reclaim_all();

        assert!(table.is_empty());
        assert!(table.conns.is_empty());
        assert!(table.id_to_slot.is_empty());
        assert!(table.pending_slots.is_empty());
        assert_eq!(table.rss_to_slot.len, 0);
        assert_eq!(
            table.unregister(first).expect_err("reclaimed binding must be absent").kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn receiver_claim_survives_recreated_poll_and_rejects_other_task() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("registration must succeed");
        let owner_waker = Waker::from(Arc::new(TestWake));
        let mut owner_cx = Context::from_waker(&owner_waker);
        let first = table.poll_record(handle, &mut owner_cx, |_| ());
        assert!(first.is_pending());

        let recreated = table.poll_record(handle, &mut owner_cx, |_| ());
        assert!(recreated.is_pending());

        let other_waker = Waker::from(Arc::new(TestWake));
        let mut other_cx = Context::from_waker(&other_waker);
        let other = table.poll_record(handle, &mut other_cx, |_| ());
        let Poll::Ready(Err(error)) = other else {
            panic!("different receiver task must be rejected");
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn consume_result_is_preserved_and_ready_is_cleared_before_callback() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("registration must succeed");
        let slot = table.slot_for_handle(handle).expect("slot must exist");
        table.conns[slot]
            .as_mut()
            .expect("connection must exist")
            .data_slot
            .publish(b"record", 100, 200, 1, 3)
            .expect("test value must fit");
        let waker = Waker::from(Arc::new(TestWake));
        let mut cx = Context::from_waker(&waker);
        let result = table.poll_record(handle, &mut cx, |record| {
            assert_eq!(record.bytes, b"record");
            Err::<(), &'static str>("consumer error")
        });
        let Poll::Ready(Ok(Err(error))) = result else {
            panic!("consumer result must be returned unchanged");
        };
        assert_eq!(error, "consumer error");
        assert!(!table.conns[slot]
            .as_ref()
            .expect("connection must still exist")
            .data_slot
            .ready);
    }

    #[test]
    fn repeated_publish_exposes_only_latest_single_slot_value() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("registration must succeed");
        let slot = table.slot_for_handle(handle).expect("slot must exist");
        let data_slot = &mut table.conns[slot]
            .as_mut()
            .expect("connection must exist")
            .data_slot;
        data_slot
            .publish(b"older", 10, 20, 1, 1)
            .expect("older test value must fit");
        data_slot
            .publish(b"latest", 30, 40, 2, 3)
            .expect("latest test value must fit");

        let waker = Waker::from(Arc::new(TestWake));
        let mut cx = Context::from_waker(&waker);
        let result = table.poll_record(handle, &mut cx, |record| {
            (
                record.bytes.to_vec(),
                record.tcp_seq,
                record.rss_hash,
                record.record_count,
                record.packet_ordinal,
            )
        });
        let Poll::Ready(Ok(value)) = result else {
            panic!("latest single-slot value must be ready");
        };
        assert_eq!(value, (b"latest".to_vec(), 30, 40, 2, 3));
        assert!(table.poll_record(handle, &mut cx, |_| ()).is_pending());
    }
}

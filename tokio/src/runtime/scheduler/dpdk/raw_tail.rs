use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ptr;

use super::device::DpdkDevice;
use super::ffi;

const RAW_TAIL_RING_CAP: usize = 256;
const RAW_TAIL_DIRTY_CAP: usize = 8192;
const RAW_TAIL_MAX_TLS_RECORD: usize = 16_640 + 5;
const ETHERNET_HEADER_LEN: usize = 14;
const IPV4_MIN_HEADER_LEN: usize = 20;
const TCP_MIN_HEADER_LEN: usize = 20;
const ETHERTYPE_IPV4: u16 = 0x0800;
const IPPROTO_TCP: u8 = 6;
const TCP_FLAG_ACK: u8 = 0x10;

pub(crate) const RAW_TAIL_RSS_KEY: [u8; 40] = [
    0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2,
    0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3, 0x8f, 0xb0,
    0xd0, 0xca, 0x2b, 0xcb, 0xae, 0x7b, 0x30, 0xb4,
    0x77, 0xcb, 0x2d, 0xa3, 0x80, 0x30, 0xf2, 0x0c,
    0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac, 0x01, 0xfa,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Identifies a TCP flow registered in the DPDK raw-tail receiver.
pub struct RawTailHandle {
    id: u64,
    worker_index: usize,
}

impl RawTailHandle {
    pub(crate) fn new(id: u64, worker_index: usize) -> Self {
        Self { id, worker_index }
    }

    /// Runtime-local raw-tail flow id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// DPDK worker that owns this handle.
    pub fn worker_index(&self) -> usize {
        self.worker_index
    }
}

#[derive(Debug, Clone, Copy)]
/// One candidate TLS record copied from retained DPDK mbufs.
pub struct RawTailRecord<'a> {
    /// Flow handle that produced this record.
    pub handle: RawTailHandle,
    /// NIC RSS hash for the TCP flow.
    pub rss_hash: u32,
    /// TCP sequence number at the start of `bytes`.
    pub tcp_seq: u32,
    /// TCP sequence immediately after `bytes`.
    pub tcp_seq_after_record: u32,
    /// Complete TLS record including the 5-byte TLS header.
    pub bytes: &'a [u8],
    /// Per-flow generation incremented once for each DPDK poll yield pass.
    pub poll_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Result returned by a raw-tail poll-time callback.
pub enum RawTailCallbackResult {
    /// A market-data frame was produced for this flow during this driver poll.
    Accepted,
    /// The TLS record decrypted, but the websocket frame was incomplete; try older records.
    NeedPrevious,
    /// This TLS candidate did not authenticate or did not belong to usable data.
    Rejected,
}

/// Function invoked inside one DPDK driver poll for each raw-tail TLS candidate.
pub type RawTailCallback = unsafe fn(*mut (), RawTailRecord<'_>) -> RawTailCallbackResult;

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

    pub(crate) fn rss_hash(&self) -> u32 {
        let mut tuple = [0u8; 12];
        tuple[..4].copy_from_slice(&self.remote_ip.octets());
        tuple[4..8].copy_from_slice(&self.local_ip.octets());
        tuple[8..10].copy_from_slice(&self.remote_port.to_be_bytes());
        tuple[10..12].copy_from_slice(&self.local_port.to_be_bytes());
        toeplitz_hash(&tuple, &RAW_TAIL_RSS_KEY)
    }

}

struct TailRing {
    slots: [*mut ffi::rte_mbuf; RAW_TAIL_RING_CAP],
    head: usize,
    len: usize,
}

impl TailRing {
    fn new() -> Self {
        Self {
            slots: [ptr::null_mut(); RAW_TAIL_RING_CAP],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, mbuf: *mut ffi::rte_mbuf) -> Option<*mut ffi::rte_mbuf> {
        if self.len == RAW_TAIL_RING_CAP {
            Some(mbuf)
        } else {
            let idx = (self.head + self.len) % RAW_TAIL_RING_CAP;
            self.slots[idx] = mbuf;
            self.len += 1;
            None
        }
    }

    fn newest_to_oldest(&self) -> TailRingRevIter<'_> {
        TailRingRevIter {
            ring: self,
            remaining: self.len,
        }
    }

    fn drain_free(&mut self) {
        for mbuf in self.newest_to_oldest() {
            unsafe { DpdkDevice::free_mbuf(mbuf) };
        }
        self.slots = [ptr::null_mut(); RAW_TAIL_RING_CAP];
        self.head = 0;
        self.len = 0;
    }
}

struct TailRingRevIter<'a> {
    ring: &'a TailRing,
    remaining: usize,
}

impl Iterator for TailRingRevIter<'_> {
    type Item = *mut ffi::rte_mbuf;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let emitted = self.ring.len - self.remaining;
        self.remaining -= 1;
        let idx = (self.ring.head + emitted) % RAW_TAIL_RING_CAP;
        Some(self.ring.slots[idx])
    }
}

struct TailConn {
    handle: RawTailHandle,
    rss_hash: Option<u32>,
    ring: TailRing,
    record_bytes: Vec<u8>,
    segments: Vec<TailSegment>,
    callback: Option<RawTailCallback>,
    callback_ctx: *mut (),
    dirty: bool,
    poll_generation: u64,
    active: bool,
}

impl Drop for TailConn {
    fn drop(&mut self) {
        self.ring.drain_free();
    }
}

pub(crate) struct RawTailTable {
    next_id: u64,
    worker_index: usize,
    conns: Vec<Option<TailConn>>,
    rss_to_slot: HashMap<u32, usize>,
    id_to_slot: HashMap<u64, usize>,
    dirty_slots: Vec<usize>,
    active_count: usize,
}

unsafe impl Send for RawTailTable {}

impl RawTailTable {
    pub(crate) fn new(worker_index: usize) -> Self {
        Self {
            next_id: 1,
            worker_index,
            conns: Vec::new(),
            rss_to_slot: HashMap::new(),
            id_to_slot: HashMap::new(),
            dirty_slots: Vec::with_capacity(RAW_TAIL_DIRTY_CAP),
            active_count: 0,
        }
    }

    pub(crate) fn register(&mut self, tuple: RawTailTuple) -> io::Result<RawTailHandle> {
        let rss_hash = tuple.rss_hash();
        if let Some(existing_slot) = self.rss_to_slot.get(&rss_hash).copied() {
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
        if self.conns.len() >= RAW_TAIL_DIRTY_CAP {
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
        let handle = RawTailHandle::new(id, self.worker_index);
        let slot = self.conns.len();
        self.rss_to_slot.insert(rss_hash, slot);
        self.id_to_slot.insert(handle.id, slot);
        self.conns.push(Some(TailConn {
            handle,
            rss_hash: Some(rss_hash),
            ring: TailRing::new(),
            record_bytes: Vec::with_capacity(RAW_TAIL_MAX_TLS_RECORD),
            segments: Vec::with_capacity(RAW_TAIL_RING_CAP),
            callback: None,
            callback_ctx: ptr::null_mut(),
            dirty: false,
            poll_generation: 0,
            active: false,
        }));
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

    pub(crate) fn set_callback(
        &mut self,
        handle: RawTailHandle,
        ctx: *mut (),
        callback: RawTailCallback,
    ) -> io::Result<()> {
        let Some(conn) = self.conn_for_handle_mut(handle) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle not registered on this worker",
            ));
        };
        conn.callback = Some(callback);
        conn.callback_ctx = ctx;
        Ok(())
    }

    pub(crate) fn unregister(&mut self, handle: RawTailHandle) {
        if let Some(idx) = self.slot_for_handle(handle) {
            let Some(conn) = self.conns[idx].as_ref() else {
                return;
            };
            if let Some(rss) = conn.rss_hash {
                self.rss_to_slot.remove(&rss);
            }
            if conn.active {
                self.active_count = self
                    .active_count
                    .checked_sub(1)
                    .expect("raw-tail active_count underflow");
            }
            self.id_to_slot.remove(&handle.id);
            self.conns[idx] = None;
        }
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
        if let Some(slot) = self.rss_to_slot.get(&rss).copied() {
            let newly_dirty = {
                let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                    return false;
                };
                if !conn.active {
                    return false;
                }
                Self::push_conn_mbuf(conn, mbuf)
            };
            if newly_dirty {
                self.push_dirty_slot(slot);
            }
            return true;
        }
        false
    }

    pub(crate) fn flush_acks(&mut self, device: &mut DpdkDevice) {
        for idx in 0..self.dirty_slots.len() {
            let slot = self.dirty_slots[idx];
            let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                continue;
            };
            let Some(mbuf) = conn.ring.newest_to_oldest().next() else {
                continue;
            };
            let Some(pkt) = parse_tcp_packet(mbuf) else {
                continue;
            };
            if pkt.tcp_ack == 0 {
                continue;
            }
            let ack = tcp_seq_after_payload(pkt.tcp_seq, pkt.payload.len());
            device.send_raw_tcp_ack(&pkt, pkt.tcp_ack, ack);
        }
        device.flush_tx();
    }

    pub(crate) fn yield_dirty_records(&mut self) {
        while let Some(slot) = self.dirty_slots.pop() {
            let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                continue;
            };
            conn.dirty = false;
            Self::yield_latest_tls_records(conn);
            conn.ring.drain_free();
        }
    }

    fn push_conn_mbuf(conn: &mut TailConn, mbuf: *mut ffi::rte_mbuf) -> bool {
        if let Some(old) = conn.ring.push(mbuf) {
            unsafe { DpdkDevice::free_mbuf(old) };
        }
        if !conn.dirty {
            conn.dirty = true;
            return true;
        }
        false
    }

    fn push_dirty_slot(&mut self, slot: usize) {
        if self.dirty_slots.len() >= RAW_TAIL_DIRTY_CAP {
            panic!(
                "raw-tail dirty queue capacity exceeded cap={}",
                RAW_TAIL_DIRTY_CAP
            );
        }
        self.dirty_slots.push(slot);
    }

    fn yield_latest_tls_records(conn: &mut TailConn) {
        let (Some(callback), Some(rss_hash)) = (conn.callback, conn.rss_hash) else {
            return;
        };
        conn.poll_generation = conn.poll_generation.wrapping_add(1);
        let poll_generation = conn.poll_generation;
        conn.segments.clear();
        let mut inferred_payload_offset = None;
        let mut inferred_next_seq = None;
        for mbuf in conn.ring.newest_to_oldest() {
            let Some(segment) = build_tail_segment(mbuf, inferred_payload_offset, inferred_next_seq) else {
                continue;
            };
            inferred_payload_offset = Some(segment.payload_offset);
            inferred_next_seq = Some(segment.tcp_seq);
            if segment.payload.len() < 5 {
                conn.segments.push(segment);
                continue;
            }
            let mut offset = segment.payload.len() - 5;
            loop {
                if looks_like_tls_header(&segment.payload[offset..]) {
                    let record_len =
                        5 + u16::from_be_bytes([segment.payload[offset + 3], segment.payload[offset + 4]]) as usize;
                    let start_seq = segment.tcp_seq.wrapping_add(offset as u32);
                    conn.record_bytes.resize(record_len, 0);
                    conn.segments.push(segment);
                    if copy_tcp_range_from_segments(
                        &conn.segments,
                        start_seq,
                        record_len,
                        &mut conn.record_bytes,
                    ) {
                        let record = RawTailRecord {
                            handle: conn.handle,
                            rss_hash,
                            tcp_seq: start_seq,
                            tcp_seq_after_record: start_seq.wrapping_add(record_len as u32),
                            bytes: &conn.record_bytes[..record_len],
                            poll_generation,
                        };
                        match unsafe { callback(conn.callback_ctx, record) } {
                            RawTailCallbackResult::Accepted => return,
                            RawTailCallbackResult::NeedPrevious | RawTailCallbackResult::Rejected => {}
                        }
                    }
                    conn.segments.pop();
                }
                if offset == 0 {
                    break;
                }
                offset -= 1;
            }
            conn.segments.push(segment);
        }
    }

    fn conn_for_handle_mut(&mut self, handle: RawTailHandle) -> Option<&mut TailConn> {
        let slot = self.slot_for_handle(handle)?;
        self.conns.get_mut(slot)?.as_mut()
    }

    fn slot_for_handle(&self, handle: RawTailHandle) -> Option<usize> {
        if handle.worker_index != self.worker_index {
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
    pub payload_offset: usize,
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
        payload_offset: ETHERNET_HEADER_LEN + ihl + data_offset,
    })
}

#[derive(Clone, Copy)]
struct TailSegment {
    tcp_seq: u32,
    payload_offset: usize,
    payload: &'static [u8],
}

fn build_tail_segment(
    mbuf: *mut ffi::rte_mbuf,
    known_payload_offset: Option<usize>,
    next_seq: Option<u32>,
) -> Option<TailSegment> {
    if let (Some(payload_offset), Some(next_seq)) = (known_payload_offset, next_seq) {
        let data = unsafe { mbuf_data(mbuf) }?;
        if data.len() < payload_offset {
            return None;
        }
        let payload = &data[payload_offset..];
        let tcp_seq = next_seq.wrapping_sub(payload.len() as u32);
        return Some(TailSegment {
            tcp_seq,
            payload_offset,
            payload,
        });
    }

    let pkt = parse_tcp_packet(mbuf)?;
    Some(TailSegment {
        tcp_seq: pkt.tcp_seq,
        payload_offset: pkt.payload_offset,
        payload: pkt.payload,
    })
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
    if buf.len() < 5 {
        return false;
    }
    if buf[0] != 0x17 || buf[1] != 0x03 || buf[2] != 0x03 {
        return false;
    }
    let len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    len > 0 && len <= 16_640
}

fn copy_tcp_range_from_segments(
    segments_newest_to_oldest: &[TailSegment],
    start_seq: u32,
    len: usize,
    out: &mut [u8],
) -> bool {
    if len > out.len() {
        return false;
    }
    let mut copied = 0usize;
    while copied < len {
        let want_seq = start_seq.wrapping_add(copied as u32);
        let mut found = false;
        for segment in segments_newest_to_oldest {
            let seg_end = segment.tcp_seq.wrapping_add(segment.payload.len() as u32);
            if seq_in_range(want_seq, segment.tcp_seq, seg_end) {
                let off = want_seq.wrapping_sub(segment.tcp_seq) as usize;
                let take = (segment.payload.len() - off).min(len - copied);
                out[copied..copied + take].copy_from_slice(&segment.payload[off..off + take]);
                copied += take;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

#[inline(always)]
fn seq_in_range(seq: u32, start: u32, end: u32) -> bool {
    seq.wrapping_sub(start) < end.wrapping_sub(start)
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

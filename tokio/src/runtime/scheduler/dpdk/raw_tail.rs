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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Result of polling one raw-tail flow.
pub enum RawTailPoll {
    /// A candidate record was accepted and older mbufs were released.
    Accepted,
    /// No complete candidate TLS record was available.
    NoRecord,
    /// The handle is not registered on this worker.
    UnknownHandle,
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

    #[inline(always)]
    fn matches(&self, pkt: &ParsedTcpPacket<'_>) -> bool {
        pkt.local_ip == self.local_ip
            && pkt.remote_ip == self.remote_ip
            && pkt.local_port == self.local_port
            && pkt.remote_port == self.remote_port
    }
}

#[derive(Clone, Copy)]
struct Segment {
    seq: u32,
    payload_ptr: *const u8,
    payload_len: usize,
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
            let old = self.slots[self.head];
            self.slots[self.head] = mbuf;
            self.head = (self.head + 1) % RAW_TAIL_RING_CAP;
            Some(old)
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

    fn release_before_tcp_seq(&mut self, seq: u32) {
        let mut kept = [ptr::null_mut(); RAW_TAIL_RING_CAP];
        let mut kept_len = 0usize;
        for mbuf in self.newest_to_oldest() {
            let keep = match parse_tcp_packet(mbuf) {
                Some(pkt) => tcp_seq_after_payload(pkt.tcp_seq, pkt.payload.len()) > seq,
                None => false,
            };
            if keep {
                kept[kept_len] = mbuf;
                kept_len += 1;
            } else {
                unsafe { DpdkDevice::free_mbuf(mbuf) };
            }
        }

        self.slots = [ptr::null_mut(); RAW_TAIL_RING_CAP];
        self.head = 0;
        self.len = 0;
        for idx in (0..kept_len).rev() {
            let _ = self.push(kept[idx]);
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
        self.remaining -= 1;
        let idx = (self.ring.head + self.remaining) % RAW_TAIL_RING_CAP;
        Some(self.ring.slots[idx])
    }
}

struct TailConn {
    handle: RawTailHandle,
    tuple: RawTailTuple,
    rss_hash: Option<u32>,
    ring: TailRing,
    dirty: bool,
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
    rss_to_id: HashMap<u32, u64>,
    dirty_ids: Vec<u64>,
}

unsafe impl Send for RawTailTable {}

impl RawTailTable {
    pub(crate) fn new(worker_index: usize) -> Self {
        Self {
            next_id: 1,
            worker_index,
            conns: Vec::new(),
            rss_to_id: HashMap::new(),
            dirty_ids: Vec::with_capacity(RAW_TAIL_DIRTY_CAP),
        }
    }

    pub(crate) fn register(&mut self, tuple: RawTailTuple) -> RawTailHandle {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("raw-tail handle id overflow");
        let handle = RawTailHandle::new(id, self.worker_index);
        self.conns.push(Some(TailConn {
            handle,
            tuple,
            rss_hash: None,
            ring: TailRing::new(),
            dirty: false,
        }));
        handle
    }

    pub(crate) fn unregister(&mut self, handle: RawTailHandle) {
        if let Some((idx, conn)) = self.find_conn_mut(handle.id) {
            if let Some(rss) = conn.rss_hash {
                self.rss_to_id.remove(&rss);
            }
            self.conns[idx] = None;
        }
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.conns.iter().all(|conn| conn.is_none())
    }

    pub(crate) fn capture_mbuf(&mut self, mbuf: *mut ffi::rte_mbuf) -> bool {
        let rss = unsafe { mbuf_rss_hash(mbuf) };
        if let Some(id) = self.rss_to_id.get(&rss).copied() {
            if let Some((_idx, conn)) = self.find_conn_mut(id) {
                Self::push_conn_mbuf(conn, mbuf);
                return true;
            }
        }

        let Some(pkt) = parse_tcp_packet(mbuf) else {
            return false;
        };
        for conn in self.conns.iter_mut().flatten() {
            if conn.rss_hash.is_none() && conn.tuple.matches(&pkt) {
                conn.rss_hash = Some(rss);
                self.rss_to_id.insert(rss, conn.handle.id);
                Self::push_conn_mbuf(conn, mbuf);
                return true;
            }
        }
        false
    }

    pub(crate) fn flush_acks(&mut self, device: &mut DpdkDevice) {
        let dirty_ids: Vec<u64> = self.dirty_ids.drain(..).collect();
        for id in dirty_ids {
            let Some((_idx, conn)) = self.find_conn_mut(id) else {
                continue;
            };
            conn.dirty = false;
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

    pub(crate) fn poll_tls_records<F>(
        &mut self,
        handle: RawTailHandle,
        scratch: &mut [u8],
        mut f: F,
    ) -> io::Result<RawTailPoll>
    where
        F: FnMut(RawTailRecord<'_>) -> bool,
    {
        if scratch.len() < RAW_TAIL_MAX_TLS_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raw-tail scratch buffer is smaller than one TLS record",
            ));
        }
        let Some((_idx, conn)) = self.find_conn_mut(handle.id) else {
            return Ok(RawTailPoll::UnknownHandle);
        };

        let mut segments = [Segment {
            seq: 0,
            payload_ptr: ptr::null(),
            payload_len: 0,
        }; RAW_TAIL_RING_CAP];
        let mut segment_len = 0usize;
        for mbuf in conn.ring.newest_to_oldest() {
            let Some(pkt) = parse_tcp_packet(mbuf) else {
                continue;
            };
            if pkt.payload.is_empty() {
                continue;
            }
            segments[segment_len] = Segment {
                seq: pkt.tcp_seq,
                payload_ptr: pkt.payload.as_ptr(),
                payload_len: pkt.payload.len(),
            };
            segment_len += 1;
            if segment_len == RAW_TAIL_RING_CAP {
                break;
            }
        }

        let Some(rss_hash) = conn.rss_hash else {
            return Ok(RawTailPoll::NoRecord);
        };
        for seg_idx in 0..segment_len {
            let seg = segments[seg_idx];
            let payload = unsafe { std::slice::from_raw_parts(seg.payload_ptr, seg.payload_len) };
            let mut offset = payload.len().saturating_sub(5);
            loop {
                if looks_like_tls_header(&payload[offset..]) {
                    let record_len = 5 + u16::from_be_bytes([payload[offset + 3], payload[offset + 4]]) as usize;
                    let start_seq = seg.seq.wrapping_add(offset as u32);
                    if copy_tcp_range(&segments[..segment_len], start_seq, record_len, scratch) {
                        let record = RawTailRecord {
                            handle,
                            rss_hash,
                            tcp_seq: start_seq,
                            tcp_seq_after_record: start_seq.wrapping_add(record_len as u32),
                            bytes: &scratch[..record_len],
                        };
                        if f(record) {
                            conn.ring.release_before_tcp_seq(record.tcp_seq_after_record);
                            return Ok(RawTailPoll::Accepted);
                        }
                    }
                }
                if offset == 0 {
                    break;
                }
                offset -= 1;
            }
        }

        Ok(RawTailPoll::NoRecord)
    }

    fn push_conn_mbuf(conn: &mut TailConn, mbuf: *mut ffi::rte_mbuf) {
        if let Some(old) = conn.ring.push(mbuf) {
            unsafe { DpdkDevice::free_mbuf(old) };
        }
        if !conn.dirty {
            conn.dirty = true;
        }
    }

    pub(crate) fn collect_dirty_from_conns(&mut self) {
        self.dirty_ids.clear();
        for conn in self.conns.iter().flatten() {
            if conn.dirty {
                self.dirty_ids.push(conn.handle.id);
            }
        }
    }

    fn find_conn_mut(&mut self, id: u64) -> Option<(usize, &mut TailConn)> {
        for (idx, conn) in self.conns.iter_mut().enumerate() {
            if conn.as_ref().map(|conn| conn.handle.id) == Some(id) {
                return conn.as_mut().map(|conn| (idx, conn));
            }
        }
        None
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

fn copy_tcp_range(segments: &[Segment], start_seq: u32, len: usize, out: &mut [u8]) -> bool {
    if len > out.len() {
        return false;
    }
    let mut copied = 0usize;
    while copied < len {
        let want_seq = start_seq.wrapping_add(copied as u32);
        let mut found = false;
        for seg in segments {
            let seg_end = seg.seq.wrapping_add(seg.payload_len as u32);
            if seq_in_range(want_seq, seg.seq, seg_end) {
                let off = want_seq.wrapping_sub(seg.seq) as usize;
                let take = (seg.payload_len - off).min(len - copied);
                let src = unsafe { std::slice::from_raw_parts(seg.payload_ptr.add(off), take) };
                out[copied..copied + take].copy_from_slice(src);
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

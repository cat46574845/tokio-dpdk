use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ptr::NonNull;
use std::task::{Context, Poll, Waker};

use smoltcp::iface::{GatewayNeighborUpdate, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::storage::LinearBuffer;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, TcpControl, TcpPacket, TcpRepr,
};

use super::device::OwnedMbuf;

pub(crate) const RAW_TAIL_CONNECTION_CAP: usize = super::SOCKET_LIFECYCLE_CAPACITY;
const RAW_TAIL_RSS_MAP_CAP: usize = RAW_TAIL_CONNECTION_CAP * 4;
const TLS_HEADER_LEN: usize = 5;
const TLS_MIN_CIPHERTEXT_LEN: usize = 17;
const TLS_MAX_CIPHERTEXT_LEN: usize = 16_640;
pub(crate) const RAW_TAIL_REQUIRED_RSS_HF: u64 = 1u64 << 4;

pub(crate) const RAW_TAIL_RSS_KEY: [u8; 40] = [
    0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2,
    0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3, 0x8f, 0xb0,
    0xd0, 0xca, 0x2b, 0xcb, 0xae, 0x7b, 0x30, 0xb4,
    0x77, 0xcb, 0x2d, 0xa3, 0x80, 0x30, 0xf2, 0x0c,
    0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac, 0x01, 0xfa,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Identifies one TCP flow registered in a worker-local raw-tail table.
pub struct RawTailHandle {
    id: u64,
    slot: usize,
    worker_index: usize,
}

impl RawTailHandle {
    pub(crate) fn new(id: u64, slot: usize, worker_index: usize) -> Self {
        Self {
            id,
            slot,
            worker_index,
        }
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

/// Borrowed input passed synchronously from the DPDK driver to a parser.
#[derive(Debug, Clone, Copy)]
pub struct RawTailInput<'a> {
    /// TCP sequence number at the first byte of `records`.
    pub tcp_seq: u32,
    /// Complete TLS records borrowed directly from the selected mbuf.
    pub records: &'a [u8],
    /// Wrapping count of RSS-matched packets observed for this flow.
    pub packet_ordinal: u64,
    /// The selected TCP segment carried FIN/RST or closed smoltcp state.
    pub connection_closed: bool,
    /// Exact record ordinal from the activation anchor when forward traversal
    /// observed every intervening header. Reverse selection leaves this empty.
    pub exact_record_ordinal: Option<u64>,
}

/// Packet/TLS framing strategy selected once when a raw-tail connection is activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawTailScanStrategy {
    /// Retain the latest packet and locate its newest complete TLS record from the tail.
    Reverse,
    /// Follow TLS record boundaries in TCP order and avoid touching payload when the
    /// next header cannot be present in the packet.
    ForwardJump,
}

/// Immutable, per-connection framing inputs derived by the application parser.
#[derive(Debug, Clone, Copy)]
pub struct RawTailParserConfig {
    /// Strategy used for this process run.
    pub scan_strategy: RawTailScanStrategy,
    /// Smallest TLS wire record which can carry this connection's application payload.
    pub tls_record_wire_min: u16,
    /// TCP position paired with the parser's current inbound TLS sequence.
    pub tcp_anchor: u32,
}

/// Whether a synchronous parser published a value for its receiver task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawTailParseDisposition {
    /// The parser did not publish an application value.
    Empty,
    /// The parser published an overwriteable application value.
    Ready,
    /// A reconnect/close decision that must survive later RX drains until the
    /// receiver observes it and unregisters the flow.
    TerminalReady,
}

type RawTailParseFn =
    for<'input> unsafe fn(NonNull<()>, RawTailInput<'input>) -> RawTailParseDisposition;

/// Non-owning, worker-local pointer to a pinned synchronous raw-tail parser.
#[derive(Debug, Clone, Copy)]
pub struct RawTailParserBinding {
    context: NonNull<()>,
    parse: RawTailParseFn,
}

impl RawTailParserBinding {
    /// Bind a pinned parser allocation to the driver.
    ///
    /// # Safety
    ///
    /// `context` must remain valid and pinned until `unregister_raw_tail`
    /// removes this binding. The callback must use that allocation with its
    /// declared signature and return synchronously. The caller's single-worker
    /// DriverSlot call graph must statically exclude overlapping entry; this
    /// hot path deliberately has no dynamic re-entry guard.
    pub unsafe fn new(context: NonNull<()>, parse: RawTailParseFn) -> Self {
        Self { context, parse }
    }

    #[inline(always)]
    unsafe fn parse(self, input: RawTailInput<'_>) -> RawTailParseDisposition {
        unsafe { (self.parse)(self.context, input) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
                "raw-tail supports IPv4 flows only",
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
    fn matches_ingress_fields(
        &self,
        remote_ip: Ipv4Addr,
        local_ip: Ipv4Addr,
        remote_port: u16,
        local_port: u16,
    ) -> bool {
        self.remote_ip == remote_ip
            && self.local_ip == local_ip
            && self.remote_port == remote_port
            && self.local_port == local_port
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

#[derive(Clone, Copy)]
struct RssSlotEntry {
    rss: u32,
    slot: usize,
}

struct RssSlotMap {
    entries: Vec<Option<RssSlotEntry>>,
    len: usize,
}

impl RssSlotMap {
    fn new() -> Self {
        assert!(
            RAW_TAIL_RSS_MAP_CAP.is_power_of_two(),
            "raw-tail RSS map capacity must be a power of two"
        );
        Self {
            entries: vec![None; RAW_TAIL_RSS_MAP_CAP],
            len: 0,
        }
    }

    #[inline(always)]
    fn home(rss: u32) -> usize {
        (rss as usize) & (RAW_TAIL_RSS_MAP_CAP - 1)
    }

    #[inline(always)]
    fn next(index: usize) -> usize {
        (index + 1) & (RAW_TAIL_RSS_MAP_CAP - 1)
    }

    #[inline(always)]
    fn probe_distance(home: usize, index: usize) -> usize {
        index.wrapping_sub(home) & (RAW_TAIL_RSS_MAP_CAP - 1)
    }

    #[inline(always)]
    fn get(&self, rss: u32) -> Option<usize> {
        let mut index = Self::home(rss);
        loop {
            match self.entries[index] {
                Some(entry) if entry.rss == rss => return Some(entry.slot),
                Some(_) => index = Self::next(index),
                None => return None,
            }
        }
    }

    fn insert(&mut self, rss: u32, slot: usize) -> Result<Option<usize>, ()> {
        if self.len >= RAW_TAIL_CONNECTION_CAP {
            return Err(());
        }
        let mut index = Self::home(rss);
        loop {
            match self.entries[index] {
                Some(entry) if entry.rss == rss => return Ok(Some(entry.slot)),
                Some(_) => index = Self::next(index),
                None => {
                    self.entries[index] = Some(RssSlotEntry { rss, slot });
                    self.len += 1;
                    return Ok(None);
                }
            }
        }
    }

    fn remove(&mut self, rss: u32) -> Option<usize> {
        let mut hole = Self::home(rss);
        loop {
            match self.entries[hole] {
                Some(entry) if entry.rss == rss => break,
                Some(_) => hole = Self::next(hole),
                None => return None,
            }
        }
        let removed = self.entries[hole]
            .take()
            .expect("located raw-tail RSS entry must remain occupied");
        self.len = self
            .len
            .checked_sub(1)
            .expect("raw-tail RSS map length invariant prevents underflow");

        let mut scan = Self::next(hole);
        while let Some(entry) = self.entries[scan] {
            let home = Self::home(entry.rss);
            if Self::probe_distance(home, scan) > Self::probe_distance(home, hole) {
                self.entries[hole] = Some(entry);
                self.entries[scan] = None;
                hole = scan;
            }
            scan = Self::next(scan);
        }
        Some(removed.slot)
    }
}

struct TailConn {
    handle: RawTailHandle,
    tuple: RawTailTuple,
    rss_hash: u32,
    socket_handle: Option<SocketHandle>,
    parser: Option<RawTailParserBinding>,
    pending_mbuf: Option<OwnedMbuf>,
    receiver_waker: Option<Waker>,
    packet_ordinal: u64,
    publication_ready: bool,
    terminal_ready: bool,
    scan_strategy: RawTailScanStrategy,
    tls_record_wire_min: u16,
    next_tls_header_seq: u32,
    jump_record_ordinal: u64,
    pending_records: Option<SelectedRecordRange>,
    #[cfg(feature = "tail-ab")]
    observed_tcp_end: u32,
}

#[derive(Clone, Copy)]
struct SelectedRecordRange {
    offset: usize,
    len: usize,
    exact_record_ordinal: Option<u64>,
}

impl TailConn {
    #[inline(always)]
    fn advance_packet_ordinal(&mut self) {
        self.packet_ordinal = self.packet_ordinal.wrapping_add(1);
    }

    #[cfg(feature = "tail-ab")]
    #[inline(always)]
    fn observe_tcp_end(&mut self, tcp_end: u32) -> u64 {
        let delta = tcp_end.wrapping_sub(self.observed_tcp_end);
        if delta == 0 || delta >= (1u32 << 31) {
            return 0;
        }
        self.observed_tcp_end = tcp_end;
        u64::from(delta)
    }

    fn latch_parser_disposition(&mut self, disposition: RawTailParseDisposition) {
        if !matches!(
            disposition,
            RawTailParseDisposition::Ready | RawTailParseDisposition::TerminalReady
        ) {
            return;
        }
        let was_ready = self.publication_ready;
        self.publication_ready = true;
        if disposition == RawTailParseDisposition::TerminalReady {
            self.terminal_ready = true;
        }
        if !was_ready {
            if let Some(waker) = self.receiver_waker.as_ref() {
                waker.wake_by_ref();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RawTailErrorCounters {
    pub(crate) packet_parse_failed: u64,
    pub(crate) tuple_mismatch: u64,
    pub(crate) tcp_state_rejected: u64,
    pub(crate) tls_tail_not_found: u64,
}

#[derive(Clone, Copy)]
enum PrepareError {
    PacketParse,
    TupleMismatch,
    TcpState,
}

#[derive(Clone, Copy)]
enum ParserIssue {
    TlsTailNotFound,
}

struct ProcessedTail {
    socket_handle: SocketHandle,
    gateway_mac: EthernetAddress,
    parser_issue: Option<ParserIssue>,
}

/// Worker-local raw-tail registry and fixed-capacity selected-mbuf slots.
pub(crate) struct RawTailTable {
    next_id: u64,
    worker_index: usize,
    rss_key: Box<[u8]>,
    conns: Vec<Option<TailConn>>,
    free_slots: Vec<usize>,
    rss_to_slot: RssSlotMap,
    socket_to_slot: Vec<Option<usize>>,
    pending_slots: Vec<usize>,
    active_count: usize,
    errors: RawTailErrorCounters,
    errors_dirty: bool,
    #[cfg(feature = "tail-ab")]
    poll_tcp_advance_sum: u64,
}

// SAFETY: DpdkDriver transfers this table only as part of its process-lifetime
// owner capability. Parser pointers are installed and invoked exclusively on
// that worker; the public unsafe binding contract pins their allocations.
unsafe impl Send for RawTailTable {}

impl RawTailTable {
    pub(crate) fn new(worker_index: usize, rss_key: &[u8]) -> Self {
        if rss_key.is_empty() {
            panic!("raw-tail RSS key must not be empty");
        }
        Self {
            next_id: 1,
            worker_index,
            rss_key: rss_key.into(),
            conns: Vec::with_capacity(RAW_TAIL_CONNECTION_CAP),
            free_slots: Vec::with_capacity(RAW_TAIL_CONNECTION_CAP),
            rss_to_slot: RssSlotMap::new(),
            socket_to_slot: vec![None; RAW_TAIL_CONNECTION_CAP],
            pending_slots: Vec::with_capacity(RAW_TAIL_CONNECTION_CAP),
            active_count: 0,
            errors: RawTailErrorCounters::default(),
            errors_dirty: false,
            #[cfg(feature = "tail-ab")]
            poll_tcp_advance_sum: 0,
        }
    }

    pub(crate) fn register(&mut self, tuple: RawTailTuple) -> io::Result<RawTailHandle> {
        let rss_hash = tuple.rss_hash(&self.rss_key);
        if let Some(existing_slot) = self.rss_to_slot.get(rss_hash) {
            let existing_id = self
                .conns
                .get(existing_slot)
                .and_then(|conn| conn.as_ref())
                .map(|conn| conn.handle.id)
                .expect("raw-tail RSS index must point to an occupied connection slot");
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "raw-tail RSS hash collision rss={} existing_id={}",
                    rss_hash, existing_id
                ),
            ));
        }
        if self.free_slots.is_empty() && self.conns.len() >= RAW_TAIL_CONNECTION_CAP {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "raw-tail connection capacity exceeded cap={}",
                    RAW_TAIL_CONNECTION_CAP
                ),
            ));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("raw-tail handle id overflow");
        let slot = self.free_slots.last().copied().unwrap_or(self.conns.len());
        let handle = RawTailHandle::new(id, slot, self.worker_index);
        match self.rss_to_slot.insert(rss_hash, slot) {
            Ok(None) => {}
            Ok(Some(_)) => {
                panic!("raw-tail duplicate RSS appeared after the pre-insert uniqueness check")
            }
            Err(()) => {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "raw-tail RSS index capacity exhausted",
                ));
            }
        }
        let conn = TailConn {
            handle,
            tuple,
            rss_hash,
            socket_handle: None,
            parser: None,
            pending_mbuf: None,
            receiver_waker: None,
            packet_ordinal: 0,
            publication_ready: false,
            terminal_ready: false,
            scan_strategy: RawTailScanStrategy::Reverse,
            tls_record_wire_min: TLS_HEADER_LEN as u16 + TLS_MIN_CIPHERTEXT_LEN as u16,
            next_tls_header_seq: 0,
            jump_record_ordinal: 0,
            pending_records: None,
            #[cfg(feature = "tail-ab")]
            observed_tcp_end: 0,
        };
        if slot == self.conns.len() {
            self.conns.push(Some(conn));
        } else {
            let reused = self
                .free_slots
                .pop()
                .expect("selected reusable raw-tail slot must be on the free list");
            assert_eq!(reused, slot, "raw-tail free-list tail must match selected slot");
            assert!(
                self.conns[slot].is_none(),
                "raw-tail reusable slot must be vacant"
            );
            self.conns[slot] = Some(conn);
        }
        Ok(handle)
    }

    #[cfg(test)]
    pub(crate) fn activate_parser(
        &mut self,
        handle: RawTailHandle,
        actual_tuple: RawTailTuple,
        socket_handle: SocketHandle,
        parser: RawTailParserBinding,
    ) -> io::Result<()> {
        self.activate_parser_configured(
            handle,
            actual_tuple,
            socket_handle,
            parser,
            RawTailParserConfig {
                scan_strategy: RawTailScanStrategy::Reverse,
                tls_record_wire_min: (TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN) as u16,
                tcp_anchor: 0,
            },
        )
    }

    pub(crate) fn activate_parser_configured(
        &mut self,
        handle: RawTailHandle,
        actual_tuple: RawTailTuple,
        socket_handle: SocketHandle,
        parser: RawTailParserBinding,
        config: RawTailParserConfig,
    ) -> io::Result<()> {
        let Some(slot) = self.slot_for_handle(handle) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle does not belong to this runtime and worker",
            ));
        };
        let socket_index = socket_handle.index();
        let Some(socket_slot) = self.socket_to_slot.get(socket_index) else {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "raw-tail socket index exceeds the fixed lifecycle map",
            ));
        };
        if socket_slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "smoltcp socket already owns a raw-tail parser binding",
            ));
        }
        let conn = self.conns[slot]
            .as_mut()
            .expect("registered raw-tail id must point to an occupied slot");
        if conn.parser.is_some() || conn.socket_handle.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "raw-tail parser is already active",
            ));
        }
        if conn.tuple != actual_tuple {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "reserved raw-tail tuple does not match connected socket reserved={:?} actual={:?}",
                    conn.tuple, actual_tuple
                ),
            ));
        }
        // All fallible validation is complete. Install every activation field
        // together so capture can never observe a partially bound parser.
        conn.socket_handle = Some(socket_handle);
        conn.parser = Some(parser);
        conn.scan_strategy = config.scan_strategy;
        conn.tls_record_wire_min = config.tls_record_wire_min;
        conn.next_tls_header_seq = config.tcp_anchor;
        conn.jump_record_ordinal = 0;
        conn.pending_records = None;
        #[cfg(feature = "tail-ab")]
        {
            conn.observed_tcp_end = config.tcp_anchor;
        }
        self.socket_to_slot[socket_index] = Some(slot);
        self.active_count += 1;
        Ok(())
    }

    pub(crate) fn unregister(&mut self, handle: RawTailHandle) -> io::Result<()> {
        let Some(slot) = self.slot_for_handle(handle) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle is not registered in its owner runtime and worker",
            ));
        };
        let (rss_hash, was_active, socket_handle) = {
            let conn = self.conns[slot]
                .as_mut()
                .expect("registered raw-tail id must point to an occupied slot");
            assert_eq!(
                conn.parser.is_some(),
                conn.socket_handle.is_some(),
                "raw-tail parser and socket bindings must share one lifecycle"
            );
            let values = (conn.rss_hash, conn.parser.is_some(), conn.socket_handle);
            // Remove the non-owning pointer before any operation that can
            // return. A successful unregister therefore permits caller drop.
            conn.parser = None;
            conn.socket_handle = None;
            conn.publication_ready = false;
            conn.terminal_ready = false;
            conn.receiver_waker = None;
            values
        };
        if let Some(socket_handle) = socket_handle {
            let socket_slot = self
                .socket_to_slot
                .get_mut(socket_handle.index())
                .expect("active raw-tail socket index must fit the fixed lifecycle map");
            assert_eq!(
                *socket_slot,
                Some(slot),
                "raw-tail socket reverse index must identify its connection"
            );
            *socket_slot = None;
        }
        let removed_slot = self.rss_to_slot.remove(rss_hash);
        assert_eq!(
            removed_slot,
            Some(slot),
            "raw-tail RSS index must remove the registered connection slot"
        );
        if was_active {
            self.active_count = self
                .active_count
                .checked_sub(1)
                .expect("raw-tail active-count invariant prevents underflow");
        }
        let removed = self.conns[slot].take();
        drop(removed);
        self.free_slots.push(slot);
        Ok(())
    }

    /// Detach a parser before its smoltcp socket is removed. The raw handle and
    /// RSS reservation remain registered so the wrapper can subsequently call
    /// `unregister_raw_tail` successfully, but no future packet or driver poll
    /// can dereference the parser pointer or the recycled SocketHandle index.
    pub(crate) fn detach_socket(&mut self, socket_handle: SocketHandle) -> bool {
        let Some(socket_slot) = self.socket_to_slot.get_mut(socket_handle.index()) else {
            return false;
        };
        let Some(slot) = socket_slot.take() else {
            return false;
        };
        let (was_active, receiver_waker) = {
            let conn = self.conns[slot]
                .as_mut()
                .expect("located raw-tail socket binding must remain occupied");
            assert!(
                conn.parser.is_some(),
                "raw-tail socket reverse index must point to an active parser binding"
            );
            let was_active = conn.parser.is_some();
            // Remove non-owning capabilities before releasing the mbuf or
            // allowing the SocketSet index to be recycled.
            conn.parser = None;
            conn.socket_handle = None;
            conn.publication_ready = false;
            conn.terminal_ready = false;
            let receiver_waker = conn.receiver_waker.take();
            conn.pending_mbuf = None;
            conn.pending_records = None;
            (was_active, receiver_waker)
        };
        if was_active {
            self.active_count = self
                .active_count
                .checked_sub(1)
                .expect("raw-tail detach active-count invariant prevents underflow");
        }
        // Capabilities and retained mbufs are gone before a receiver is woken.
        // Its next poll observes the inactive binding and reconnects.
        if let Some(waker) = receiver_waker {
            waker.wake();
        }
        true
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.active_count == 0
    }

    #[inline(always)]
    pub(crate) fn capture_mbuf(&mut self, mbuf: OwnedMbuf) -> Result<(), OwnedMbuf> {
        if self.active_count == 0 {
            return Err(mbuf);
        }
        // NIC RSS metadata is the first packet access before tail selection.
        let Some(rss_hash) = mbuf.rss_hash() else {
            return Err(mbuf);
        };
        let Some(slot) = self.rss_to_slot.get(rss_hash) else {
            return Err(mbuf);
        };
        #[cfg(feature = "tail-ab")]
        let mut tcp_advance = 0u64;
        let old_was_none = {
            let Some(conn) = self.conns.get_mut(slot).and_then(|conn| conn.as_mut()) else {
                return Err(mbuf);
            };
            if conn.parser.is_none() {
                return Err(mbuf);
            }
            if conn.terminal_ready {
                // Terminal publication is latched until the receiver causes
                // unregister. The selected packet owner drops here without
                // touching packet headers or payload.
                return Ok(());
            }
            conn.advance_packet_ordinal();
            let jump_view = parse_jump_tcp_view(&mbuf);
            let header_in_payload = jump_view.as_ref().is_some_and(|view| {
                (conn.next_tls_header_seq.wrapping_sub(view.tcp_seq) as usize)
                    < view.payload.len()
            });
            let tuple_matches = jump_view
                .as_ref()
                .is_some_and(|view| jump_view_matches_tuple(&conn.tuple, view));
            #[cfg(feature = "tail-ab")]
            if tuple_matches {
                let view = jump_view
                    .as_ref()
                    .expect("matched jump view must remain available in this RX iteration");
                let tcp_end = view.tcp_seq.wrapping_add(view.payload.len() as u32);
                tcp_advance = conn.observe_tcp_end(tcp_end);
            }
            #[cfg(feature = "market-trace")]
            let locate_scope = crate::runtime::market_trace::scope(
                crate::runtime::market_trace::SPAN_DPDK_RAW_TAIL_LOCATE,
                crate::runtime::market_trace::dpdk_track(conn.handle.worker_index),
                jump_view.as_ref().map_or(0, |view| view.payload.len()) as u64,
            );
            let selected = match conn.scan_strategy {
                RawTailScanStrategy::Reverse if tuple_matches => jump_view
                    .as_ref()
                    .and_then(|view| {
                        find_tail_aligned_tls_records(
                            view.payload,
                            usize::from(conn.tls_record_wire_min),
                        )
                    })
                    .map(|selected| SelectedRecordRange {
                        offset: selected.offset,
                        len: selected.len,
                        exact_record_ordinal: selected.exact_record_ordinal,
                    }),
                RawTailScanStrategy::Reverse => None,
                RawTailScanStrategy::ForwardJump if header_in_payload && tuple_matches => {
                    jump_view.as_ref().and_then(|view| {
                        select_forward_tls_records_from_payload(conn, view.payload, view.tcp_seq)
                    })
                }
                RawTailScanStrategy::ForwardJump => None,
            };
            #[cfg(feature = "market-trace")]
            drop(locate_scope);
            let connection_closed = tuple_matches
                && jump_view
                    .as_ref()
                    .is_some_and(|view| view.tcp[13] & 0x05 != 0);
            if selected.is_none() && !connection_closed {
                None
            } else {
                conn.pending_records = selected;
                let old = replace_latest(&mut conn.pending_mbuf, mbuf);
                let old_was_none = old.is_none();
                drop(old);
                Some(old_was_none)
            }
        };
        if old_was_none == Some(true) {
            self.pending_slots.push(slot);
        }
        #[cfg(feature = "tail-ab")]
        {
            self.poll_tcp_advance_sum += tcp_advance;
        }
        Ok(())
    }

    #[cfg(feature = "tail-ab")]
    #[inline(always)]
    pub(crate) fn take_poll_tcp_advance_sum(&mut self) -> u64 {
        let total = self.poll_tcp_advance_sum;
        self.poll_tcp_advance_sum = 0;
        total
    }

    /// Commit every selected TCP tail and synchronously invoke its parser while
    /// the selected mbuf is still owned locally. The returned socket handles
    /// are handed to the driver's existing shared egress scheduler only after
    /// each parser has returned and its mbuf has been released.
    pub(crate) fn finish_drain(
        &mut self,
        now: SmolInstant,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static, LinearBuffer<'static>>,
        gateway_neighbor_configured: bool,
        egress_handles: &mut Vec<SocketHandle>,
    ) -> bool {
        egress_handles.clear();
        let mut observed_gateway = None;
        for pending_index in 0..self.pending_slots.len() {
            let slot = self.pending_slots[pending_index];
            let outcome = {
                let conn = self.conns[slot]
                    .as_mut()
                    .expect("pending raw-tail slot must contain a connection");
                Self::process_selected_tail(conn, now, sockets)
            };
            match outcome {
                Ok(processed) => {
                    if observed_gateway.is_none() {
                        observed_gateway = Some(processed.gateway_mac);
                    }
                    match processed.parser_issue {
                        Some(ParserIssue::TlsTailNotFound) => {
                            self.errors.tls_tail_not_found =
                                self.errors.tls_tail_not_found.saturating_add(1);
                            self.errors_dirty = true;
                        }
                        None => {}
                    }
                    egress_handles.push(processed.socket_handle);
                }
                Err(PrepareError::PacketParse) => {
                    self.errors.packet_parse_failed =
                        self.errors.packet_parse_failed.saturating_add(1);
                    self.errors_dirty = true;
                }
                Err(PrepareError::TupleMismatch) => {
                    self.errors.tuple_mismatch = self.errors.tuple_mismatch.saturating_add(1);
                    self.errors_dirty = true;
                }
                Err(PrepareError::TcpState) => {
                    self.errors.tcp_state_rejected =
                        self.errors.tcp_state_rejected.saturating_add(1);
                    self.errors_dirty = true;
                }
            }
        }

        let gateway_observed = if gateway_neighbor_configured {
            if let Some(gateway_mac) = observed_gateway {
                gateway_update_was_observed(iface.observe_gateway_hardware_addr(
                    now, HardwareAddress::Ethernet(gateway_mac),
                ))
            } else {
                false
            }
        } else {
            false
        };
        self.pending_slots.clear();
        gateway_observed
    }

    fn process_selected_tail(
        conn: &mut TailConn,
        now: SmolInstant,
        sockets: &mut SocketSet<'static, LinearBuffer<'static>>,
    ) -> Result<ProcessedTail, PrepareError> {
        #[cfg(feature = "market-trace")]
        let track_id = crate::runtime::market_trace::dpdk_track(conn.handle.worker_index);
        let mbuf = conn
            .pending_mbuf
            .take()
            .expect("selected raw-tail slot must own one mbuf");
        #[cfg(feature = "market-trace")]
        let packet_parse_scope = crate::runtime::market_trace::scope(
            crate::runtime::market_trace::SPAN_DPDK_RAW_TAIL_PACKET_PARSE,
            track_id,
            0,
        );
        let parsed = parse_selected_tcp(&mbuf).ok_or(PrepareError::PacketParse)?;
        if !conn.tuple.matches_ingress_fields(
            parsed.remote_ip,
            parsed.local_ip,
            parsed.tcp_repr.src_port,
            parsed.tcp_repr.dst_port,
        ) {
            return Err(PrepareError::TupleMismatch);
        }
        let socket_handle = conn
            .socket_handle
            .expect("active raw-tail connection must retain its socket handle");
        // remove_socket() detaches raw-tail before removing/recycling this
        // index, so this direct O(1) access remains the activated TCP socket.
        let socket = sockets.get_mut::<TcpSocket<'_, LinearBuffer<'_>>>(socket_handle);
        #[cfg(feature = "market-trace")]
        drop(packet_parse_scope);
        #[cfg(feature = "market-trace")]
        let tcp_commit_scope = crate::runtime::market_trace::scope(
            crate::runtime::market_trace::SPAN_DPDK_RAW_TAIL_TCP_COMMIT,
            track_id,
            parsed.tcp_repr.payload.len() as u64,
        );
        let commit = socket
            .commit_lossy_tail(now, &parsed.tcp_repr)
            .map_err(|_| PrepareError::TcpState)?;
        #[cfg(feature = "market-trace")]
        drop(tcp_commit_scope);

        let tcp_seq = parsed.tcp_repr.seq_number.0 as u32;
        let connection_closed = commit.closed
            || matches!(parsed.tcp_repr.control, TcpControl::Fin | TcpControl::Rst);
        let parser = conn
            .parser
            .expect("active raw-tail connection must retain its parser binding");
        let selected = conn.pending_records.take();
        let parser_issue = Self::dispatch_committed_payload(
            conn,
            parser,
            parsed.tcp_repr.payload,
            tcp_seq,
            connection_closed,
            selected,
        );
        let processed = ProcessedTail {
            socket_handle,
            gateway_mac: parsed.eth_src,
            parser_issue,
        };
        drop(parsed);
        drop(mbuf);
        Ok(processed)
    }

    fn dispatch_committed_payload(
        conn: &mut TailConn,
        parser: RawTailParserBinding,
        payload: &[u8],
        tcp_seq: u32,
        connection_closed: bool,
        selected: Option<SelectedRecordRange>,
    ) -> Option<ParserIssue> {
        let tail = selected.map(|selected| TailAlignedTlsRecords {
            offset: selected.offset,
            len: selected.len,
            exact_record_ordinal: selected.exact_record_ordinal,
        });
        #[cfg(feature = "market-trace")]
        let track_id = crate::runtime::market_trace::dpdk_track(conn.handle.worker_index);
        let (records, record_tcp_seq) = match tail {
            Some(tls_tail) => (
                &payload[tls_tail.offset..tls_tail.offset + tls_tail.len],
                tcp_seq.wrapping_add(tls_tail.offset as u32),
            ),
            None => (&payload[payload.len()..], tcp_seq),
        };

        if records.is_empty() && !connection_closed {
            return (!payload.is_empty()).then_some(ParserIssue::TlsTailNotFound);
        }
        let input = RawTailInput {
            tcp_seq: record_tcp_seq,
            records,
            packet_ordinal: conn.packet_ordinal,
            connection_closed,
            exact_record_ordinal: tail.and_then(|tail| tail.exact_record_ordinal),
        };
        // SAFETY: the pinned binding contract remains active, and `records`
        // cannot escape this synchronous HRTB call. The selected mbuf remains
        // owned by process_selected_tail until this call returns.
        #[cfg(feature = "market-trace")]
        let parser_scope = crate::runtime::market_trace::scope(
            crate::runtime::market_trace::SPAN_DPDK_RAW_TAIL_PARSER,
            track_id,
            records.len() as u64,
        );
        let disposition = unsafe { parser.parse(input) };
        #[cfg(feature = "market-trace")]
        drop(parser_scope);
        let disposition = latch_closed_disposition(disposition, connection_closed);
        #[cfg(feature = "market-trace")]
        let wake_scope = crate::runtime::market_trace::scope(
            crate::runtime::market_trace::SPAN_DPDK_RAW_TAIL_WAKE,
            track_id,
            disposition as u64,
        );
        conn.latch_parser_disposition(disposition);
        #[cfg(feature = "market-trace")]
        drop(wake_scope);
        None
    }

    pub(crate) fn poll_publication_ready(
        &mut self,
        handle: RawTailHandle,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(conn) = self.conn_for_handle_mut(handle) else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "raw-tail handle does not belong to this runtime and worker",
            )));
        };
        if conn.parser.is_none() || conn.socket_handle.is_none() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raw-tail parser is not active",
            )));
        }
        if conn.receiver_waker.is_none() {
            conn.receiver_waker = Some(cx.waker().clone());
        }
        if !conn.publication_ready {
            return Poll::Pending;
        }
        conn.publication_ready = false;
        Poll::Ready(Ok(()))
    }

    #[inline(always)]
    pub(crate) fn errors_dirty(&self) -> bool {
        self.errors_dirty
    }

    pub(crate) fn take_error_counters(&mut self) -> RawTailErrorCounters {
        self.errors_dirty = false;
        std::mem::take(&mut self.errors)
    }

    #[cfg(feature = "dpdk-raw-mbuf-capture")]
    pub(crate) fn release_pending_mbufs_for_shutdown(&mut self) {
        self.pending_slots.clear();
        for conn in self.conns.iter_mut().flatten() {
            drop(conn.pending_mbuf.take());
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
        let conn = self.conns.get(handle.slot)?.as_ref()?;
        if conn.handle.id != handle.id {
            return None;
        }
        Some(handle.slot)
    }
}

#[inline(always)]
fn replace_latest<T>(slot: &mut Option<T>, latest: T) -> Option<T> {
    std::mem::replace(slot, Some(latest))
}

#[inline(always)]
fn gateway_update_was_observed(update: GatewayNeighborUpdate) -> bool {
    matches!(
        update,
        GatewayNeighborUpdate::Changed
            | GatewayNeighborUpdate::Resolved
            | GatewayNeighborUpdate::Unchanged
    )
}

#[inline(always)]
fn latch_closed_disposition(
    disposition: RawTailParseDisposition,
    connection_closed: bool,
) -> RawTailParseDisposition {
    if connection_closed && disposition == RawTailParseDisposition::Ready {
        RawTailParseDisposition::TerminalReady
    } else {
        disposition
    }
}

struct ParsedSelectedTcp<'a> {
    eth_src: EthernetAddress,
    remote_ip: Ipv4Addr,
    local_ip: Ipv4Addr,
    tcp_repr: TcpRepr<'a>,
}

/// Parse the selected frame only after the RX drain has established it as this
/// RSS flow's final mbuf. The lossy-tail TCP representation omits options which
/// cannot affect sender state; no payload is copied.
#[inline(always)]
fn parse_selected_tcp(mbuf: &OwnedMbuf) -> Option<ParsedSelectedTcp<'_>> {
    let frame = mbuf.data()?;
    let view = parse_jump_tcp_frame(frame)?;
    let tcp = TcpPacket::new_unchecked(view.tcp);
    let tcp_repr = TcpRepr::parse_lossy_tail(&tcp).ok()?;
    Some(ParsedSelectedTcp {
        eth_src: EthernetAddress([frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]]),
        remote_ip: Ipv4Addr::new(view.ip[12], view.ip[13], view.ip[14], view.ip[15]),
        local_ip: Ipv4Addr::new(view.ip[16], view.ip[17], view.ip[18], view.ip[19]),
        tcp_repr,
    })
}

#[derive(Clone, Copy)]
struct TailAlignedTlsRecords {
    offset: usize,
    len: usize,
    exact_record_ordinal: Option<u64>,
}

#[inline(always)]
fn tls_record_len_at(payload: &[u8], offset: usize) -> Option<usize> {
    let header_end = offset + TLS_HEADER_LEN;
    let header = payload.get(offset..header_end)?;
    if header[0] != 0x17 || header[1] != 0x03 || header[2] != 0x03 {
        return None;
    }
    let ciphertext_len = u16::from_be_bytes([header[3], header[4]]) as usize;
    if !(TLS_MIN_CIPHERTEXT_LEN..=TLS_MAX_CIPHERTEXT_LEN).contains(&ciphertext_len) {
        return None;
    }
    Some(TLS_HEADER_LEN + ciphertext_len)
}

/// Locate the newest complete TLS 1.3 record ending at this TCP payload. The
/// common single-record path reads only offset zero. Otherwise search backward
/// from this connection's application-derived minimum wire length and stop at
/// the TLS ciphertext maximum;
/// a miss never scans an earlier part of a large payload or another mbuf.
fn find_tail_aligned_tls_records(
    payload: &[u8],
    per_connection_min_wire_len: usize,
) -> Option<TailAlignedTlsRecords> {
    const TLS_MAX_WIRE_LEN: usize = TLS_HEADER_LEN + TLS_MAX_CIPHERTEXT_LEN;
    let mut record_end = payload.len();
    loop {
        if record_end < TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN {
            return None;
        }

        let offset = if tls_record_len_at(payload, 0) == Some(record_end) {
            0
        } else {
            let oldest_offset = record_end.saturating_sub(TLS_MAX_WIRE_LEN).max(1);
            let newest_offset = record_end - (TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN);
            if oldest_offset > newest_offset {
                return None;
            }
            let search = &payload[oldest_offset..=newest_offset];
            let mut search_end = search.len();
            let mut matched = None;
            while search_end != 0 {
                // SAFETY: `search` is a live slice and the returned pointer is
                // used only to recover an offset within that same slice.
                let found = unsafe {
                    libc::memrchr(
                        search.as_ptr().cast(),
                        0x17,
                        search_end,
                    )
                };
                if found.is_null() {
                    break;
                }
                let relative = unsafe { found.cast::<u8>().offset_from(search.as_ptr()) as usize };
                let candidate = oldest_offset + relative;
                if tls_record_len_at(payload, candidate)
                    .is_some_and(|len| candidate + len == record_end)
                {
                    matched = Some(candidate);
                    break;
                }
                search_end = relative;
            }
            matched?
        };
        let record_len = record_end - offset;
        if record_len >= per_connection_min_wire_len {
            return Some(TailAlignedTlsRecords {
                offset,
                len: record_len,
                exact_record_ordinal: None,
            });
        }

        // A short control record can trail the wanted application record. It
        // is framing only: peel it and continue without decrypting or copying.
        record_end = offset;
    }
}

#[inline(always)]
fn jump_view_matches_tuple(tuple: &RawTailTuple, view: &JumpTcpView<'_>) -> bool {
    tuple.matches_ingress_fields(
        Ipv4Addr::new(view.ip[12], view.ip[13], view.ip[14], view.ip[15]),
        Ipv4Addr::new(view.ip[16], view.ip[17], view.ip[18], view.ip[19]),
        u16::from_be_bytes([view.tcp[0], view.tcp[1]]),
        u16::from_be_bytes([view.tcp[2], view.tcp[3]]),
    )
}

#[derive(Clone, Copy)]
struct JumpTcpView<'a> {
    ip: &'a [u8],
    tcp: &'a [u8],
    tcp_seq: u32,
    payload: &'a [u8],
}

/// Read only the fixed L2/L3/L4 fields required to decide whether the next
/// tracked record header can occur in this packet. Only FIN/RST is read from
/// the TCP flags; ACK/window/options remain untouched until full tail parsing.
#[inline(always)]
fn parse_jump_tcp_view(mbuf: &OwnedMbuf) -> Option<JumpTcpView<'_>> {
    parse_jump_tcp_frame(mbuf.data()?)
}

#[inline(always)]
fn parse_jump_tcp_frame(frame: &[u8]) -> Option<JumpTcpView<'_>> {
    const ETHERNET_HEADER_LEN: usize = 14;
    const IPV4_ETHERTYPE: u16 = 0x0800;
    const TCP_PROTOCOL: u8 = 6;
    const MIN_IPV4_HEADER_LEN: usize = 20;
    const MIN_TCP_HEADER_LEN: usize = 20;

    if frame.len() < ETHERNET_HEADER_LEN + MIN_IPV4_HEADER_LEN {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != IPV4_ETHERTYPE {
        return None;
    }
    let ip = &frame[ETHERNET_HEADER_LEN..];
    if ip[0] >> 4 != 4 {
        return None;
    }
    let ip_header_len = usize::from(ip[0] & 0x0f) * 4;
    if ip_header_len < MIN_IPV4_HEADER_LEN || ip.len() < ip_header_len + MIN_TCP_HEADER_LEN {
        return None;
    }
    let ip_total_len = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if ip_total_len < ip_header_len + MIN_TCP_HEADER_LEN || ip_total_len > ip.len() {
        return None;
    }
    if ip[9] != TCP_PROTOCOL {
        return None;
    }
    let tcp = &ip[ip_header_len..ip_total_len];
    let tcp_header_len = usize::from(tcp[12] >> 4) * 4;
    if tcp_header_len < MIN_TCP_HEADER_LEN || tcp_header_len > tcp.len() {
        return None;
    }
    Some(JumpTcpView {
        ip,
        tcp,
        tcp_seq: u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]),
        payload: &tcp[tcp_header_len..],
    })
}

#[inline(always)]
fn select_forward_tls_records_from_payload(
    conn: &mut TailConn,
    payload: &[u8],
    packet_seq: u32,
) -> Option<SelectedRecordRange> {
    let header_offset = conn.next_tls_header_seq.wrapping_sub(packet_seq) as usize;
    if header_offset >= payload.len() {
        return None;
    }

    let mut cursor = header_offset;
    let mut selected = None;
    while let Some(record_len) = tls_record_len_at(payload, cursor) {
        let record_end = cursor + record_len;
        conn.next_tls_header_seq = packet_seq.wrapping_add(record_end as u32);
        let record_ordinal = conn.jump_record_ordinal;
        conn.jump_record_ordinal = conn.jump_record_ordinal.wrapping_add(1);
        if record_end > payload.len() {
            break;
        }
        if record_len >= usize::from(conn.tls_record_wire_min) {
            selected = Some(SelectedRecordRange {
                offset: cursor,
                len: record_len,
                exact_record_ordinal: Some(record_ordinal),
            });
        }
        cursor = record_end;
        if cursor == payload.len() {
            break;
        }
    }
    selected
}

fn toeplitz_hash(input: &[u8], key: &[u8]) -> u32 {
    let required_key_bits = input.len() * 8 + 32;
    assert!(
        key.len() * 8 >= required_key_bits,
        "RSS key must contain every Toeplitz input window"
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::Wake;

    fn tls_record(body: &[u8]) -> Vec<u8> {
        let mut record = vec![0x17, 0x03, 0x03];
        let ciphertext_len = body.len().max(TLS_MIN_CIPHERTEXT_LEN);
        record.extend_from_slice(&(ciphertext_len as u16).to_be_bytes());
        record.extend_from_slice(body);
        record.resize(TLS_HEADER_LEN + ciphertext_len, 0);
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

    fn test_owner(worker_index: usize) -> usize {
        worker_index
    }

    #[derive(Default)]
    struct ParserProbe {
        calls: usize,
        input_ptr: usize,
        input_len: usize,
    }

    unsafe fn parse_probe(
        context: NonNull<()>,
        input: RawTailInput<'_>,
    ) -> RawTailParseDisposition {
        let mut probe = context.cast::<ParserProbe>();
        let probe = unsafe { probe.as_mut() };
        probe.calls += 1;
        probe.input_ptr = input.records.as_ptr() as usize;
        probe.input_len = input.records.len();
        RawTailParseDisposition::Ready
    }

    fn parser_binding(probe: &mut ParserProbe) -> RawTailParserBinding {
        unsafe { RawTailParserBinding::new(NonNull::from(probe).cast(), parse_probe) }
    }

    struct TestWake;

    impl Wake for TestWake {
        fn wake(self: Arc<Self>) {}
    }

    struct CountWake(Arc<AtomicUsize>);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn latest_slot_replacement_drops_the_superseded_owner() {
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = Some(DropProbe(drops.clone()));
        let old = replace_latest(&mut slot, DropProbe(drops.clone()));
        drop(old);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(slot);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn packet_ordinal_uses_infallible_wrapping_increment() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let conn = table.conns[slot].as_mut().unwrap();
        conn.packet_ordinal = u64::MAX - 1;
        conn.advance_packet_ordinal();
        assert_eq!(conn.packet_ordinal, u64::MAX);
        conn.advance_packet_ordinal();
        assert_eq!(conn.packet_ordinal, 0);
    }

    #[cfg(feature = "tail-ab")]
    #[test]
    fn tcp_end_state_counts_only_forward_sequence_advance() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let conn = table.conns[slot].as_mut().unwrap();
        conn.observed_tcp_end = 100;
        assert_eq!(conn.observe_tcp_end(140), 40);
        assert_eq!(conn.observe_tcp_end(120), 0);
        assert_eq!(conn.observe_tcp_end(140), 0);
        assert_eq!(conn.observe_tcp_end(175), 35);
    }

    #[test]
    fn parser_binding_has_no_invalidation_or_reentry_guard_and_borrows_input() {
        let mut probe = ParserProbe::default();
        let binding = parser_binding(&mut probe);
        let bytes = tls_record(b"market");
        let disposition = unsafe {
            binding.parse(RawTailInput {
                tcp_seq: 10,
                records: &bytes,
                packet_ordinal: 3,
                connection_closed: false,
                exact_record_ordinal: None,
            })
        };
        assert_eq!(disposition, RawTailParseDisposition::Ready);
        assert_eq!(probe.calls, 1);
        assert_eq!(probe.input_ptr, bytes.as_ptr() as usize);
        assert_eq!(probe.input_len, bytes.len());
    }

    #[test]
    fn selects_only_the_newest_tls_record_from_one_payload() {
        let first = tls_record(b"first");
        let second = tls_record(b"second");
        let mut payload = vec![0xaa, 0xbb, 0xcc];
        payload.extend_from_slice(&first);
        payload.extend_from_slice(&second);
        let records = find_tail_aligned_tls_records(&payload, TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN)
            .expect("newest complete TLS record must be found in one payload");
        assert_eq!(records.offset, 3 + first.len());
        assert_eq!(&payload[records.offset..], second);
    }

    #[test]
    fn finds_a_coalesced_ws_tls_record_at_the_tail_of_100kb() {
        let record = tls_record(&vec![0x5a; 845]);
        let mut payload = vec![0xaa; 100 * 1024 - record.len()];
        payload.extend_from_slice(&record);
        let records = find_tail_aligned_tls_records(&payload, TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN)
            .expect("bounded tail search must find a large coalesced TLS record");
        assert_eq!(records.offset, payload.len() - record.len());
        assert_eq!(&payload[records.offset..], record);
    }

    #[test]
    fn large_non_tls_tail_is_an_accepted_bounded_miss() {
        let payload = vec![0xaa; 100 * 1024];
        assert!(find_tail_aligned_tls_records(&payload, TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN).is_none());
    }

    #[test]
    fn reverse_search_uses_the_application_profile_minimum_after_offset_zero() {
        let record = tls_record(&vec![0x5a; 90]);
        let mut payload = vec![0xaa; 400];
        payload.extend_from_slice(&record);
        assert!(find_tail_aligned_tls_records(&payload, 165).is_none());
        assert!(find_tail_aligned_tls_records(&record, 165).is_none());

        let market_record = tls_record(&vec![0x5a; 180]);
        assert!(find_tail_aligned_tls_records(&market_record, 165).is_some());
    }

    #[test]
    fn reverse_peels_short_control_records_and_selects_the_market_record_before_them() {
        let market = tls_record(&vec![0x5a; 180]);
        let control_one = tls_record(&[]);
        let control_two = tls_record(&[0x01]);
        let mut payload = vec![0xaa; 37];
        payload.extend_from_slice(&market);
        payload.extend_from_slice(&control_one);
        payload.extend_from_slice(&control_two);

        let selected = find_tail_aligned_tls_records(&payload, 165)
            .expect("short trailing controls must not hide the final market record");
        assert_eq!(selected.offset, 37);
        assert_eq!(selected.len, market.len());
    }

    #[test]
    fn reverse_rejects_a_payload_containing_only_control_records() {
        let mut payload = tls_record(&[]);
        payload.extend_from_slice(&tls_record(&[0x01]));
        assert!(find_tail_aligned_tls_records(&payload, 165).is_none());
    }

    #[test]
    fn forward_jump_tracks_multiple_records_and_skips_to_a_split_records_end() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let conn = table.conns[slot].as_mut().unwrap();
        conn.next_tls_header_seq = 100;

        let first = tls_record(b"first");
        let second = tls_record(b"second");
        let mut payload = first.clone();
        payload.extend_from_slice(&second);
        let selected = select_forward_tls_records_from_payload(conn, &payload, 100)
            .expect("the newest complete application record must be selected");
        assert_eq!(selected.offset, first.len());
        assert_eq!(selected.len, second.len());
        assert_eq!(selected.exact_record_ordinal, Some(1));
        assert_eq!(conn.next_tls_header_seq, 100 + payload.len() as u32);

        let split_start = conn.next_tls_header_seq;
        let split_wire_len = 205u16;
        let mut split_prefix = vec![0x17, 0x03, 0x03];
        split_prefix.extend_from_slice(&(split_wire_len - TLS_HEADER_LEN as u16).to_be_bytes());
        split_prefix.resize(40, 0x55);
        assert!(select_forward_tls_records_from_payload(conn, &split_prefix, split_start).is_none());
        assert_eq!(conn.next_tls_header_seq, split_start + u32::from(split_wire_len));

        let after_split = tls_record(b"after-split");
        let selected = select_forward_tls_records_from_payload(
            conn,
            &after_split,
            split_start + u32::from(split_wire_len),
        )
        .expect("packet beginning at the jumped boundary must be selected");
        assert_eq!(selected.offset, 0);
        assert_eq!(selected.len, after_split.len());
        assert_eq!(selected.exact_record_ordinal, Some(3));
    }

    #[test]
    fn forward_counts_control_headers_but_never_selects_them() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let conn = table.conns[slot].as_mut().unwrap();
        conn.next_tls_header_seq = 100;
        conn.tls_record_wire_min = 165;

        let control = tls_record(&[]);
        let market = tls_record(&vec![0x5a; 180]);
        let mut payload = control.clone();
        payload.extend_from_slice(&market);
        payload.extend_from_slice(&control);
        let selected = select_forward_tls_records_from_payload(conn, &payload, 100)
            .expect("market record between controls must be selected");
        assert_eq!(selected.offset, control.len());
        assert_eq!(selected.len, market.len());
        assert_eq!(selected.exact_record_ordinal, Some(1));
        assert_eq!(conn.jump_record_ordinal, 3);
        assert_eq!(conn.next_tls_header_seq, 100 + payload.len() as u32);

        let controls_only = [control.clone(), control].concat();
        assert!(select_forward_tls_records_from_payload(
            conn,
            &controls_only,
            conn.next_tls_header_seq,
        )
        .is_none());
        assert_eq!(conn.jump_record_ordinal, 5);
    }

    #[test]
    fn does_not_assemble_a_tls_record_split_across_tcp_payloads() {
        let first_packet = [0x17, 0x03, 0x03, 0x00, 0x08, 1, 2, 3];
        let second_packet = [4, 5, 6, 7, 8];
        assert!(find_tail_aligned_tls_records(&first_packet, TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN).is_none());
        assert!(find_tail_aligned_tls_records(&second_packet, TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN).is_none());
    }

    #[test]
    fn accepted_gateway_updates_count_as_this_drains_observation() {
        assert!(gateway_update_was_observed(GatewayNeighborUpdate::Changed));
        assert!(gateway_update_was_observed(GatewayNeighborUpdate::Resolved));
        assert!(gateway_update_was_observed(GatewayNeighborUpdate::Unchanged));
        assert!(!gateway_update_was_observed(GatewayNeighborUpdate::Ignored));
    }

    #[test]
    fn rss_removal_backshifts_a_wrapped_probe_cluster() {
        let mut map = RssSlotMap::new();
        let base = (RAW_TAIL_RSS_MAP_CAP - 1) as u32;
        let second = base.wrapping_add(RAW_TAIL_RSS_MAP_CAP as u32);
        let third = second.wrapping_add(RAW_TAIL_RSS_MAP_CAP as u32);
        assert_eq!(map.insert(base, 1), Ok(None));
        assert_eq!(map.insert(second, 2), Ok(None));
        assert_eq!(map.insert(third, 3), Ok(None));
        assert_eq!(map.remove(base), Some(1));
        assert_eq!(map.get(second), Some(2));
        assert_eq!(map.get(third), Some(3));
        assert_eq!(map.remove(second), Some(2));
        assert_eq!(map.get(third), Some(3));
        assert_eq!(map.len, 1);
    }

    #[test]
    fn parser_precedes_shared_egress_and_selected_owner_drops_on_return() {
        struct OrderParser {
            events: Rc<RefCell<Vec<&'static str>>>,
        }

        unsafe fn parse_order_parser(
            context: NonNull<()>,
            _input: RawTailInput<'_>,
        ) -> RawTailParseDisposition {
            let parser = unsafe { context.cast::<OrderParser>().as_ref() };
            parser.events.borrow_mut().push("parser");
            RawTailParseDisposition::Ready
        }

        struct SelectedOwner {
            payload: Vec<u8>,
            events: Rc<RefCell<Vec<&'static str>>>,
        }

        impl Drop for SelectedOwner {
            fn drop(&mut self) {
                self.events.borrow_mut().push("mbuf_drop");
            }
        }

        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let socket_handle = SocketHandle::default();
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut parser = OrderParser {
            events: events.clone(),
        };
        let binding = unsafe {
            RawTailParserBinding::new(
                NonNull::from(&mut parser).cast(),
                parse_order_parser,
            )
        };
        table
            .activate_parser(
                handle,
                tuple(443),
                socket_handle,
                binding,
            )
            .expect("activation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let selected = SelectedOwner {
            payload: tls_record(b"latest"),
            events: events.clone(),
        };
        let committed_socket = {
            let conn = table.conns[slot].as_mut().unwrap();
            let parser = conn.parser.unwrap();
            let selected_range = find_tail_aligned_tls_records(
                &selected.payload,
                TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN,
            )
            .map(|selected| SelectedRecordRange {
                offset: selected.offset,
                len: selected.len,
                exact_record_ordinal: selected.exact_record_ordinal,
            });
            assert!(RawTailTable::dispatch_committed_payload(
                conn,
                parser,
                &selected.payload,
                7,
                false,
                selected_range,
            )
            .is_none());
            assert_eq!(&*events.borrow(), &["parser"]);
            conn.socket_handle.unwrap()
        };
        drop(selected);
        assert_eq!(&*events.borrow(), &["parser", "mbuf_drop"]);

        let mut shared_egress = Vec::with_capacity(RAW_TAIL_CONNECTION_CAP);
        shared_egress.push(committed_socket);
        events.borrow_mut().push("egress_queue");
        events.borrow_mut().push("poll_tail_flush");
        assert_eq!(shared_egress, [socket_handle]);
        assert_eq!(
            &*events.borrow(),
            &["parser", "mbuf_drop", "egress_queue", "poll_tail_flush"]
        );
    }

    #[test]
    fn activation_is_transactional_across_worker_tuple_and_socket_binding() {
        let owner = test_owner(0);
        let mut table = RawTailTable::new(owner, &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let mut probe = ParserProbe::default();
        let binding = parser_binding(&mut probe);

        let foreign_worker = RawTailHandle::new(
            handle.id(),
            handle.slot,
            owner + 1,
        );
        assert_eq!(
            table
                .activate_parser(
                    foreign_worker,
                    tuple(443),
                    SocketHandle::default(),
                    binding,
                )
                .expect_err("foreign worker handle must fail")
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            table
                .activate_parser(handle, tuple(444), SocketHandle::default(), binding)
                .expect_err("wrong tuple must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        table
            .activate_parser(handle, tuple(443), SocketHandle::default(), binding)
            .expect("matching tuple and socket must activate");
        assert_eq!(
            table
                .activate_parser(handle, tuple(443), SocketHandle::default(), binding)
                .expect_err("second activation must not replace the parser")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn publication_poll_keeps_the_first_receiver_waker_without_identity_checks() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let mut probe = ParserProbe::default();
        table
            .activate_parser(
                handle,
                tuple(443),
                SocketHandle::default(),
                parser_binding(&mut probe),
            )
            .expect("activation must succeed");
        let owner_waker = Waker::from(Arc::new(TestWake));
        let mut owner_cx = Context::from_waker(&owner_waker);
        assert!(table.poll_publication_ready(handle, &mut owner_cx).is_pending());
        let slot = table.slot_for_handle(handle).unwrap();
        table.conns[slot].as_mut().unwrap().publication_ready = true;
        assert!(matches!(
            table.poll_publication_ready(handle, &mut owner_cx),
            Poll::Ready(Ok(()))
        ));
        assert!(table.poll_publication_ready(handle, &mut owner_cx).is_pending());

        let other_waker = Waker::from(Arc::new(TestWake));
        let mut other_cx = Context::from_waker(&other_waker);
        assert!(table.poll_publication_ready(handle, &mut other_cx).is_pending());
        let stored = table.conns[slot]
            .as_ref()
            .unwrap()
            .receiver_waker
            .as_ref()
            .unwrap();
        assert!(stored.will_wake(&owner_waker));
        assert!(!stored.will_wake(&other_waker));
    }

    #[test]
    fn publication_wake_is_coalesced_until_the_receiver_consumes_ready() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let mut probe = ParserProbe::default();
        table
            .activate_parser(
                handle,
                tuple(443),
                SocketHandle::default(),
                parser_binding(&mut probe),
            )
            .expect("activation must succeed");
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountWake(wakes.clone())));
        let mut cx = Context::from_waker(&waker);
        assert!(table.poll_publication_ready(handle, &mut cx).is_pending());

        let slot = table.slot_for_handle(handle).unwrap();
        let conn = table.conns[slot].as_mut().unwrap();
        conn.latch_parser_disposition(RawTailParseDisposition::Ready);
        conn.latch_parser_disposition(RawTailParseDisposition::Ready);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(matches!(
            table.poll_publication_ready(handle, &mut cx),
            Poll::Ready(Ok(()))
        ));
        table.conns[slot]
            .as_mut()
            .unwrap()
            .latch_parser_disposition(RawTailParseDisposition::Ready);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn parser_miss_and_empty_result_preserve_an_unconsumed_publication() {
        #[derive(Default)]
        struct EmptyProbe {
            calls: usize,
        }

        unsafe fn parse_empty(
            context: NonNull<()>,
            _input: RawTailInput<'_>,
        ) -> RawTailParseDisposition {
            let mut probe = context.cast::<EmptyProbe>();
            unsafe { probe.as_mut() }.calls += 1;
            RawTailParseDisposition::Empty
        }

        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let mut probe = EmptyProbe::default();
        let binding = unsafe {
            RawTailParserBinding::new(NonNull::from(&mut probe).cast(), parse_empty)
        };
        table
            .activate_parser(
                handle,
                tuple(443),
                SocketHandle::default(),
                binding,
            )
            .expect("activation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let wake_count = Arc::new(AtomicUsize::new(0));
        let conn = table.conns[slot].as_mut().unwrap();
        conn.publication_ready = true;
        conn.receiver_waker = Some(Waker::from(Arc::new(CountWake(wake_count.clone()))));

        assert!(matches!(
            RawTailTable::dispatch_committed_payload(
                conn,
                binding,
                b"not tls",
                10,
                false,
                None,
            ),
            Some(ParserIssue::TlsTailNotFound)
        ));
        assert!(conn.publication_ready);
        assert_eq!(probe.calls, 0);
        assert_eq!(wake_count.load(Ordering::Relaxed), 0);

        let record = tls_record(b"new but not publishable");
        let selected = find_tail_aligned_tls_records(
            &record,
            TLS_HEADER_LEN + TLS_MIN_CIPHERTEXT_LEN,
        )
        .map(|selected| SelectedRecordRange {
            offset: selected.offset,
            len: selected.len,
            exact_record_ordinal: selected.exact_record_ordinal,
        });
        assert!(RawTailTable::dispatch_committed_payload(
            conn,
            binding,
            &record,
            11,
            false,
            selected,
        )
        .is_none());
        assert!(conn.publication_ready);
        assert_eq!(probe.calls, 1);
        assert_eq!(wake_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn terminal_publication_remains_latched_after_receiver_observes_ready() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let mut probe = ParserProbe::default();
        table
            .activate_parser(
                handle,
                tuple(443),
                SocketHandle::default(),
                parser_binding(&mut probe),
            )
            .expect("activation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        table.conns[slot]
            .as_mut()
            .unwrap()
            .latch_parser_disposition(RawTailParseDisposition::TerminalReady);
        let waker = Waker::from(Arc::new(TestWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            table.poll_publication_ready(handle, &mut cx),
            Poll::Ready(Ok(()))
        ));
        let conn = table.conns[slot].as_ref().unwrap();
        assert!(conn.terminal_ready);
        assert!(!conn.publication_ready);
    }

    #[test]
    fn a_published_fin_or_rst_is_always_terminal() {
        assert_eq!(
            latch_closed_disposition(RawTailParseDisposition::Ready, true),
            RawTailParseDisposition::TerminalReady
        );
        assert_eq!(
            latch_closed_disposition(RawTailParseDisposition::Empty, true),
            RawTailParseDisposition::Empty
        );
        assert_eq!(
            latch_closed_disposition(RawTailParseDisposition::Ready, false),
            RawTailParseDisposition::Ready
        );
    }

    #[test]
    fn unregister_removes_binding_and_reuses_a_clean_slot() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let first = table.register(tuple(443)).expect("reservation must succeed");
        let first_slot = table.slot_for_handle(first).unwrap();
        let mut probe = ParserProbe::default();
        table
            .activate_parser(
                first,
                tuple(443),
                SocketHandle::default(),
                parser_binding(&mut probe),
            )
            .expect("activation must succeed");
        table.unregister(first).expect("unregister must succeed");
        assert_eq!(
            table.unregister(first).expect_err("second unregister must be NotFound").kind(),
            io::ErrorKind::NotFound
        );

        let second = table.register(tuple(444)).expect("new reservation must succeed");
        let second_slot = table.slot_for_handle(second).unwrap();
        assert_eq!(first_slot, second_slot);
        assert!(table.slot_for_handle(first).is_none());
        let conn = table.conns[second_slot].as_ref().unwrap();
        assert!(conn.parser.is_none());
        assert!(conn.receiver_waker.is_none());
        assert!(!conn.publication_ready);
        assert!(!conn.terminal_ready);
        assert_eq!(conn.packet_ordinal, 0);
    }

    #[test]
    fn socket_detach_removes_nonowning_capabilities_but_preserves_unregister() {
        let mut table = RawTailTable::new(test_owner(0), &RAW_TAIL_RSS_KEY);
        let handle = table.register(tuple(443)).expect("reservation must succeed");
        let socket_handle = SocketHandle::default();
        let mut probe = ParserProbe::default();
        table
            .activate_parser(
                handle,
                tuple(443),
                socket_handle,
                parser_binding(&mut probe),
            )
            .expect("activation must succeed");
        let slot = table.slot_for_handle(handle).unwrap();
        let wake_count = Arc::new(AtomicUsize::new(0));
        let conn = table.conns[slot].as_mut().unwrap();
        conn.publication_ready = true;
        conn.receiver_waker = Some(Waker::from(Arc::new(CountWake(wake_count.clone()))));

        assert!(table.detach_socket(socket_handle));
        let conn = table.conns[slot].as_ref().unwrap();
        assert!(conn.parser.is_none());
        assert!(conn.socket_handle.is_none());
        assert!(!conn.publication_ready);
        assert_eq!(table.active_count, 0);
        assert_eq!(wake_count.load(Ordering::Relaxed), 1);
        assert!(table.slot_for_handle(handle).is_some());
        table
            .unregister(handle)
            .expect("detached raw-tail handle must still unregister");
    }

    #[test]
    fn owned_mbuf_is_a_drop_guard() {
        assert!(std::mem::needs_drop::<OwnedMbuf>());
    }
}

use crate::packet_info::PacketInfo;
use crate::plugin_registry::PluginRegistry;
use libpcap_tools::{Duration, Flow, FlowID, Packet};
use pcap_parser::data::PacketData;
use pnet_macros_support::packet::Packet as PnetPacket;
use pnet_packet::tcp::{TcpFlags, TcpPacket};
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::num::Wrapping;

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum TcpStatus {
    Closed = 0,
    Listen,
    SynSent,
    SynRcv,
    Established,
    Closing,
    CloseWait,
    FinWait1,
    FinWait2,
    LastAck,
    TimeWait,
}

impl Default for TcpStatus {
    fn default() -> Self {
        TcpStatus::Closed
    }
}

pub struct TcpSegment {
    pub rel_seq: Wrapping<u32>,
    pub rel_ack: Wrapping<u32>,
    pub flags: u16,
    pub data: Vec<u8>,
    pub pcap_index: usize,
}

pub struct TcpPeer {
    /// Initial Seq number (absolute)
    isn: Wrapping<u32>,
    /// Initial Ack number (absolute)
    ian: Wrapping<u32>,
    /// Next Seq number
    next_rel_seq: Wrapping<u32>,
    /// Last acknowledged number
    last_rel_ack: Wrapping<u32>,
    /// Connection state
    status: TcpStatus,
    /// The current list of segments (ordered by rel_seq)
    segments: VecDeque<TcpSegment>,
    /// DEBUG: host address
    addr: IpAddr,
    /// DEBUG: port
    port: u16,
}

impl TcpPeer {
    fn insert_sorted(&mut self, s: TcpSegment) {
        // find index
        let idx = self.segments.iter().enumerate().find_map(|(n, item)| {
            if s.rel_seq < item.rel_seq {
                Some(n)
            } else {
                None
            }
        });
        match idx {
            Some(idx) => self.segments.insert(idx, s),
            None => self.segments.push_back(s),
        }
    }
}

pub struct TcpStream {
    pub client: TcpPeer,
    pub server: TcpPeer,
    pub status: TcpStatus,
    // XXX timestamp of last seen packet
    pub last_seen_ts: Duration,
}

pub struct TcpStreamReassembly {
    pub m: HashMap<FlowID, TcpStream>,

    pub timeout: Duration,
}

impl Default for TcpStreamReassembly {
    fn default() -> Self {
        TcpStreamReassembly {
            m: HashMap::new(),
            timeout: Duration::new(120, 0),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TcpStreamError {
    Anomaly,
    /// Packet received but connection has expired
    Expired,
    HandshakeFailed,
}

impl TcpPeer {
    pub fn new(addr: &IpAddr, port: u16) -> Self {
        TcpPeer {
            isn: Wrapping(0),
            ian: Wrapping(0),
            next_rel_seq: Wrapping(0),
            last_rel_ack: Wrapping(0),
            status: TcpStatus::Closed,
            segments: VecDeque::new(),
            addr: *addr,
            port,
        }
    }
}

impl TcpStream {
    pub fn new(flow: &Flow) -> Self {
        TcpStream {
            client: TcpPeer::new(&flow.five_tuple.src, flow.five_tuple.src_port),
            server: TcpPeer::new(&flow.five_tuple.dst, flow.five_tuple.dst_port),
            status: TcpStatus::Closed,
            last_seen_ts: flow.last_seen,
        }
    }

    pub fn handle_new_connection<'a>(
        &mut self,
        tcp: &'a TcpPacket,
        to_server: bool,
    ) -> Result<(), TcpStreamError> {
        let seq = Wrapping(tcp.get_sequence());
        let ack = Wrapping(tcp.get_acknowledgement());
        let tcp_flags = tcp.get_flags();

        let (mut conn, mut rev_conn) = if to_server {
            (&mut self.client, &mut self.server)
        } else {
            (&mut self.server, &mut self.client)
        };

        match conn.status {
            // Client -- SYN --> Server
            TcpStatus::Closed => {
                if tcp_flags & TcpFlags::RST != 0 {
                    // TODO check if destination.segments must be removed
                    // client sent a RST, this is expected
                    return Ok(());
                }
                // XXX check flags: SYN ?
                if tcp_flags & TcpFlags::SYN == 0 {
                    // not a SYN - usually happens at start of pcap if missed SYN
                    warn!("First packet of a TCP stream is not a SYN");
                    // XXX test is ACK + data, and set established if possible ?
                    return Err(TcpStreamError::Anomaly);
                }
                conn.isn = seq;
                conn.next_rel_seq = Wrapping(1);
                rev_conn.ian = seq;
                self.status = TcpStatus::SynSent;
                conn.status = TcpStatus::SynSent;
                rev_conn.status = TcpStatus::Listen;
            }
            // Server -- SYN+ACK --> Client
            TcpStatus::Listen => {
                if tcp_flags != (TcpFlags::SYN | TcpFlags::ACK) {
                    // XXX ?
                }
                // XXX if plen != 0, add plen to 1 ?
                if ack != rev_conn.isn + Wrapping(1) {
                    warn!("NEW/SYN-ACK: ack number is wrong");
                    return Err(TcpStreamError::HandshakeFailed);
                }
                conn.isn = seq;
                conn.next_rel_seq = Wrapping(1);
                rev_conn.ian = seq;
                rev_conn.last_rel_ack = Wrapping(1);

                conn.status = TcpStatus::SynRcv;
                self.status = TcpStatus::SynRcv;
            }
            // Client -- ACK --> Server
            TcpStatus::SynSent => {
                if tcp_flags != TcpFlags::ACK {
                    // XXX
                    warn!("Not an ACK");
                }
                // TODO check seq, ack
                if ack != rev_conn.isn + Wrapping(1) {
                    warn!("NEW/ACK: ack number is wrong");
                    return Err(TcpStreamError::HandshakeFailed);
                }
                conn.status = TcpStatus::Established;
                rev_conn.status = TcpStatus::Established;
                rev_conn.last_rel_ack = Wrapping(1);
                self.status = TcpStatus::Established;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    pub fn handle_established_connection<'a>(
        &mut self,
        tcp: &'a TcpPacket,
        to_server: bool,
        pinfo: &PacketInfo,
        registry: &PluginRegistry,
    ) -> Result<(), TcpStreamError> {
        let (mut origin, mut destination) = if to_server {
            (&mut self.client, &mut self.server)
        } else {
            (&mut self.server, &mut self.client)
        };

        let rel_seq = Wrapping(tcp.get_sequence()) - origin.isn;
        let rel_ack = Wrapping(tcp.get_acknowledgement()) - destination.isn;
        let tcp_flags = tcp.get_flags();
        let plen = tcp.payload().len();

        trace!("EST: plen={}", plen);
        debug!(
            "    Tcp rel seq {} ack {} next seq {}",
            rel_seq, rel_ack, origin.next_rel_seq
        );

        // TODO check if closing connection

        if tcp_flags & TcpFlags::ACK == 0 {
            warn!("EST/ packet without ACK");
        }

        let segment = TcpSegment {
            rel_seq,
            rel_ack,
            flags: tcp_flags,
            data: tcp.payload().to_vec(), // XXX data cloned here
            pcap_index: pinfo.pcap_index,
        };
        queue_segment(&mut origin, segment);

        debug!("  segments count: {}", origin.segments.len());
        debug!(
            "  PEER segments count (before ACK): {}",
            destination.segments.len()
        );

        // TODO check for close request
        // if tcp_flags & (TcpFlags::FIN | TcpFlags::RST) != 0 {
        //     // XXX
        //     warn!("Requesting end of connection");
        //     self.handle_closing_connection(tcp, to_server);
        // }

        // TODO if there is a ACK, check & send segments on the *other* side
        if tcp_flags & TcpFlags::ACK != 0 {
            send_peer_segments(destination, origin, rel_ack, pinfo, registry);
        }

        // if ack > destination.next_seq {
        //     warn!("EST/data: ack number is wrong (missed packet?)");
        //     warn!("  expected ack 0x{:x}", destination.next_seq);
        //     warn!("  got ack 0x{:x}", ack);
        //     return Ok(Fragment::Incomplete);
        // }
        // if ack < destination.next_seq {
        //     trace!(
        //         "TCP: partially ACKed data (expecting up to ACK {})",
        //         destination.next_seq.wrapping_sub(destination.isn)
        //     );
        // }

        // origin.next_seq = origin.next_seq.wrapping_add(plen as u32);

        debug!(
            "    PEER EST rel next seq {} last_ack {}",
            destination.next_rel_seq, destination.last_rel_ack,
        );

        Ok(())
    }

    fn handle_closing_connection(
        &mut self,
        tcp: &TcpPacket,
        to_server: bool,
        pinfo: &PacketInfo,
        registry: &PluginRegistry,
    ) -> Result<(), TcpStreamError> {
        let (mut origin, destination) = if to_server {
            (&mut self.client, &mut self.server)
        } else {
            (&mut self.server, &mut self.client)
        };

        let tcp_flags = tcp.get_flags();
        let rel_seq = Wrapping(tcp.get_sequence()) - origin.isn;
        let rel_ack = Wrapping(tcp.get_acknowledgement()) - destination.isn;

        if tcp_flags & TcpFlags::ACK != 0 {
            debug!("ACKing segments up to {}", rel_ack);
            send_peer_segments(destination, origin, rel_ack, pinfo, registry);
        }
        if tcp_flags & TcpFlags::RST != 0 {
            // if we get a RST, check the sequence number and remove matching segments
            debug!("RST received. rel_seq: {}", rel_seq);
            debug!(
                "{} remaining (undelivered) segments DESTINATION",
                destination.segments.len()
            );
            for (n, s) in destination.segments.iter().enumerate() {
                debug!("  s[{}]: rel_seq={} plen={}", n, s.rel_seq, s.data.len());
            }
            // remove queued segments up to rel_seq
            destination.segments.retain(|s| s.rel_ack != rel_seq);
            debug!(
                "{} remaining (undelivered) segments DESTINATION after removal",
                destination.segments.len()
            );
            origin.status = TcpStatus::Closed; // XXX except if ACK ?
            return Ok(());
        }

        // queue segment (even if FIN, to get correct seq numbers)
        let rel_seq = Wrapping(tcp.get_sequence()) - origin.isn;
        let rel_ack = Wrapping(tcp.get_acknowledgement()) - destination.isn;
        let segment = TcpSegment {
            rel_seq,
            rel_ack,
            flags: tcp_flags,
            data: tcp.payload().to_vec(), // XXX data cloned here
            pcap_index: pinfo.pcap_index,
        };
        queue_segment(&mut origin, segment);

        // if tcp_flags & TcpFlags::FIN != 0 {
        //     warn!("origin next seq was {}", origin.next_rel_seq.0);
        //     origin.next_rel_seq += Wrapping(1);
        // }

        match origin.status {
            TcpStatus::Established => {
                if tcp_flags & TcpFlags::FIN == 0 {
                    warn!("Not a FIN");
                }
                origin.status = TcpStatus::FinWait1;
            }
            _ => {
                warn!(
                    "Unhandled closing transition: origin host {} status {:?}",
                    origin.addr, origin.status
                );
                warn!(
                    "    dest host {} status {:?}",
                    destination.addr, destination.status
                );
            }
        }

        debug!(
            "TCP connection closing, {} remaining (undelivered) segments",
            origin.segments.len()
        );
        // DEBUG
        for (n, s) in origin.segments.iter().enumerate() {
            debug!("  s[{}]: plen={}", n, s.data.len());
        }

        // TODO what now?

        if origin.segments.is_empty() {
            return Ok(());
        }

        Ok(())
    }

    // force expiration (for ex after timeout) of this stream
    fn expire(&mut self) {
        self.client.status = TcpStatus::Closed;
        self.server.status = TcpStatus::Closed;
    }
} // TcpStream

fn queue_segment(peer: &mut TcpPeer, segment: TcpSegment) {
    // only store segments with data
    if segment.data.is_empty() && segment.flags & TcpFlags::FIN == 0 {
        return;
    }
    // TODO check & merge segments
    if let Some(s) = peer.segments.front_mut() {
        let next_seq = s.rel_seq + Wrapping(s.data.len() as u32);
        match segment.rel_seq.cmp(&next_seq) {
            Ordering::Equal => {
                // XXX do nothing, simply queue segment
                // // simple case: merge segment
                // trace!(
                //     "Merging segments (seq {} and {})",
                //     s.rel_seq,
                //     segment.rel_seq
                // );
                // s.data.extend_from_slice(&segment.data);
                // s.rel_ack = segment.rel_ack;
                // // XXX pcap_index should be a list (and append to it)
                // // TODO check next segment in queue to test if a hole was filled
                // return;
            }
            Ordering::Greater => {
                // we have a hole
                warn!("Missing segment");
            }
            Ordering::Less => {
                // overlap
                warn!("Segment with overlap");
            }
        }
    }
    trace!("Pushing segment");
    peer.insert_sorted(segment);
}

fn send_peer_segments(
    origin: &mut TcpPeer,
    destination: &mut TcpPeer,
    rel_ack: Wrapping<u32>,
    pinfo: &PacketInfo,
    registry: &PluginRegistry,
) {
    debug!(
        "Trying to send segments for {}:{} up to {} (last ack: {})",
        origin.addr, origin.port, rel_ack, origin.last_rel_ack
    );
    if rel_ack == origin.last_rel_ack {
        trace!("re-acking last data, doing nothing");
        return;
    }
    if rel_ack < origin.last_rel_ack {
        warn!("ack < last_ack");
    }

    // DEBUG
    for (n, s) in origin.segments.iter().enumerate() {
        debug!("  s[{}]: rel_seq={} plen={}", n, s.rel_seq, s.data.len());
    }

    // TODO check consistency of segment ACK numbers + order and/or missing fragments and/or overlap

    #[allow(clippy::while_let_loop)]
    loop {
        if let Some(segment) = origin.segments.front() {
            debug!(
                "segment: rel_seq={}  len={}",
                segment.rel_seq,
                segment.data.len()
            );
            debug!(
                "  origin.next_rel_seq {} ack {}",
                origin.next_rel_seq, rel_ack
            );
            if origin.next_rel_seq > rel_ack {
                warn!("next_seq > ack - partial ACK ?");
                break;
            }
            if rel_ack == segment.rel_seq {
                trace!("got a segment, not yet acked: not sending");
                break;
            }
        } else {
            // warn!("No data segment");
            break;
        }

        let mut segment = match origin.segments.pop_front() {
            Some(s) => s,
            None => return,
        };

        if rel_ack < segment.rel_seq {
            warn!("TCP ACK of unseen segment");
            continue;
        }

        if rel_ack < segment.rel_seq + Wrapping(segment.data.len() as u32) {
            // warn!("ACK lower then seq + segment size - SACK?");
            debug!("ACK for part of buffer");
            // split data and insert new dummy segment
            debug!("rel_ack {} segment.rel_seq {}", rel_ack, segment.rel_seq);
            debug!("segment data len {}", segment.data.len());
            let remaining = segment
                .data
                .split_off((rel_ack - segment.rel_seq).0 as usize);
            let new_segment = TcpSegment {
                data: remaining,
                rel_ack,
                ..segment
            };
            debug!(
                "insert new segment from {} len {}",
                new_segment.rel_ack,
                new_segment.data.len()
            );
            origin.insert_sorted(new_segment);
        }

        send_single_segment(origin, destination, segment, pinfo, registry);
    }

    if origin.next_rel_seq != rel_ack {
        // missed segments, or mayber received FIN ?
        warn!(
            "TCP ACKed unseen segment next_seq {} != ack {} (Missed segments?)",
            origin.next_rel_seq, rel_ack
        );
        // TODO notify upper layer for missing data
    }

    origin.last_rel_ack = rel_ack;
}

fn send_single_segment(
    origin: &mut TcpPeer,
    _destination: &mut TcpPeer,
    segment: TcpSegment,
    pinfo: &PacketInfo,
    registry: &PluginRegistry,
) {
    trace!(
        "Sending segment from {}:{} (plen={}, pcap_index={})",
        origin.addr,
        origin.port,
        segment.data.len(),
        segment.pcap_index,
    );

    if !segment.data.is_empty() {
        // send ACKed segments for remote connection side
        let five_tuple = &pinfo.five_tuple.get_reverse();
        let to_server = !pinfo.to_server;
        let pinfo = PacketInfo {
            five_tuple,
            to_server,
            l4_payload: Some(&segment.data),
            ..*pinfo
        };

        // XXX build a dummy packet
        let packet = Packet {
            interface: 0,
            caplen: 0,
            origlen: 0,
            ts: Duration::new(0, 0),
            data: PacketData::L4(pinfo.l4_type, &segment.data),
            pcap_index: segment.pcap_index,
        };
        // let start = ::std::time::Instant::now();
        registry.run_plugins_transport(pinfo.l4_type, &packet, &pinfo);
        // let elapsed = start.elapsed();
        // debug!("Time to run l4 plugins: {}.{}", elapsed.as_secs(), elapsed.as_millis());

        origin.next_rel_seq += Wrapping(segment.data.len() as u32);
    }

    if segment.flags & TcpFlags::FIN != 0 {
        trace!("Segment has FIN");
        origin.next_rel_seq += Wrapping(1);
    }

    if segment.flags & TcpFlags::RST != 0 {
        trace!("Segment has RST");
        // origin.status = TcpStatus::FinWait1;
        // XXX destination.status
        // XXX stream.status
    }
}

impl TcpStreamReassembly {
    pub(crate) fn update(
        &mut self,
        flow: &Flow,
        tcp: &TcpPacket,
        to_server: bool,
        pinfo: &PacketInfo,
        registry: &PluginRegistry,
    ) -> Result<(), TcpStreamError> {
        trace!("5-t: {}", flow.five_tuple);
        trace!("  flow id: {:x}", flow.flow_id);
        trace!(
            "  seq: {:x}  ack {:x}",
            tcp.get_sequence(),
            tcp.get_acknowledgement()
        );

        let mut stream = self
            .m
            .entry(flow.flow_id)
            .or_insert_with(|| TcpStream::new(flow));
        trace!("stream state: {:?}", stream.status);
        trace!("to_server: {}", to_server);

        // check time delay with previous packet before updating
        if flow.last_seen - stream.last_seen_ts > self.timeout {
            warn!("TCP stream received packet after timeout");
            stream.expire();
            return Err(TcpStreamError::Expired);
        }
        stream.last_seen_ts = flow.last_seen;

        let (origin, _destination) = if to_server {
            (&stream.client, &stream.server)
        } else {
            (&stream.server, &stream.client)
        };

        trace!(
            "origin: {}:{} status {:?}",
            origin.addr,
            origin.port,
            origin.status
        );
        debug_print_tcp_flags(tcp.get_flags());

        match origin.status {
            TcpStatus::Closed | TcpStatus::Listen | TcpStatus::SynSent | TcpStatus::SynRcv => {
                stream.handle_new_connection(&tcp, to_server)
            }
            TcpStatus::Established => {
                // check for close request
                if tcp.get_flags() & (TcpFlags::FIN | TcpFlags::RST) != 0 {
                    trace!("Requesting end of connection");
                    stream.handle_closing_connection(tcp, to_server, pinfo, registry)
                } else {
                    stream.handle_established_connection(tcp, to_server, pinfo, registry)
                }
            }
            _ => stream.handle_closing_connection(tcp, to_server, pinfo, registry),
        }
    }
    pub(crate) fn check_expired_connections(&mut self, now: Duration) {
        for (flow_id, stream) in self.m.iter_mut() {
            if now < stream.last_seen_ts {
                warn!(
                    "stream.last_seen_ts is in the future for flow id {:x}",
                    flow_id
                );
                continue;
            }
            if now - stream.last_seen_ts > self.timeout {
                warn!("TCP stream timeout reached for flow {:x}", flow_id);
                stream.expire();
            }
        }
    }
}

pub(crate) fn finalize_tcp_streams(analyzer: &mut crate::analyzer::Analyzer) {
    warn!("expiring all TCP connections");
    for (flow_id, stream) in analyzer.tcp_defrag.m.iter() {
        // TODO do we have anything to do?
        if let Some(flow) = analyzer.flows.get_flow(*flow_id) {
            debug!("  flow: {:?}", flow);
        }
    }
    analyzer.tcp_defrag.m.clear();
}

fn debug_print_tcp_flags(tcp_flags: u16) {
    let mut s = String::from("tcp_flags: [");
    if tcp_flags & TcpFlags::SYN != 0 {
        s += "S"
    }
    if tcp_flags & TcpFlags::FIN != 0 {
        s += "F"
    }
    if tcp_flags & TcpFlags::ACK != 0 {
        s += "A"
    }
    if tcp_flags & TcpFlags::RST != 0 {
        s += "R"
    }
    if tcp_flags & TcpFlags::URG != 0 {
        s += "U"
    }
    if tcp_flags & TcpFlags::PSH != 0 {
        s += "P"
    }
    s += "]";
    debug!("{}", s);
}

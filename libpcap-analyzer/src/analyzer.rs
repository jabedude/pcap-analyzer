use crate::erspan::ErspanPacket;
use crate::flow_map::FlowMap;
use crate::ip_defrag::{DefragEngine, Fragment, IPDefragEngine};
use crate::layers::LinkLayerType;
use crate::packet_info::PacketInfo;
use crate::plugin::*;
use crate::plugin_registry::*;
use crate::ppp::{PppPacket, PppProtocolTypes};
use crate::pppoe::PppoeSessionPacket;
use crate::vxlan::*;
use libpcap_tools::*;

use pcap_parser::{data::PacketData, Linktype};
use std::cmp::min;
use std::net::IpAddr;
use std::ops::DerefMut;
use std::sync::Arc;

use pnet_packet::ethernet::{EtherType, EtherTypes, EthernetPacket};
use pnet_packet::gre::GrePacket;
use pnet_packet::icmp::IcmpPacket;
use pnet_packet::icmpv6::Icmpv6Packet;
use pnet_packet::ip::{IpNextHeaderProtocol, IpNextHeaderProtocols};
use pnet_packet::ipv4::{Ipv4Flags, Ipv4Packet};
use pnet_packet::ipv6::{ExtensionPacket, FragmentPacket, Ipv6Packet};
use pnet_packet::tcp::TcpPacket;
use pnet_packet::udp::UdpPacket;
use pnet_packet::vlan::VlanPacket;
use pnet_packet::{Packet as PnetPacket, PacketSize};

#[derive(Clone, Debug, Default)]
pub struct L3Info {
    /// Layer 4 protocol (e.g TCP, UDP, ICMP)
    pub l4_proto: u8,
    pub three_tuple: ThreeTuple,
}

/// Pcap/Pcap-ng analyzer
///
/// Read input pcap/pcap-ng data, parse it and call plugin callbacks
/// for each ISO layer (L2 if available, L3 and L4).
/// Flows are created for L4 sessions. Events are sent when plugins
/// are created or destroyed.
///
/// The number of worker threads can be configured from the `num_threads`
/// configuration variable. By default, it is 0 (auto-detect the number
/// of cores and create the same number of threads).
///
/// All callbacks for a single ISO layer will be called concurrently before
/// calling the next level callbacks.
pub struct Analyzer {
    pub(crate) registry: Arc<PluginRegistry>,

    pub(crate) flows: FlowMap,

    ipv4_defrag: Box<dyn DefragEngine>,
    ipv6_defrag: Box<dyn DefragEngine>,

    do_checksums: bool,
}

impl Analyzer {
    pub fn new(registry: Arc<PluginRegistry>, config: &Config) -> Analyzer {
        let do_checksums = config.get_bool("do_checksums").unwrap_or(true);
        Analyzer {
            registry,
            flows: FlowMap::default(),
            ipv4_defrag: Box::new(IPDefragEngine::new()),
            ipv6_defrag: Box::new(IPDefragEngine::new()),
            do_checksums,
        }
    }

    #[inline]
    fn handle_l2(&mut self, packet: &Packet, ctx: &ParseContext, data: &[u8]) -> Result<(), Error> {
        handle_l2(packet, ctx, data, self)
    }
}

pub(crate) fn handle_l2(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l2 (idx={})", ctx.pcap_index);

    // resize slice to remove padding
    let datalen = min(packet.caplen as usize, data.len());
    let data = &data[..datalen];

    // let start = ::std::time::Instant::now();
    run_plugins_v2_physical(packet, ctx, data, analyzer)?;
    // let elapsed = start.elapsed();
    // debug!("Time to run l2 plugins: {}.{}", elapsed.as_secs(), elapsed.as_millis());

    match EthernetPacket::new(data) {
        Some(eth) => {
            // debug!("    source: {}", eth.get_source());
            // debug!("    dest  : {}", eth.get_destination());
            let dest = eth.get_destination();
            if dest.is_multicast() {
                match &data[..6] {
                    [0x01, 0x00, 0x0c, 0xcc, 0xcc, 0xcc] => {
                        info!("Cisco CDP/VTP/UDLD - ignoring");
                        // the 'ethertype' field is used for length
                        return Ok(());
                    }
                    [0x01, 0x00, 0x0c, 0xcd, 0xcd, 0xd0] => {
                        info!("Cisco Multicast address - ignoring");
                        return Ok(());
                    }
                    _ => {
                        info!("Ethernet broadcast (unknown type) (idx={})", ctx.pcap_index);
                    }
                }
            }
            let ethertype = eth.get_ethertype();
            let payload = eth.payload();
            trace!("    ethertype: 0x{:x}", ethertype.0);
            run_plugins_v2_link(packet, ctx, LinkLayerType::Ethernet, payload, analyzer)?;
            handle_l3(&packet, &ctx, payload, ethertype, analyzer)
        }
        None => {
            // packet too small to be ethernet
            Ok(())
        }
    }
}

pub(crate) fn handle_l3(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    if data.is_empty() {
        return Ok(());
    }

    // see https://www.iana.org/assignments/ieee-802-numbers/ieee-802-numbers.xhtml
    match ethertype {
        EtherTypes::Ipv4 => handle_l3_ipv4(packet, ctx, data, ethertype, analyzer),
        EtherTypes::Ipv6 => handle_l3_ipv6(packet, ctx, data, ethertype, analyzer),
        EtherTypes::Vlan => handle_l3_vlan_801q(packet, ctx, data, ethertype, analyzer),
        // ignore ARP packets
        EtherTypes::Arp => Ok(()),
        // 0x880b: PPP (rfc7042)
        EtherType(0x880b) => handle_l3_ppp(packet, ctx, data, ethertype, analyzer),
        EtherType(0x88be) => handle_l3_erspan(packet, ctx, data, ethertype, analyzer),
        EtherTypes::PppoeSession => handle_l3_pppoesession(packet, ctx, data, ethertype, analyzer),

        e => {
            warn!(
                "Unsupported ethertype {} (0x{:x}) (idx={})",
                e, e.0, ctx.pcap_index
            );
            Ok(())
        }
    }
}

fn handle_l3_ipv4(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_ipv4 (idx={})", ctx.pcap_index);
    let ipv4 = Ipv4Packet::new(data).ok_or("Could not build IPv4 packet from data")?;
    // eprintln!("ABORT pkt {:?}", ipv4);
    let orig_len = data.len();

    let ip_len = ipv4.get_total_length() as usize;

    // remove padding
    let (data, ipv4) = {
        if ip_len < data.len() && ip_len > 0 {
            let d = &data[..ip_len];
            let ipv4 = Ipv4Packet::new(d).ok_or("Could not build IPv4 packet from data")?;
            (d, ipv4)
        } else {
            (data, ipv4)
        }
    };

    let t3 = ThreeTuple {
        proto: ethertype.0,
        src: IpAddr::V4(ipv4.get_source()),
        dst: IpAddr::V4(ipv4.get_destination()),
    };
    let l4_proto = ipv4.get_next_level_protocol().0;

    if analyzer.do_checksums {
        let cksum = ::pnet_packet::ipv4::checksum(&ipv4);
        if cksum != ipv4.get_checksum() {
            warn!("IPv4: invalid checksum");
        }
    }

    run_plugins_v2_network(packet, ctx, data, &t3, l4_proto, analyzer)?;

    // if get_total_length is 0, assume TSO offloading and no padding
    let payload = if ip_len == 0 {
        warn!(
            "IPv4: packet reported length is 0. Assuming TSO (idx={})",
            ctx.pcap_index
        );
        // the payload() function from pnet will fail
        let start = ipv4.get_header_length() as usize * 4;
        if start > data.len() {
            warn!("IPv4: ip_len == 0 and ipv4.get_header_length is invalid!");
            return Ok(());
        }
        &data[start..]
    } else {
        ipv4.payload()
    };

    // check IP fragmentation before calling handle_l4
    let frag_offset = (ipv4.get_fragment_offset() * 8) as usize;
    let more_fragments = ipv4.get_flags() & Ipv4Flags::MoreFragments != 0;
    let defrag = analyzer.ipv4_defrag.update(
        ipv4.get_identification().into(),
        frag_offset,
        more_fragments,
        payload,
    );
    let payload = match defrag {
        Fragment::NoFrag(d) => {
            debug_assert!(d.len() < orig_len);
            d
        }
        Fragment::Complete(ref v) => {
            warn!("IPv4 defrag done, using defrag buffer len={}", v.len());
            &v
        }
        Fragment::Incomplete => {
            debug!("IPv4 defragmentation incomplete");
            return Ok(());
        }
        Fragment::Error => {
            warn!("IPv4 defragmentation error");
            return Ok(());
        }
    };

    let l3_info = L3Info {
        three_tuple: t3,
        l4_proto,
    };
    handle_l3_common(packet, ctx, payload, &l3_info, analyzer)
}

fn is_ipv6_opt(opt: IpNextHeaderProtocol) -> bool {
    match opt {
        IpNextHeaderProtocols::Hopopt
        | IpNextHeaderProtocols::Ipv6Opts
        | IpNextHeaderProtocols::Ipv6Route
        | IpNextHeaderProtocols::Ipv6Frag
        | IpNextHeaderProtocols::Esp
        | IpNextHeaderProtocols::Ah
        | IpNextHeaderProtocols::MobilityHeader => true,
        _ => false,
    }
}

fn handle_l3_ipv6(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_ipv6 (idx={})", ctx.pcap_index);
    let ipv6 = Ipv6Packet::new(data).ok_or("Could not build IPv6 packet from data")?;

    let mut payload = ipv6.payload();
    let mut l4_proto = ipv6.get_next_header();

    if payload.is_empty() {
        // jumbogram ? (rfc2675)
        trace!("IPv6 length is 0. Jumbogram?");
        if data.len() >= 40 {
            payload = &data[40..];
        } else {
            warn!(
                "IPv6 length is 0, but frame is too short for an IPv6 header (idx={})",
                ctx.pcap_index
            );
            return Ok(());
        }
    }

    // XXX remove padding ?

    let mut extensions = Vec::new();
    let mut frag_ext = None;

    // skip all extensions (keep them ?)
    while is_ipv6_opt(l4_proto) {
        let ext = ExtensionPacket::new(payload)
            .ok_or("Could not build IPv6 Extension packet from payload")?;
        let next_header = ext.get_next_header();
        if l4_proto == IpNextHeaderProtocols::Ipv6Frag {
            if frag_ext.is_some() {
                warn!("multiple IPv6Frag extensions idx={}", ctx.pcap_index);
                return Ok(());
            }
            frag_ext = FragmentPacket::new(payload);
        }
        let offset = ext.packet_size();
        extensions.push((l4_proto, ext));
        l4_proto = next_header;
        payload = &payload[offset..];
    }

    let t3 = ThreeTuple {
        proto: ethertype.0,
        src: IpAddr::V6(ipv6.get_source()),
        dst: IpAddr::V6(ipv6.get_destination()),
    };

    run_plugins_v2_network(packet, ctx, payload, &t3, l4_proto.0, analyzer)?;

    if l4_proto == IpNextHeaderProtocols::Ipv6NoNxt {
        // usually the case for IPv6 mobility
        // XXX header data could be inspected?
        trace!("No next header");
        if !payload.is_empty() {
            warn!("No next header, but data is present (len={})", payload.len());
        }
        return Ok(());
    }

    let l3_info = L3Info {
        three_tuple: t3,
        l4_proto: l4_proto.0,
    };

    if let Some(frag_info) = frag_ext {
        handle_l4_ipv6frag(
            packet, ctx, &frag_info, payload, &l3_info, l4_proto, analyzer,
        )
    } else {
        handle_l3_common(packet, ctx, payload, &l3_info, analyzer)
    }
}

fn handle_l3_vlan_801q(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    _ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_vlan_801q (idx={})", ctx.pcap_index);
    let vlan = VlanPacket::new(data).ok_or("Could not build 802.1Q Vlan packet from data")?;
    let next_ethertype = vlan.get_ethertype();
    trace!("    802.1q: VLAN id={}", vlan.get_vlan_identifier());

    handle_l3(&packet, &ctx, vlan.payload(), next_ethertype, analyzer)
}

fn handle_l3_erspan(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    _ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_erspan (idx={})", ctx.pcap_index);
    let erspan = ErspanPacket::new(data).ok_or("Could not build Erspan packet from data")?;
    trace!(
        "    erspan: VLAN id={} span ID={}",
        erspan.get_vlan(),
        erspan.get_span_id()
    );
    handle_l2(packet, ctx, erspan.payload(), analyzer)
}

fn handle_l3_pppoesession(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_pppoesession (idx={})", ctx.pcap_index);
    let session =
        PppoeSessionPacket::new(data).ok_or("Could not build PppoeSession packet from data")?;
    trace!(
        "    pppoesession: version={} type={} code={}",
        session.get_version(),
        session.get_type(),
        session.get_code(),
    );
    let ppp_data = session.payload();
    handle_l3_ppp(packet, ctx, ppp_data, ethertype, analyzer)
}

fn handle_l3_ppp(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    ethertype: EtherType,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_ppp (idx={})", ctx.pcap_index);
    let ppp = PppPacket::new(data).ok_or("Could not build Ppp packet from data")?;
    let proto = ppp.get_protocol();
    let payload = ppp.payload();
    trace!("    ppp: protocol=0x{:02x}", proto.0,);
    match proto {
        PppProtocolTypes::Ipv4 => handle_l3_ipv4(packet, ctx, payload, ethertype, analyzer),
        PppProtocolTypes::Ipv6 => handle_l3_ipv6(packet, ctx, payload, ethertype, analyzer),
        _ => {
            warn!("Unsupported PPP protocol 0x{:02x}", proto.0);
            Ok(())
        }
    }
}

fn handle_l3_common(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    match IpNextHeaderProtocol(l3_info.l4_proto) {
        IpNextHeaderProtocols::Tcp => handle_l4_tcp(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Udp => handle_l4_udp(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Icmp => handle_l4_icmp(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Icmpv6 => handle_l4_icmpv6(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Esp => handle_l4_generic(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Gre => handle_l4_gre(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Ipv4 => handle_l3(packet, ctx, data, EtherTypes::Ipv4, analyzer),
        IpNextHeaderProtocols::Ipv6 => handle_l3(packet, ctx, data, EtherTypes::Ipv6, analyzer),
        p => {
            warn!("Unsupported L4 proto {}", p);
            handle_l4_generic(packet, ctx, data, &l3_info, analyzer)
        }
    }
}

fn handle_l4_tcp(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l4_tcp (idx={})", ctx.pcap_index);
    trace!("    l4_data len: {}", data.len());
    let tcp = TcpPacket::new(data).ok_or("Could not build TCP packet from data")?;

    // XXX handle TCP defrag
    let l4_payload = Some(tcp.payload());
    let src_port = tcp.get_source();
    let dst_port = tcp.get_destination();

    handle_l4_common(
        packet, ctx, data, l3_info, src_port, dst_port, l4_payload, analyzer,
    )
}

fn handle_l4_udp(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l4_udp (idx={})", ctx.pcap_index);
    trace!("    l4_data len: {}", data.len());
    let udp = UdpPacket::new(data).ok_or("Could not build UDP packet from data")?;

    let l4_payload = Some(udp.payload());
    let src_port = udp.get_source();
    let dst_port = udp.get_destination();

    // if sport/dport == 4789, this could be VXLAN
    // XXX l4 plugins will not be called
    if src_port == 4789 || dst_port == 4789 {
        return handle_l4_vxlan(packet, ctx, data, l3_info, udp.payload(), analyzer);
    }

    handle_l4_common(
        packet, ctx, data, l3_info, src_port, dst_port, l4_payload, analyzer,
    )
}

fn handle_l4_icmp(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l4_icmp (idx={})", ctx.pcap_index);
    let icmp = IcmpPacket::new(data).ok_or("Could not build ICMP packet from data")?;
    trace!(
        "ICMP type={:?} code={:?}",
        icmp.get_icmp_type(),
        icmp.get_icmp_code()
    );

    let l4_payload = Some(icmp.payload());
    let src_port = u16::from(icmp.get_icmp_type().0);
    let dst_port = u16::from(icmp.get_icmp_code().0);

    if analyzer.do_checksums {
        let cksum = ::pnet_packet::icmp::checksum(&icmp);
        if cksum != icmp.get_checksum() {
            warn!("ICMP: invalid checksum");
        }
    }

    handle_l4_common(
        packet, ctx, data, l3_info, src_port, dst_port, l4_payload, analyzer,
    )
}

fn handle_l4_icmpv6(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l4_icmpv6 (idx={})", ctx.pcap_index);
    let icmpv6 = Icmpv6Packet::new(data).ok_or("Could not build ICMPv6 packet from data")?;
    trace!(
        "ICMPv6 type={:?} code={:?}",
        icmpv6.get_icmpv6_type(),
        icmpv6.get_icmpv6_code()
    );

    let l4_payload = Some(icmpv6.payload());
    let src_port = 0;
    let dst_port = 0;

    if let (IpAddr::V6(src), IpAddr::V6(dst)) = (l3_info.three_tuple.src, l3_info.three_tuple.dst) {
        let cksum = ::pnet_packet::icmpv6::checksum(&icmpv6, &src, &dst);
        if cksum != icmpv6.get_checksum() {
            warn!("ICMPv6: invalid checksum");
        }
    }

    handle_l4_common(
        packet, ctx, data, l3_info, src_port, dst_port, l4_payload, analyzer,
    )
}

fn handle_l4_gre(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    _l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l4_gre (idx={})", ctx.pcap_index);
    let l3_data = data;

    let gre = GrePacket::new(l3_data).ok_or("Could not build GRE packet from data")?;

    let next_proto = gre.get_protocol_type();
    // XXX can panic: 'Source routed GRE packets not supported' in gre_routing_length()
    // if gre.get_routing_present() != 1 {
    //     warn!("Source routed GRE packets not supported");
    //     return Ok(());
    // }
    let data = if next_proto == 0x880b {
        // PPTP GRE is slightly different, and pnet_packet offset is wrong
        // See https://en.wikipedia.org/wiki/Generic_Routing_Encapsulation
        let mut offset = 8;
        if gre.get_sequence_present() != 0 {
            offset += 4;
        }
        if l3_data[1] >> 7 != 0 {
            // there is an acknowledge number
            offset += 4;
        }
        debug_assert!(offset <= l3_data.len());
        &l3_data[offset..]
    } else {
        gre.payload()
    };
    trace!("GRE: type=0x{:x}", next_proto);

    handle_l3(packet, ctx, data, EtherType(next_proto), analyzer)
}

fn handle_l4_vxlan(
    packet: &Packet,
    ctx: &ParseContext,
    _data: &[u8],
    _l3_info: &L3Info,
    l4_data: &[u8],
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l4_vxlan (idx={})", ctx.pcap_index);
    let vxlan = VxlanPacket::new(l4_data).ok_or("Could not build Vxlan packet from data")?;
    let payload = vxlan.payload();

    trace!("    Vxlan: VLAN id={}", vxlan.get_vlan_identifier());

    handle_l2(packet, ctx, payload, analyzer)
}

fn handle_l4_ipv6frag(
    packet: &Packet,
    ctx: &ParseContext,
    frag_info: &FragmentPacket,
    data: &[u8],
    l3_info: &L3Info,
    l4_proto: IpNextHeaderProtocol,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!("handle_l3_ipv6frag (idx={})", ctx.pcap_index);
    let frag_offset = frag_info.get_fragment_offset() as usize;
    let frag_id = frag_info.get_id();
    let last_fragment = frag_info.is_last_fragment();
    trace!(
        "IPv6 Fragment frag_offset={} id={} last_fragment={}",
        frag_offset,
        frag_id,
        last_fragment
    );

    let defrag = {
        // check IP fragmentation before calling handle_l4
        let more_fragments = !last_fragment;
        analyzer
            .ipv6_defrag
            .update(frag_id, frag_offset, more_fragments, data)
    };
    let data = match defrag {
        Fragment::NoFrag(d) => d,
        Fragment::Complete(ref v) => {
            warn!(
                "IPv6Fragment defrag done, using defrag buffer len={}",
                v.len()
            );
            &v
        }
        Fragment::Incomplete => {
            trace!("IPv6Fragment defragmentation incomplete");
            return Ok(());
        }
        Fragment::Error => {
            warn!("IPv6Fragment defragmentation error");
            return Ok(());
        }
    };

    match l4_proto {
        IpNextHeaderProtocols::Tcp => handle_l4_tcp(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Udp => handle_l4_udp(packet, ctx, data, &l3_info, analyzer),
        IpNextHeaderProtocols::Icmp => handle_l4_icmp(packet, ctx, data, &l3_info, analyzer),
        _ => {
            warn!("IPv6Fragment: Unsupported L4 proto {}", l4_proto);
            handle_l4_generic(packet, ctx, data, &l3_info, analyzer)
        }
    }
}

fn handle_l4_generic(
    packet: &Packet,
    ctx: &ParseContext,
    data: &[u8],
    l3_info: &L3Info,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    trace!(
        "handle_l4_generic (idx={}, l4_proto={})",
        ctx.pcap_index,
        l3_info.three_tuple.proto
    );
    // in generic function, we don't know how to get l4_payload
    let l4_payload = None;
    let src_port = 0;
    let dst_port = 0;

    handle_l4_common(
        packet, ctx, data, l3_info, src_port, dst_port, l4_payload, analyzer,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_l4_common(
    packet: &Packet,
    ctx: &ParseContext,
    l4_data: &[u8],
    l3_info: &L3Info,
    src_port: u16,
    dst_port: u16,
    l4_payload: Option<&[u8]>,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    let five_tuple =
        FiveTuple::from_three_tuple(&l3_info.three_tuple, src_port, dst_port, l3_info.l4_proto);
    trace!("5t: {}", five_tuple);
    let now = packet.ts;

    let flow_id = {
        // flows modification section
        let flows = &mut analyzer.flows;
        // lookup flow
        let flow_id = match flows.lookup_flow(&five_tuple) {
            Some(id) => id,
            None => {
                let flow = Flow::new(&five_tuple, packet.ts.secs, packet.ts.micros);
                gen_event_new_flow(&flow, &analyzer.registry);
                flows.insert_flow(five_tuple.clone(), flow)
            }
        };

        // update flow
        flows.entry(flow_id).and_modify(|flow| {
            flow.flow_id = flow_id;
            flow.last_seen = now;
        });
        flow_id
    };

    // get a read-only reference to flow
    let flow = analyzer
        .flows
        .get_flow(flow_id)
        .expect("could not get flow from ID")
        .clone(); // clone because run_plugins_v2_transport borrows analyzer

    let to_server = flow.five_tuple == five_tuple;

    let pinfo = PacketInfo {
        five_tuple: &five_tuple,
        to_server,
        l3_type: l3_info.three_tuple.proto,
        l4_data,
        l4_type: five_tuple.proto,
        l4_payload,
        flow: Some(&flow),
        pcap_index: ctx.pcap_index,
    };
    // let start = ::std::time::Instant::now();
    run_plugins_v2_transport(packet, ctx, &pinfo, analyzer)?;
    // let elapsed = start.elapsed();
    // debug!("Time to run l4 plugins: {}.{}", elapsed.as_secs(), elapsed.as_millis());

    // XXX do other stuff

    // XXX check session expiration
    // const FLOW_EXPIRATION: u32 = 100;
    // for (flow_id, flow) in self.flows.iter() {
    //     if (now - flow.last_seen).secs > FLOW_EXPIRATION {
    //         warn!(
    //             "Flow {} candidate for expiration (delay: {} secs)",
    //             flow_id,
    //             (now - flow.last_seen).secs
    //         );
    //     }
    // }

    Ok(())
}

fn run_plugins_v2<'i, F>(
    packet: &Packet,
    ctx: &ParseContext,
    layer: u8,
    layer_filter: u16,
    cb: F,
    analyzer: &mut Analyzer,
) -> Result<(), Error>
where
    F: for<'p> Fn(&'p mut dyn Plugin) -> PluginResult<'i>,
{
    trace!(
        "running plugins for layer={} filter=0x{:04x}",
        layer,
        layer_filter
    );
    // clone the registry (which is an Arc)
    // so analyzer is not borrowed for the plugins loop
    let registry = analyzer.registry.clone();
    let empty_vec = vec![];
    // get plugins for this specific filter
    let l1 = registry
        .get_plugins_for_layer(layer, layer_filter)
        .unwrap_or(&empty_vec)
        .as_slice();
    // get catch-all plugins (filter == 0)
    let l2 = registry
        .get_plugins_for_layer(layer, 0)
        .unwrap_or(&empty_vec)
        .as_slice();
    for plugin in l1.iter().chain(l2) {
        let r = {
            // limit duration of lock to vallback
            let mut p = plugin.lock().expect("locking plugin failed (recursion ?)");
            cb(p.deref_mut())
        };
        match r {
            PluginResult::None => continue,
            PluginResult::Error(e) => {
                // XXX ignore error in plugins ? just log ?
                warn!("Plugin returned error {:?}", e);
                continue;
            }
            PluginResult::L2(e, payload) => {
                handle_l3(packet, ctx, payload, EtherType(e), analyzer)?
            }
            PluginResult::L3(l3, payload) => handle_l3_common(packet, ctx, payload, &l3, analyzer)?,
            PluginResult::L4(t5, payload) => {
                let l3_info = L3Info::default(); // XXX
                handle_l4_common(
                    packet,
                    ctx,
                    &[],
                    &l3_info,
                    t5.src_port,
                    t5.dst_port,
                    Some(payload),
                    analyzer,
                )?;
            }
        }
    }
    Ok(())
}

/// Run plugins attached to the physical layer
pub(crate) fn run_plugins_v2_physical<'a>(
    packet: &Packet,
    ctx: &ParseContext,
    data: &'a [u8],
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    let cb = move |p: &mut dyn Plugin| p.handle_layer_physical(packet, data);
    let layer = 1;
    let layer_filter = 0;
    run_plugins_v2(packet, ctx, layer, layer_filter, cb, analyzer)
}

/// Run plugins attached to the link layer (ethernet, etc.)
pub(crate) fn run_plugins_v2_link<'a>(
    packet: &Packet,
    ctx: &ParseContext,
    linktype: LinkLayerType,
    l2_payload: &'a [u8],
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    let cb = move |p: &mut dyn Plugin| p.handle_layer_link(packet, linktype as u16, l2_payload);
    let layer = 2;
    let layer_filter = linktype as u16;
    run_plugins_v2(packet, ctx, layer, layer_filter, cb, analyzer)
}

/// Run plugins attached to the network layer (IPv4, IPv6, Arp, IPsec, etc.)
fn run_plugins_v2_network<'a>(
    packet: &Packet,
    ctx: &ParseContext,
    l3_payload: &'a [u8],
    three_tuple: &ThreeTuple,
    l4_proto: u8,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    let cb =
        move |p: &mut dyn Plugin| p.handle_layer_network(packet, l3_payload, three_tuple, l4_proto);
    let layer = 3;
    let layer_filter = three_tuple.proto;
    run_plugins_v2(packet, ctx, layer, layer_filter, cb, analyzer)
}

/// Run plugins attached to the transport layer (TCP, UDP, etc.)
fn run_plugins_v2_transport(
    packet: &Packet,
    ctx: &ParseContext,
    pinfo: &PacketInfo,
    analyzer: &mut Analyzer,
) -> Result<(), Error> {
    let cb = move |p: &mut dyn Plugin| p.handle_layer_transport(packet, pinfo);
    let layer = 4;
    let layer_filter = pinfo.l4_type as u16;
    run_plugins_v2(packet, ctx, layer, layer_filter, cb, analyzer)
}

pub(crate) fn gen_event_new_flow(flow: &Flow, registry: &PluginRegistry) {
    // let start = ::std::time::Instant::now();
    registry.run_plugins(
        |p| p.plugin_type() & PLUGIN_FLOW_NEW != 0,
        |p| p.flow_created(flow),
    );
    // let elapsed = start.elapsed();
    // debug!("Time to run flow_created: {}.{}", elapsed.as_secs(), elapsed.as_millis());
}

impl PcapAnalyzer for Analyzer {
    /// Initialize all plugins
    fn init(&mut self) -> Result<(), Error> {
        self.registry.run_plugins(|_| true, |p| p.pre_process());
        Ok(())
    }

    /// Dispatch function: given a packet, use link type to get the real data, and
    /// call the matching handling function (some pcap blocks encode ethernet, or IPv4 etc.)
    fn handle_packet(&mut self, packet: &Packet, ctx: &ParseContext) -> Result<(), Error> {
        match packet.data {
            PacketData::L2(data) => self.handle_l2(packet, &ctx, data),
            PacketData::L3(ethertype, data) => {
                handle_l3(packet, &ctx, data, EtherType(ethertype), self)
            }
            PacketData::L4(_, _) => unimplemented!(), // XXX
            PacketData::Unsupported(_) => {
                warn!("Unsupported data format (unknown linktype ?)");
                Err(Error::Generic("Unsupported data format"))
            }
        }
    }

    /// Finalize analysis and notify plugins
    fn teardown(&mut self) {
        {
            let flows = &self.flows;
            // expire remaining flows
            trace!("{} flows remaining in table", flows.len());
            // let start = ::std::time::Instant::now();
            self.registry.run_plugins(
                |p| p.plugin_type() & PLUGIN_FLOW_DEL != 0,
                |p| {
                    flows.values().for_each(|flow| {
                        p.flow_destroyed(flow);
                    });
                },
            );
            // let elapsed = start.elapsed();
            // debug!("Time to run flow_destroyed {}.{}", elapsed.as_secs(), elapsed.as_millis());
            self.flows.clear();

            self.registry.run_plugins(|_| true, |p| p.post_process());
        };
    }
}

impl SafePcapAnalyzer for Analyzer {}

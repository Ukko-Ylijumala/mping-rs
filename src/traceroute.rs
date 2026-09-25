// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

/*!
ICMP traceroute: one Echo Request per TTL. The router that expires the TTL
answers with Time Exceeded, the target itself with an Echo Reply.

Built on the raw-socket pieces of [crate::hopcount], like [crate::pmtu].
Hops are handed to the caller one at a time through a callback as they are
found, so the UI can show them live. Every probe of a run carries the same
ICMP checksum (the Paris-traceroute trick), so ECMP routers that hash on it
keep all probes on one path. See `doc/design/traceroute.md`.
*/

use crate::{
    hopcount::{
        ICMP_HEADER_SIZE, IPV4_HEADER_MIN, embedded_echo_v4, embedded_echo_v6, make_socket,
        set_sockopt_int, time_left,
    },
    strings::*,
    structs::DEFAULT_PAYLOAD_SIZE,
};
use libc::{IP_TTL, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_HOPS, c_int};
use pnet_packet::{
    Packet,
    icmp::{
        self, IcmpPacket, IcmpTypes, echo_reply::EchoReplyPacket,
        echo_request::MutableEchoRequestPacket,
    },
    icmpv6::{
        Icmpv6Packet, Icmpv6Types, echo_reply::EchoReplyPacket as EchoReplyPacketV6,
        echo_request::MutableEchoRequestPacket as MutableEchoRequestPacketV6,
    },
};
use rand::random;
use socket2::{SockAddr, Socket};
use std::{
    fmt,
    io::ErrorKind,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

const MAX_SILENT: u8 = 10; // consecutive unanswered TTLs before giving up
const RECV_BUF_SIZE: usize = 1500; // ICMP errors quote only the head of our probe
const INNER_V4_DST: usize = 16; // destination address offset in an IPv4 header
const INNER_V6_DST: usize = 24; // destination address offset in an IPv6 header

/// What answered a probe of a given TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopKind {
    /// An intermediate router: Time Exceeded.
    Router,
    /// The target itself: Echo Reply.
    Target,
    /// Destination Unreachable quoting our probe, from a router or the target.
    Unreachable,
    /// No answer within the timeout.
    Silent,
}

/// One hop of a traceroute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceHop {
    pub ttl: u8,
    /// Who answered; `None` for a silent hop.
    pub addr: Option<IpAddr>,
    /// Probe round trip to whoever answered.
    pub rtt: Option<Duration>,
    pub kind: HopKind,
}

/// Why a traceroute run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceEnd {
    /// The target answered.
    Reached,
    /// Something on the way (or the target) sent Destination Unreachable.
    Unreachable,
    /// `max_hops` probed without reaching the target.
    MaxHops,
    /// [MAX_SILENT] TTLs in a row went unanswered.
    Silent,
    /// The caller asked to stop (target stopped or removed).
    Aborted,
}

impl fmt::Display for TraceEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: &str = match self {
            TraceEnd::Reached => TRACE_REACHED,
            TraceEnd::Unreachable => TRACE_UNREACH,
            TraceEnd::MaxHops => TRACE_MAXHOPS,
            TraceEnd::Silent => TRACE_SILENT,
            TraceEnd::Aborted => TRACE_ABORTED,
        };
        write!(f, "{s}")
    }
}

/* -------------------------------------------------------------------------- */

/**
Trace the route to `target`, one probe per TTL from 1 to `max_hops`, waiting
up to `timeout` for each. Blocking.

`on_hop` receives every hop as soon as its verdict is in (silent ones too)
and returns whether to continue; `false` ends the run as [TraceEnd::Aborted].
The run also ends when the target answers, on a Destination Unreachable, at
`max_hops`, or after [MAX_SILENT] consecutive silent hops.
*/
pub fn trace_route(
    target: IpAddr,
    timeout: Duration,
    max_hops: u8,
    debug: bool,
    mut on_hop: impl FnMut(TraceHop) -> bool,
) -> Result<TraceEnd, String> {
    let mut prober: Prober = Prober::new(target, debug)?;
    let mut silent: u8 = 0;
    for ttl in 1..=max_hops.max(1) {
        let hop: TraceHop = prober.probe(ttl, timeout)?;
        let kind: HopKind = hop.kind;
        if !on_hop(hop) {
            return Ok(TraceEnd::Aborted);
        }
        match kind {
            HopKind::Target => return Ok(TraceEnd::Reached),
            HopKind::Unreachable => return Ok(TraceEnd::Unreachable),
            HopKind::Router => silent = 0,
            HopKind::Silent => {
                silent += 1;
                if silent >= MAX_SILENT {
                    return Ok(TraceEnd::Silent);
                }
            }
        }
    }
    Ok(TraceEnd::MaxHops)
}

/**
Build the Echo Request for probe `seq` of a run identified by `ident`.

Paris-traceroute trick: the first payload word is the ones' complement of
the sequence number, so `seq + !seq == 0xffff` (negative zero) and the ICMP
checksum is identical for every probe of the run. Load balancers that hash
on the checksum (as a stand-in for ports) then keep the run on one path.
*/
fn build_probe(v4: bool, ident: u16, seq: u16) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = vec![0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    buf[ICMP_HEADER_SIZE..ICMP_HEADER_SIZE + 2].copy_from_slice(&(!seq).to_be_bytes());
    if v4 {
        let mut pkt = MutableEchoRequestPacket::new(&mut buf).ok_or(ERR_PACKET)?;
        pkt.set_icmp_type(IcmpTypes::EchoRequest);
        pkt.set_identifier(ident);
        pkt.set_sequence_number(seq);
        let checksum = icmp::checksum(&IcmpPacket::new(pkt.packet()).ok_or(ERR_CKSUM)?);
        pkt.set_checksum(checksum);
    } else {
        // checksum left to the kernel (RFC 3542); the payload trick keeps it constant all the same
        let mut pkt = MutableEchoRequestPacketV6::new(&mut buf).ok_or(ERR_PACKET)?;
        pkt.set_icmpv6_type(Icmpv6Types::EchoRequest);
        pkt.set_identifier(ident);
        pkt.set_sequence_number(seq);
    }
    Ok(buf)
}

/// Destination of the IPv4 packet quoted in an ICMPv4 error message.
fn embedded_dst_v4(icmp_msg: &[u8]) -> Option<Ipv4Addr> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    let bytes: &[u8] = inner_ip.get(INNER_V4_DST..INNER_V4_DST + 4)?;
    Some(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?))
}

/// Destination of the IPv6 packet quoted in an ICMPv6 error message.
fn embedded_dst_v6(icmp_msg: &[u8]) -> Option<Ipv6Addr> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    let bytes: &[u8] = inner_ip.get(INNER_V6_DST..INNER_V6_DST + 16)?;
    Some(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?))
}

/**
Is this raw-socket datagram an answer to probe `seq` of run `ident` towards
`target`? IPv4 raw sockets deliver the IP header too; honor the IHL field.

Errors must quote our identifier *and* sequence number *and* our target:
several traceroutes may run at once and a router's Time Exceeded for
another target's probe would otherwise be taken for ours.
*/
fn classify_v4(
    target: Ipv4Addr,
    ident: u16,
    seq: u16,
    data: &[u8],
    from: IpAddr,
) -> Option<HopKind> {
    let ihl: usize = ((*data.first()? & 0x0f) as usize) * 4;
    if ihl < IPV4_HEADER_MIN || data.len() < ihl + ICMP_HEADER_SIZE {
        return None;
    }
    let msg: &[u8] = &data[ihl..];
    let resp = IcmpPacket::new(msg)?;
    let ours =
        || embedded_echo_v4(msg) == Some((ident, seq)) && embedded_dst_v4(msg) == Some(target);
    match resp.get_icmp_type() {
        IcmpTypes::EchoReply => {
            let reply = EchoReplyPacket::new(msg)?;
            (from == IpAddr::V4(target)
                && reply.get_identifier() == ident
                && reply.get_sequence_number() == seq)
                .then_some(HopKind::Target)
        }
        IcmpTypes::TimeExceeded => ours().then_some(HopKind::Router),
        IcmpTypes::DestinationUnreachable => ours().then_some(HopKind::Unreachable),
        _ => None,
    }
}

/// Same as [classify_v4] for IPv6, whose raw sockets deliver only the ICMPv6 message.
fn classify_v6(
    target: Ipv6Addr,
    ident: u16,
    seq: u16,
    data: &[u8],
    from: IpAddr,
) -> Option<HopKind> {
    if data.len() < ICMP_HEADER_SIZE {
        return None;
    }
    let resp = Icmpv6Packet::new(data)?;
    let ours =
        || embedded_echo_v6(data) == Some((ident, seq)) && embedded_dst_v6(data) == Some(target);
    match resp.get_icmpv6_type() {
        Icmpv6Types::EchoReply => {
            let reply = EchoReplyPacketV6::new(data)?;
            (from == IpAddr::V6(target)
                && reply.get_identifier() == ident
                && reply.get_sequence_number() == seq)
                .then_some(HopKind::Target)
        }
        Icmpv6Types::TimeExceeded => ours().then_some(HopKind::Router),
        Icmpv6Types::DestinationUnreachable => ours().then_some(HopKind::Unreachable),
        _ => None,
    }
}

/* -------------------------------------------------------------------------- */

/// One raw ICMP socket plus the identity of this run.
struct Prober {
    socket: Socket,
    target: IpAddr,
    dest: SockAddr,
    /// Random per run: concurrent traceroutes must not accept each other's answers.
    ident: u16,
    debug: bool,
}

impl Prober {
    fn new(target: IpAddr, debug: bool) -> Result<Self, String> {
        let socket: Socket = make_socket(target.is_ipv4(), debug)?;
        Ok(Self {
            socket,
            target,
            dest: SocketAddr::new(target, 0).into(),
            ident: random(),
            debug,
        })
    }

    /// Send one probe with the given TTL and wait up to `timeout` for whoever answers.
    fn probe(&mut self, ttl: u8, timeout: Duration) -> Result<TraceHop, String> {
        let v4: bool = self.target.is_ipv4();
        let (level, name): (c_int, c_int) = if v4 {
            (IPPROTO_IP, IP_TTL)
        } else {
            (IPPROTO_IPV6, IPV6_UNICAST_HOPS)
        };
        set_sockopt_int(&self.socket, level, name, ttl as c_int)?;
        let seq: u16 = ttl as u16;
        let packet: Vec<u8> = build_probe(v4, self.ident, seq)?;
        if self.debug {
            eprintln!("{INFO_TRACE_PROBE} {ttl}");
        }
        let sent: Instant = Instant::now();
        self.socket
            .send_to(&packet, &self.dest)
            .map_err(|e| format!("{ERR_SEND}: {e}"))?;

        let silent = TraceHop {
            ttl,
            addr: None,
            rtt: None,
            kind: HopKind::Silent,
        };
        let deadline: Instant = sent + timeout;
        let mut buf = [const { MaybeUninit::<u8>::uninit() }; RECV_BUF_SIZE];
        loop {
            let Ok(left) = time_left(deadline) else {
                return Ok(silent);
            };
            self.socket
                .set_read_timeout(Some(left))
                .map_err(|e| format!("{ERR_SOCK_TIMEOUT}: {e}"))?;
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(silent);
                }
                Err(e) => return Err(format!("{ERR_RECV}: {e}")),
            };
            let rtt: Duration = sent.elapsed();
            // SAFETY: recv_from initialized the first `n` bytes.
            let data: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), n) };
            let Some(from_ip) = from.as_socket().map(|sa| sa.ip()) else {
                continue;
            };
            let kind: Option<HopKind> = match self.target {
                IpAddr::V4(t) => classify_v4(t, self.ident, seq, data, from_ip),
                IpAddr::V6(t) => classify_v6(t, self.ident, seq, data, from_ip),
            };
            if let Some(kind) = kind {
                if self.debug {
                    eprintln!("{INFO_TRACE_HOP} {ttl}: {from_ip} {kind:?} ({rtt:?})");
                }
                return Ok(TraceHop {
                    ttl,
                    addr: Some(from_ip),
                    rtt: Some(rtt),
                    kind,
                });
            }
        }
    }
}

/* ================================= tests ================================== */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hopcount::IPV6_HEADER_SIZE;

    const IDENT: u16 = 0x1234;
    const T4: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
    const T6: Ipv6Addr = Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888);
    const ROUTER4: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    const ROUTER6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));

    /// An IPv4 header (no options) with the given destination.
    fn ip4_header(dst: Ipv4Addr) -> Vec<u8> {
        let mut h = vec![0u8; IPV4_HEADER_MIN];
        h[0] = 0x45;
        h[INNER_V4_DST..INNER_V4_DST + 4].copy_from_slice(&dst.octets());
        h
    }

    /// An IPv6 header with the given destination.
    fn ip6_header(dst: Ipv6Addr) -> Vec<u8> {
        let mut h = vec![0u8; IPV6_HEADER_SIZE];
        h[INNER_V6_DST..INNER_V6_DST + 16].copy_from_slice(&dst.octets());
        h
    }

    fn echo(kind: u8, ident: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 0];
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&seq.to_be_bytes());
        m
    }

    /// ICMPv4 `[type, code, csum, unused]` quoting a probe to `dst` (with its IP header).
    fn v4_error(kind: u8, dst: Ipv4Addr, ident: u16, seq: u16) -> Vec<u8> {
        let mut d = ip4_header(Ipv4Addr::UNSPECIFIED); // outer header, from the router
        d.extend_from_slice(&[kind, 0, 0, 0, 0, 0, 0, 0]);
        d.extend(ip4_header(dst));
        d.extend(echo(8, ident, seq));
        d
    }

    /// ICMPv6 `[type, code, csum, unused]` quoting a probe to `dst`.
    fn v6_error(kind: u8, dst: Ipv6Addr, ident: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 0, 0, 0, 0, 0];
        m.extend(ip6_header(dst));
        m.extend(echo(128, ident, seq));
        m
    }

    #[test]
    fn checksum_is_constant_across_probes() {
        let csum = |seq: u16| {
            let p = build_probe(true, IDENT, seq).unwrap();
            u16::from_be_bytes([p[2], p[3]])
        };
        let first = csum(1);
        for seq in 2..=64 {
            assert_eq!(csum(seq), first, "seq {seq}");
        }
        // the payload really carries !seq and the header the seq itself
        let p = build_probe(true, IDENT, 5).unwrap();
        assert_eq!(&p[6..8], &5u16.to_be_bytes());
        assert_eq!(&p[8..10], &(!5u16).to_be_bytes());
        assert_eq!(p.len(), ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE);
        let p6 = build_probe(false, IDENT, 5).unwrap();
        assert_eq!(p6[0], 128);
        assert_eq!(&p6[8..10], &(!5u16).to_be_bytes());
    }

    #[test]
    fn classify_v4_hops() {
        let mut reply = ip4_header(Ipv4Addr::UNSPECIFIED);
        reply.extend(echo(0, IDENT, 3));
        assert_eq!(
            classify_v4(T4, IDENT, 3, &reply, IpAddr::V4(T4)),
            Some(HopKind::Target)
        );
        assert_eq!(
            classify_v4(T4, IDENT, 3, &reply, ROUTER4),
            None,
            "reply not from target"
        );
        assert_eq!(
            classify_v4(T4, IDENT, 4, &reply, IpAddr::V4(T4)),
            None,
            "stale seq"
        );

        let exceeded = v4_error(11, T4, IDENT, 3);
        assert_eq!(
            classify_v4(T4, IDENT, 3, &exceeded, ROUTER4),
            Some(HopKind::Router)
        );
        let other_run = v4_error(11, T4, IDENT + 1, 3);
        assert_eq!(classify_v4(T4, IDENT, 3, &other_run, ROUTER4), None);
        let other_target = v4_error(11, Ipv4Addr::new(1, 1, 1, 1), IDENT, 3);
        assert_eq!(
            classify_v4(T4, IDENT, 3, &other_target, ROUTER4),
            None,
            "same seq, other target"
        );
        let unreach = v4_error(3, T4, IDENT, 3);
        assert_eq!(
            classify_v4(T4, IDENT, 3, &unreach, ROUTER4),
            Some(HopKind::Unreachable)
        );
        assert_eq!(
            classify_v4(T4, IDENT, 3, &[0x45, 0], ROUTER4),
            None,
            "truncated"
        );
    }

    #[test]
    fn classify_v6_hops() {
        let reply = echo(129, IDENT, 2);
        assert_eq!(
            classify_v6(T6, IDENT, 2, &reply, IpAddr::V6(T6)),
            Some(HopKind::Target)
        );
        assert_eq!(classify_v6(T6, IDENT, 2, &reply, ROUTER6), None);

        let exceeded = v6_error(3, T6, IDENT, 2);
        assert_eq!(
            classify_v6(T6, IDENT, 2, &exceeded, ROUTER6),
            Some(HopKind::Router)
        );
        let other_target = v6_error(3, Ipv6Addr::LOCALHOST, IDENT, 2);
        assert_eq!(classify_v6(T6, IDENT, 2, &other_target, ROUTER6), None);
        let unreach = v6_error(1, T6, IDENT, 2);
        assert_eq!(
            classify_v6(T6, IDENT, 2, &unreach, ROUTER6),
            Some(HopKind::Unreachable)
        );
        assert_eq!(
            classify_v6(T6, IDENT, 2, &[3, 0, 0], ROUTER6),
            None,
            "truncated"
        );
    }
}

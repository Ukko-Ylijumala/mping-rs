// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

/*!
Path MTU discovery with Don't-Fragment ICMP Echo Requests.

Reuses the raw-socket machinery of [crate::hopcount]: the same socket
creation, the "skip everything that isn't ours" receive discipline and the
quoted-probe check on ICMP error messages. A probe is an Echo Request
padded to a given total IP packet size with DF set. An Echo Reply means the
size fits; a Fragmentation Needed (v4) / Packet Too Big (v6) error from a
router means it doesn't, and usually says what does; silence means a router
dropped the probe without telling us (a "PMTUD black hole"), in which case
the answer comes from bisection. See `doc/design/pmtu.md`.
*/

use crate::{
    hopcount::{
        ICMP_HEADER_SIZE, IDENT_PMTU_BIT, IPV4_HEADER_MIN, IPV6_HEADER_SIZE, make_socket,
        quotes_probe_v4, quotes_probe_v6, set_sockopt_int, time_left,
    },
    strings::*,
};
use libc::{EMSGSIZE, IPPROTO_IP, IPPROTO_IPV6, c_int};
#[cfg(target_os = "macos")]
use libc::{IP_DONTFRAG, IPV6_DONTFRAG};
#[cfg(target_os = "linux")]
use libc::{
    IP_MTU, IP_MTU_DISCOVER, IP_PMTUDISC_PROBE, IPV6_MTU, IPV6_MTU_DISCOVER, IPV6_PMTUDISC_PROBE,
};
use pnet_packet::{
    Packet,
    icmp::{
        self, IcmpPacket, IcmpTypes, destination_unreachable::IcmpCodes,
        echo_reply::EchoReplyPacket, echo_request::MutableEchoRequestPacket,
    },
    icmpv6::{
        Icmpv6Packet, Icmpv6Types, echo_reply::EchoReplyPacket as EchoReplyPacketV6,
        echo_request::MutableEchoRequestPacket as MutableEchoRequestPacketV6,
    },
};
use rand::random;
#[cfg(target_os = "linux")]
use socket2::{Domain, Protocol, Type};
use socket2::{SockAddr, Socket};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::{
    fmt,
    io::ErrorKind,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

const MIN_MTU_V4: u16 = 576; // RFC 1122 minimum reassembly buffer, the practical floor
const MIN_MTU_V6: u16 = 1280; // RFC 8200 minimum link MTU
const DEFAULT_MAX_MTU: u16 = 1500; // upper bound when the kernel can't tell us the route MTU
/**
Probe budget. Bisecting 576..1500 needs ~10 probes, router hints far fewer;
a black hole costs two per failed size (each timeout is confirmed by a
retry), up to ~23 for 576..1500 and ~31 for 576..9000.
*/
const MAX_PROBES: u32 = 32;
const RECV_BUF_SIZE: usize = u16::MAX as usize; // an echo reply is as big as the probe
#[cfg(target_os = "linux")]
const KERNEL_MTU_PORT: u16 = 33434; // any port: the UDP socket is only connected, never sent on

/// Result of a path MTU discovery run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pmtu {
    /// Largest IP packet size (headers included) that reached the target unfragmented.
    pub mtu: u16,
    /// Some probe was dropped silently (no ICMP error came back): a PMTUD
    /// black hole. The result then comes from bisection rather than from a
    /// router reporting its next-hop MTU.
    pub blackhole: bool,
    /// Number of probes sent.
    pub probes: u32,
}

impl fmt::Display for Pmtu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.mtu)?;
        if self.blackhole {
            write!(f, " ({WARN_PMTU_BLACKHOLE})")?;
        }
        Ok(())
    }
}

/// What one DF probe of a given size told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    /// The target answered: this size fits the whole path.
    Reply,
    /// Too big for some link; the router's next-hop MTU if it reported a usable one.
    TooBig(Option<u16>),
    /// Nothing came back within the timeout.
    Timeout,
    /// A Destination Unreachable (other than Fragmentation Needed) for our probe.
    Unreachable,
}

/* -------------------------------------------------------------------------- */

/**
Discover the path MTU to `target`. Blocking; sends up to [MAX_PROBES] probes
with `timeout` each.

Search strategy: start at the kernel's route MTU (Linux, else 1500). A reply
there is the common one-probe case. Otherwise a router's reported next-hop
MTU is tested directly; without one (or on silence) the floor is checked to
make sure the target answers at all, then the gap is bisected. Every path
ends in either the largest size that got a reply or a hard error.
*/
pub fn determine_pmtu(target: IpAddr, timeout: Duration, debug: bool) -> Result<Pmtu, String> {
    let min: u16 = if target.is_ipv4() {
        MIN_MTU_V4
    } else {
        MIN_MTU_V6
    };
    let mut prober: Prober = Prober::new(target, debug)?;
    let upper: u16 = kernel_mtu(target).unwrap_or(DEFAULT_MAX_MTU).max(min);
    if debug {
        eprintln!("{INFO_PMTU_UPPER} {upper}, floor {min}");
    }
    let (mtu, blackhole) = search(min, upper, MAX_PROBES, |size| prober.probe(size, timeout))?;
    Ok(Pmtu {
        mtu,
        blackhole,
        probes: prober.probes,
    })
}

/**
The size search proper, kept free of sockets so it can be exercised against
a simulated path. `probe(size)` reports what one DF probe of `size` bytes
saw. A timeout is re-probed once at the same size before it counts as a
failure. Returns the largest size that got a reply and whether any size
vanished silently twice on the way (black hole).
*/
fn search(
    min: u16,
    upper: u16,
    max_probes: u32,
    mut probe: impl FnMut(u16) -> Result<ProbeResult, String>,
) -> Result<(u16, bool), String> {
    let mut good: Option<u16> = None; // largest size known to pass
    let mut bad: Option<u16> = None; // smallest size known to fail
    let mut blackhole: bool = false;
    let mut candidate: u16 = upper;
    let mut from_hint: bool = false; // candidate is a router's reported next-hop MTU
    let mut silent: bool = false; // the last failure was a timeout
    let mut retried: bool = false; // candidate already timed out once
    for _ in 0..max_probes {
        let mut hint: Option<u16> = None;
        match probe(candidate)? {
            /*
            Silence is only a verdict once confirmed: a single lost probe or
            reply (ordinary packet loss) would otherwise mark the size failed
            for good, capping the result below it with a bogus black hole.
            */
            ProbeResult::Timeout if !retried => {
                retried = true;
                continue;
            }
            ProbeResult::Reply => {
                good = Some(candidate);
                /*
                Done when nothing is left between good and bad - or when the
                size came from a router's Fragmentation Needed: that router is
                authoritative for its link, and any narrower link further on
                would have answered this probe with its own error instead.
                */
                if from_hint || bad.is_none_or(|b| b - candidate <= 1) {
                    return Ok((candidate, blackhole));
                }
            }
            ProbeResult::TooBig(h) => {
                bad = Some(candidate);
                hint = h;
                silent = false;
            }
            ProbeResult::Timeout => {
                bad = Some(candidate);
                blackhole = true;
                silent = true;
            }
            ProbeResult::Unreachable => return Err(ERR_UNREACH.to_string()),
        }

        // A router's next-hop MTU is only worth testing if it narrows the gap.
        let usable_hint: Option<u16> =
            hint.filter(|h| *h >= min && *h < candidate && good.is_none_or(|g| *h > g));
        from_hint = usable_hint.is_some();
        retried = false;
        candidate = match usable_hint {
            Some(h) => h,
            None => {
                let b: u16 = bad.expect("a failed probe sets `bad`");
                match good {
                    Some(g) if b - g <= 1 => return Ok((g, blackhole)),
                    Some(g) => g + (b - g) / 2,
                    // nothing known to pass yet: does the target answer at the floor at all?
                    None if b > min => min,
                    None if silent => return Err(TIMEOUT.to_string()),
                    None => return Err(ERR_PMTU_FLOOR.to_string()),
                }
            }
        };
    }
    Err(ERR_PMTU_PROBES.to_string())
}

/// Set the Don't Fragment behaviour on a raw ICMP socket, per platform.
/// Linux `PROBE` mode also bypasses the kernel's cached path MTU, so probes
/// really hit the wire at the requested size (up to the interface MTU).
fn set_dont_fragment(socket: &Socket, v4: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let (level, name, value): (c_int, c_int, c_int) = if v4 {
        (IPPROTO_IP, IP_MTU_DISCOVER, IP_PMTUDISC_PROBE)
    } else {
        (IPPROTO_IPV6, IPV6_MTU_DISCOVER, IPV6_PMTUDISC_PROBE)
    };
    #[cfg(target_os = "macos")]
    let (level, name, value): (c_int, c_int, c_int) = if v4 {
        (IPPROTO_IP, IP_DONTFRAG, 1)
    } else {
        (IPPROTO_IPV6, IPV6_DONTFRAG, 1)
    };
    set_sockopt_int(socket, level, name, value)
}

/// The kernel's current idea of the MTU towards `target` (interface MTU or a
/// cached path MTU), read off a connected but never used UDP socket.
#[cfg(target_os = "linux")]
fn kernel_mtu(target: IpAddr) -> Option<u16> {
    let (domain, level, name) = if target.is_ipv4() {
        (Domain::IPV4, IPPROTO_IP, IP_MTU)
    } else {
        (Domain::IPV6, IPPROTO_IPV6, IPV6_MTU)
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    sock.connect(&SocketAddr::new(target, KERNEL_MTU_PORT).into())
        .ok()?;
    let mut mtu: c_int = 0;
    let mut len: libc::socklen_t = size_of::<c_int>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            level,
            name,
            (&raw mut mtu).cast(),
            &raw mut len,
        )
    };
    (ret == 0).then(|| u16::try_from(mtu).ok()).flatten()
}

/// No portable way to ask; the search starts from [DEFAULT_MAX_MTU].
#[cfg(not(target_os = "linux"))]
fn kernel_mtu(_target: IpAddr) -> Option<u16> {
    None
}

/* -------------------------------------------------------------------------- */

/// One raw ICMP socket plus the bookkeeping to send sized DF probes and
/// pick our own answers out of the raw-socket firehose.
struct Prober {
    socket: Socket,
    target: IpAddr,
    dest: SockAddr,
    v4: bool,
    /// Random per run (top bit set, unlike hopcount's): concurrent runs must
    /// not accept each other's replies or errors.
    ident: u16,
    seq: u16,
    probes: u32,
    debug: bool,
}

impl Prober {
    fn new(target: IpAddr, debug: bool) -> Result<Self, String> {
        let v4: bool = target.is_ipv4();
        let socket: Socket = make_socket(v4, debug)?;
        set_dont_fragment(&socket, v4)?;
        Ok(Self {
            socket,
            target,
            dest: SocketAddr::new(target, 0).into(),
            v4,
            ident: random::<u16>() | IDENT_PMTU_BIT,
            seq: 0,
            probes: 0,
            debug,
        })
    }

    fn done(&self, mtu: u16, blackhole: bool) -> Pmtu {
        Pmtu {
            mtu,
            blackhole,
            probes: self.probes,
        }
    }

    /// Build an Echo Request whose IP packet totals `size` bytes on the wire.
    fn build(&self, size: u16) -> Result<Vec<u8>, String> {
        let ip_hdr: usize = if self.v4 {
            IPV4_HEADER_MIN
        } else {
            IPV6_HEADER_SIZE
        };
        let icmp_len: usize = (size as usize)
            .checked_sub(ip_hdr)
            .filter(|n| *n >= ICMP_HEADER_SIZE)
            .ok_or(ERR_PMTU_SIZE)?;
        let mut buf: Vec<u8> = vec![0u8; icmp_len];
        if self.v4 {
            let mut pkt = MutableEchoRequestPacket::new(&mut buf).ok_or(ERR_PACKET)?;
            pkt.set_icmp_type(IcmpTypes::EchoRequest);
            pkt.set_identifier(self.ident);
            pkt.set_sequence_number(self.seq);
            let checksum = icmp::checksum(&IcmpPacket::new(pkt.packet()).ok_or(ERR_CKSUM)?);
            pkt.set_checksum(checksum);
        } else {
            // checksum left to the kernel, as in hopcount (RFC 3542)
            let mut pkt = MutableEchoRequestPacketV6::new(&mut buf).ok_or(ERR_PACKET)?;
            pkt.set_icmpv6_type(Icmpv6Types::EchoRequest);
            pkt.set_identifier(self.ident);
            pkt.set_sequence_number(self.seq);
        }
        Ok(buf)
    }

    /// Send one probe of `size` bytes and wait up to `timeout` for a verdict.
    fn probe(&mut self, size: u16, timeout: Duration) -> Result<ProbeResult, String> {
        self.seq = self.seq.wrapping_add(1);
        self.probes += 1;
        let packet: Vec<u8> = self.build(size)?;
        if self.debug {
            eprintln!("{INFO_PMTU_PROBE} {size} (seq {})", self.seq);
        }
        if let Err(e) = self.socket.send_to(&packet, &self.dest) {
            if e.raw_os_error() == Some(EMSGSIZE) {
                // the kernel won't even try: bigger than the interface MTU
                return Ok(ProbeResult::TooBig(None));
            }
            return Err(format!("{ERR_SEND}: {e}"));
        }

        let deadline: Instant = Instant::now() + timeout;
        let mut buf = vec![MaybeUninit::<u8>::uninit(); RECV_BUF_SIZE];
        loop {
            let Ok(left) = time_left(deadline) else {
                return Ok(ProbeResult::Timeout);
            };
            self.socket
                .set_read_timeout(Some(left))
                .map_err(|e| format!("{ERR_SOCK_TIMEOUT}: {e}"))?;
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(ProbeResult::Timeout);
                }
                Err(e) => return Err(format!("{ERR_RECV}: {e}")),
            };
            // SAFETY: recv_from initialized the first `n` bytes.
            let data: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), n) };
            let from_target: bool = from.as_socket().is_some_and(|sa| sa.ip() == self.target);
            let verdict: Option<ProbeResult> = match self.target {
                IpAddr::V4(t) => classify_v4(t, self.ident, self.seq, data, from_target),
                IpAddr::V6(t) => classify_v6(t, self.ident, self.seq, data, from_target),
            };
            if let Some(v) = verdict {
                if self.debug {
                    eprintln!("{INFO_PMTU_VERDICT} {size}: {v:?} ({n} bytes from {from:?})");
                }
                return Ok(v);
            }
        }
    }
}

/**
Is this raw-socket datagram the verdict on probe `seq` of run `ident` towards
`target`? IPv4 raw sockets deliver the IP header too; honor the IHL field.

Errors must quote our identifier, sequence number *and* target: a router's
Fragmentation Needed or Destination Unreachable for another target's probe
reaches every raw ICMP socket on the host.
*/
fn classify_v4(
    target: Ipv4Addr,
    ident: u16,
    seq: u16,
    data: &[u8],
    from_target: bool,
) -> Option<ProbeResult> {
    let ihl: usize = ((*data.first()? & 0x0f) as usize) * 4;
    if ihl < IPV4_HEADER_MIN || data.len() < ihl + ICMP_HEADER_SIZE {
        return None;
    }
    let msg: &[u8] = &data[ihl..];
    let resp = IcmpPacket::new(msg)?;
    match resp.get_icmp_type() {
        IcmpTypes::EchoReply => {
            let reply = EchoReplyPacket::new(msg)?;
            (from_target && reply.get_identifier() == ident && reply.get_sequence_number() == seq)
                .then_some(ProbeResult::Reply)
        }
        IcmpTypes::DestinationUnreachable => {
            if !quotes_probe_v4(msg, target, ident, seq) {
                return None;
            }
            if resp.get_icmp_code() == IcmpCodes::FragmentationRequiredAndDFFlagSet {
                // RFC 1191: next-hop MTU in the low 16 bits of the "unused" word
                let mtu: u16 = u16::from_be_bytes([msg[6], msg[7]]);
                Some(ProbeResult::TooBig((mtu > 0).then_some(mtu)))
            } else {
                Some(ProbeResult::Unreachable)
            }
        }
        _ => None,
    }
}

/// Same as [classify_v4] for IPv6, whose raw sockets deliver only the ICMPv6 message.
fn classify_v6(
    target: Ipv6Addr,
    ident: u16,
    seq: u16,
    data: &[u8],
    from_target: bool,
) -> Option<ProbeResult> {
    if data.len() < ICMP_HEADER_SIZE {
        return None;
    }
    let resp = Icmpv6Packet::new(data)?;
    match resp.get_icmpv6_type() {
        Icmpv6Types::EchoReply => {
            let reply = EchoReplyPacketV6::new(data)?;
            (from_target && reply.get_identifier() == ident && reply.get_sequence_number() == seq)
                .then_some(ProbeResult::Reply)
        }
        Icmpv6Types::PacketTooBig => {
            if !quotes_probe_v6(data, target, ident, seq) {
                return None;
            }
            // RFC 4443: MTU of the constricting link in the 32 bits after the header
            let mtu: u32 = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            Some(ProbeResult::TooBig(
                u16::try_from(mtu).ok().filter(|m| *m > 0),
            ))
        }
        Icmpv6Types::DestinationUnreachable => {
            quotes_probe_v6(data, target, ident, seq).then_some(ProbeResult::Unreachable)
        }
        _ => None,
    }
}

/* ================================= tests ================================== */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hopcount::test_packets::{echo, ip4_header, v4_error_msg, v6_error_msg};

    const SEQ: u16 = 7;
    const IDENT: u16 = 0x8123;
    const T4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const OTHER4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const T6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
    const OTHER6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);

    /// IPv4 header (no options) in front of an ICMP message, as a raw socket delivers it.
    fn v4_datagram(icmp: &[u8]) -> Vec<u8> {
        let mut d = ip4_header(Ipv4Addr::UNSPECIFIED);
        d.extend_from_slice(icmp);
        d
    }

    /// ICMPv4 Destination Unreachable `[3, code, csum, word]` quoting a probe to `dst`.
    fn v4_error(code: u8, word: u32, dst: Ipv4Addr, ident: u16, seq: u16) -> Vec<u8> {
        v4_datagram(&v4_error_msg(3, code, word, dst, &echo(8, ident, seq)))
    }

    /// ICMPv6 error `[type, code, csum, word]` quoting a probe to `dst`.
    fn v6_error(kind: u8, word: u32, dst: Ipv6Addr, ident: u16, seq: u16) -> Vec<u8> {
        v6_error_msg(kind, 0, word, dst, &echo(128, ident, seq))
    }

    #[test]
    fn classify_v4_replies_and_errors() {
        let classify =
            |data: &[u8], from_target: bool| classify_v4(T4, IDENT, SEQ, data, from_target);
        let reply = v4_datagram(&echo(0, IDENT, SEQ));
        assert_eq!(classify(&reply, true), Some(ProbeResult::Reply));
        assert_eq!(classify(&reply, false), None, "not from the target");
        assert_eq!(
            classify_v4(T4, IDENT, SEQ + 1, &reply, true),
            None,
            "stale sequence"
        );
        let other = v4_datagram(&echo(0, IDENT & !IDENT_PMTU_BIT, SEQ));
        assert_eq!(classify(&other, true), None, "hopcount's reply");

        let frag = v4_error(4, 1400, T4, IDENT, SEQ);
        assert_eq!(
            classify(&frag, false),
            Some(ProbeResult::TooBig(Some(1400)))
        );
        let frag0 = v4_error(4, 0, T4, IDENT, SEQ);
        assert_eq!(classify(&frag0, false), Some(ProbeResult::TooBig(None)));
        let unreach = v4_error(1, 0, T4, IDENT, SEQ);
        assert_eq!(classify(&unreach, false), Some(ProbeResult::Unreachable));
        let foreign = v4_error(4, 1400, T4, IDENT + 1, SEQ);
        assert_eq!(classify(&foreign, false), None, "someone else's probe");
        let stale = v4_error(4, 1400, T4, IDENT, SEQ - 1);
        assert_eq!(classify(&stale, false), None, "error for an earlier probe");
        // same ident and seq (another run, or a late error), but for another target
        let other_frag = v4_error(4, 1400, OTHER4, IDENT, SEQ);
        assert_eq!(classify(&other_frag, false), None, "other target's hint");
        let other_unreach = v4_error(1, 0, OTHER4, IDENT, SEQ);
        assert_eq!(
            classify(&other_unreach, false),
            None,
            "other target unreachable"
        );

        assert_eq!(classify(&[], true), None);
        assert_eq!(classify(&[0x45, 0, 0], true), None, "truncated");
    }

    #[test]
    fn classify_v6_replies_and_errors() {
        let classify =
            |data: &[u8], from_target: bool| classify_v6(T6, IDENT, SEQ, data, from_target);
        let reply = echo(129, IDENT, SEQ);
        assert_eq!(classify(&reply, true), Some(ProbeResult::Reply));
        assert_eq!(classify(&reply, false), None);

        let too_big = v6_error(2, 1280, T6, IDENT, SEQ);
        assert_eq!(
            classify(&too_big, false),
            Some(ProbeResult::TooBig(Some(1280)))
        );
        let huge = v6_error(2, 70_000, T6, IDENT, SEQ);
        assert_eq!(classify(&huge, false), Some(ProbeResult::TooBig(None)));
        let unreach = v6_error(1, 0, T6, IDENT, SEQ);
        assert_eq!(classify(&unreach, false), Some(ProbeResult::Unreachable));
        let foreign = v6_error(2, 1280, T6, IDENT + 1, SEQ);
        assert_eq!(classify(&foreign, false), None);
        let other_too_big = v6_error(2, 1280, OTHER6, IDENT, SEQ);
        assert_eq!(classify(&other_too_big, false), None, "other target's hint");
        let other_unreach = v6_error(1, 0, OTHER6, IDENT, SEQ);
        assert_eq!(
            classify(&other_unreach, false),
            None,
            "other target unreachable"
        );
        assert_eq!(classify(&[2, 0, 0], false), None, "truncated");
    }

    /// Run the search against a simulated path; returns (result, probe sizes sent).
    fn simulate(
        min: u16,
        upper: u16,
        mut path: impl FnMut(u16) -> ProbeResult,
    ) -> (Result<(u16, bool), String>, Vec<u16>) {
        let mut sent: Vec<u16> = Vec::new();
        let res = search(min, upper, MAX_PROBES, |size| {
            sent.push(size);
            Ok(path(size))
        });
        (res, sent)
    }

    #[test]
    fn search_one_probe_when_upper_fits() {
        let (res, sent) = simulate(MIN_MTU_V4, 1500, |_| ProbeResult::Reply);
        assert_eq!(res, Ok((1500, false)));
        assert_eq!(sent, vec![1500]);
    }

    #[test]
    fn search_follows_router_hint() {
        let path = |s: u16| {
            if s <= 1400 {
                ProbeResult::Reply
            } else {
                ProbeResult::TooBig(Some(1400))
            }
        };
        let (res, sent) = simulate(MIN_MTU_V4, 1500, path);
        assert_eq!(res, Ok((1400, false)));
        assert_eq!(sent, vec![1500, 1400]);
    }

    #[test]
    fn search_bisects_without_hint() {
        for pmtu in [577u16, 1000, 1492, 1499] {
            let path = |s: u16| {
                if s <= pmtu {
                    ProbeResult::Reply
                } else {
                    ProbeResult::TooBig(None)
                }
            };
            let (res, sent) = simulate(MIN_MTU_V4, 1500, path);
            assert_eq!(res, Ok((pmtu, false)), "pmtu {pmtu}: {sent:?}");
            assert_eq!(sent[1], MIN_MTU_V4, "floor is checked before bisecting");
            assert!(sent.len() as u32 <= MAX_PROBES);
        }
    }

    #[test]
    fn search_bisects_through_black_hole() {
        let path = |s: u16| {
            if s <= 1400 {
                ProbeResult::Reply
            } else {
                ProbeResult::Timeout
            }
        };
        let (res, sent) = simulate(MIN_MTU_V4, 1500, path);
        assert_eq!(res, Ok((1400, true)));
        assert!(sent.len() as u32 <= MAX_PROBES, "{sent:?}");
        // every silent size is confirmed by a second probe before it counts
        for s in sent.iter().filter(|s| **s > 1400) {
            assert_eq!(sent.iter().filter(|x| *x == s).count(), 2, "{s}: {sent:?}");
        }

        // worst cases: every bisection step a black hole, all within budget
        for (upper, pmtu) in [(1500u16, 577u16), (9000, 577), (9000, 8999)] {
            let path = |s: u16| {
                if s <= pmtu {
                    ProbeResult::Reply
                } else {
                    ProbeResult::Timeout
                }
            };
            let (res, sent) = simulate(MIN_MTU_V4, upper, path);
            assert_eq!(res, Ok((pmtu, true)), "{upper}/{pmtu}: {sent:?}");
        }
    }

    /// A path with MTU `pmtu` that loses the first probe of size `lost`.
    fn lossy_path(pmtu: u16, lost: u16) -> impl FnMut(u16) -> ProbeResult {
        let mut dropped: bool = false;
        move |s: u16| {
            if s == lost && !dropped {
                dropped = true;
                ProbeResult::Timeout
            } else if s <= pmtu {
                ProbeResult::Reply
            } else {
                ProbeResult::TooBig(None)
            }
        }
    }

    #[test]
    fn search_retries_a_single_lost_probe() {
        // the upper bound itself: one retry, no black hole
        let (res, sent) = simulate(MIN_MTU_V4, 1500, lossy_path(1500, 1500));
        assert_eq!(res, Ok((1500, false)));
        assert_eq!(sent, vec![1500, 1500]);

        // mid-bisection: the lost size still counts as good
        let (res, sent) = simulate(MIN_MTU_V4, 1500, lossy_path(1400, 1384));
        assert_eq!(res, Ok((1400, false)), "{sent:?}");
        assert_eq!(sent.iter().filter(|s| **s == 1384).count(), 2, "{sent:?}");

        // a lost floor probe is not "the target doesn't answer"
        let (res, sent) = simulate(MIN_MTU_V4, 1500, lossy_path(1000, MIN_MTU_V4));
        assert_eq!(res, Ok((1000, false)), "{sent:?}");
        assert_eq!(&sent[..3], &[1500, MIN_MTU_V4, MIN_MTU_V4]);

        // a failure confirmed on the retry is an ordinary failure, not a black hole
        let (res, sent) = simulate(MIN_MTU_V4, 1500, lossy_path(1400, 1500));
        assert_eq!(res, Ok((1400, false)), "{sent:?}");
        assert_eq!(&sent[..2], &[1500, 1500]);
    }

    #[test]
    fn search_ignores_useless_hints() {
        // hint above the failed size, and a hint below the floor
        for bogus in [9000u16, 100] {
            let path = |s: u16| {
                if s <= 1200 {
                    ProbeResult::Reply
                } else {
                    ProbeResult::TooBig(Some(bogus))
                }
            };
            let (res, _) = simulate(MIN_MTU_V4, 1500, path);
            assert_eq!(res, Ok((1200, false)), "hint {bogus}");
        }
        // a hint that is itself too big is followed once, then bisected past
        // (a reply at a hinted size is final - see search_follows_router_hint)
        let path = |s: u16| {
            if s <= 1300 {
                ProbeResult::Reply
            } else {
                ProbeResult::TooBig(Some(1400))
            }
        };
        let (res, sent) = simulate(MIN_MTU_V4, 1500, path);
        assert_eq!(res, Ok((1300, false)));
        assert_eq!(&sent[..2], &[1500, 1400]);
    }

    #[test]
    fn search_v6_floor_and_upper_below_floor() {
        let path = |s: u16| {
            if s <= 1280 {
                ProbeResult::Reply
            } else {
                ProbeResult::TooBig(Some(1280))
            }
        };
        let (res, sent) = simulate(MIN_MTU_V6, 1500, path);
        assert_eq!(res, Ok((1280, false)));
        assert_eq!(sent, vec![1500, 1280]);
        // the caller clamps upper to the floor; a reply there is final
        let (res, sent) = simulate(MIN_MTU_V6, 1280, |_| ProbeResult::Reply);
        assert_eq!(res, Ok((1280, false)));
        assert_eq!(sent, vec![1280]);
    }

    #[test]
    fn search_errors() {
        let (res, sent) = simulate(MIN_MTU_V4, 1500, |_| ProbeResult::Timeout);
        assert_eq!(res, Err(TIMEOUT.to_string()), "silent target");
        assert_eq!(sent, vec![1500, 1500, MIN_MTU_V4, MIN_MTU_V4]);
        let (res, _) = simulate(MIN_MTU_V4, 1500, |_| ProbeResult::TooBig(None));
        assert_eq!(res, Err(ERR_PMTU_FLOOR.to_string()));
        // the floor's own verdict decides the error, not an earlier silence
        let path = |s: u16| {
            if s > 1000 {
                ProbeResult::Timeout
            } else {
                ProbeResult::TooBig(None)
            }
        };
        let (res, sent) = simulate(MIN_MTU_V4, 1500, path);
        assert_eq!(res, Err(ERR_PMTU_FLOOR.to_string()));
        assert_eq!(sent, vec![1500, 1500, MIN_MTU_V4]);
        let (res, _) = simulate(MIN_MTU_V4, 1500, |_| ProbeResult::Unreachable);
        assert_eq!(res, Err(ERR_UNREACH.to_string()));
        let res = search(MIN_MTU_V4, 1500, MAX_PROBES, |_| Err("boom".to_string()));
        assert_eq!(res, Err("boom".to_string()), "socket errors propagate");
        // a budget too small for the bisection is reported as such
        let path = |s: u16| {
            if s <= 1000 {
                ProbeResult::Reply
            } else {
                ProbeResult::TooBig(None)
            }
        };
        let res = search(MIN_MTU_V4, 1500, 3, |size| Ok(path(size)));
        assert_eq!(res, Err(ERR_PMTU_PROBES.to_string()));
    }
}

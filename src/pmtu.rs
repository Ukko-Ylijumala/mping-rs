// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

/*!
Path MTU discovery with Don't-Fragment ICMP Echo Requests.

Reuses the raw-socket machinery of [crate::hopcount]: the same socket
creation, the "skip everything that isn't ours" receive discipline and the
embedded-ident check on ICMP error messages. A probe is an Echo Request
padded to a given total IP packet size with DF set. An Echo Reply means the
size fits; a Fragmentation Needed (v4) / Packet Too Big (v6) error from a
router means it doesn't, and usually says what does; silence means a router
dropped the probe without telling us (a "PMTUD black hole"), in which case
the answer comes from bisection. See `doc/design/pmtu.md`.
*/

use crate::{
    hopcount::{
        ICMP_HEADER_SIZE, IPV4_HEADER_MIN, IPV6_HEADER_SIZE, embedded_echo_v4, embedded_echo_v6,
        make_socket, set_sockopt_int, time_left,
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
#[cfg(target_os = "linux")]
use socket2::{Domain, Protocol, Type};
use socket2::{SockAddr, Socket};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::{
    fmt,
    io::ErrorKind,
    mem::MaybeUninit,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

const ID_VALUE: u16 = 0x4d54; // "MT" - distinct from hopcount's identifier, both may run at once
const MIN_MTU_V4: u16 = 576; // RFC 1122 minimum reassembly buffer, the practical floor
const MIN_MTU_V6: u16 = 1280; // RFC 8200 minimum link MTU
const DEFAULT_MAX_MTU: u16 = 1500; // upper bound when the kernel can't tell us the route MTU
const MAX_PROBES: u32 = 16; // bisecting 576..1500 needs ~10; router hints need far fewer
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
saw. Returns the largest size that got a reply and whether any probe
vanished silently on the way (black hole).
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
    for _ in 0..max_probes {
        let mut hint: Option<u16> = None;
        match probe(candidate)? {
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
            pkt.set_identifier(ID_VALUE);
            pkt.set_sequence_number(self.seq);
            let checksum = icmp::checksum(&IcmpPacket::new(pkt.packet()).ok_or(ERR_CKSUM)?);
            pkt.set_checksum(checksum);
        } else {
            // checksum left to the kernel, as in hopcount (RFC 3542)
            let mut pkt = MutableEchoRequestPacketV6::new(&mut buf).ok_or(ERR_PACKET)?;
            pkt.set_icmpv6_type(Icmpv6Types::EchoRequest);
            pkt.set_identifier(ID_VALUE);
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
            let verdict: Option<ProbeResult> = if self.v4 {
                classify_v4(self.seq, data, from_target)
            } else {
                classify_v6(self.seq, data, from_target)
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

/// Is this raw-socket datagram the verdict on our probe `seq`? IPv4 raw
/// sockets deliver the IP header too; honor the IHL field.
fn classify_v4(seq: u16, data: &[u8], from_target: bool) -> Option<ProbeResult> {
    let ihl: usize = ((*data.first()? & 0x0f) as usize) * 4;
    if ihl < IPV4_HEADER_MIN || data.len() < ihl + ICMP_HEADER_SIZE {
        return None;
    }
    let msg: &[u8] = &data[ihl..];
    let resp = IcmpPacket::new(msg)?;
    match resp.get_icmp_type() {
        IcmpTypes::EchoReply => {
            let reply = EchoReplyPacket::new(msg)?;
            (from_target
                && reply.get_identifier() == ID_VALUE
                && reply.get_sequence_number() == seq)
                .then_some(ProbeResult::Reply)
        }
        IcmpTypes::DestinationUnreachable => {
            if embedded_echo_v4(msg) != Some((ID_VALUE, seq)) {
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

/// Is this raw-socket datagram the verdict on our probe `seq`? IPv6 raw
/// sockets deliver only the ICMPv6 message.
fn classify_v6(seq: u16, data: &[u8], from_target: bool) -> Option<ProbeResult> {
    if data.len() < ICMP_HEADER_SIZE {
        return None;
    }
    let resp = Icmpv6Packet::new(data)?;
    match resp.get_icmpv6_type() {
        Icmpv6Types::EchoReply => {
            let reply = EchoReplyPacketV6::new(data)?;
            (from_target
                && reply.get_identifier() == ID_VALUE
                && reply.get_sequence_number() == seq)
                .then_some(ProbeResult::Reply)
        }
        Icmpv6Types::PacketTooBig => {
            if embedded_echo_v6(data) != Some((ID_VALUE, seq)) {
                return None;
            }
            // RFC 4443: MTU of the constricting link in the 32 bits after the header
            let mtu: u32 = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            Some(ProbeResult::TooBig(
                u16::try_from(mtu).ok().filter(|m| *m > 0),
            ))
        }
        Icmpv6Types::DestinationUnreachable => {
            (embedded_echo_v6(data) == Some((ID_VALUE, seq))).then_some(ProbeResult::Unreachable)
        }
        _ => None,
    }
}

/* ================================= tests ================================== */

#[cfg(test)]
mod tests {
    use super::*;

    const SEQ: u16 = 7;

    /// 20-byte IPv4 header (no options) in front of an ICMP message.
    fn v4_datagram(icmp: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; IPV4_HEADER_MIN];
        d[0] = 0x45;
        d.extend_from_slice(icmp);
        d
    }

    fn echo(kind: u8, ident: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 0];
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&seq.to_be_bytes());
        m.extend_from_slice(&[0u8; 8]); // some payload
        m
    }

    /// ICMPv4 error `[type, code, csum, word]` quoting our echo request.
    fn v4_error(code: u8, word: u32, ident: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![3, code, 0, 0];
        m.extend_from_slice(&word.to_be_bytes());
        m.extend_from_slice(&v4_datagram(&echo(8, ident, seq)));
        m
    }

    /// ICMPv6 error `[type, code, csum, word]` quoting our echo request.
    fn v6_error(kind: u8, word: u32, ident: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 0];
        m.extend_from_slice(&word.to_be_bytes());
        m.extend_from_slice(&[0u8; IPV6_HEADER_SIZE]);
        m.extend_from_slice(&echo(128, ident, seq));
        m
    }

    #[test]
    fn classify_v4_replies_and_errors() {
        let reply = v4_datagram(&echo(0, ID_VALUE, SEQ));
        assert_eq!(classify_v4(SEQ, &reply, true), Some(ProbeResult::Reply));
        assert_eq!(classify_v4(SEQ, &reply, false), None, "not from the target");
        assert_eq!(classify_v4(SEQ + 1, &reply, true), None, "stale sequence");
        let other = v4_datagram(&echo(0, 0xb00b, SEQ));
        assert_eq!(classify_v4(SEQ, &other, true), None, "hopcount's reply");

        let frag = v4_datagram(&v4_error(4, 1400, ID_VALUE, SEQ));
        assert_eq!(
            classify_v4(SEQ, &frag, false),
            Some(ProbeResult::TooBig(Some(1400)))
        );
        let frag0 = v4_datagram(&v4_error(4, 0, ID_VALUE, SEQ));
        assert_eq!(
            classify_v4(SEQ, &frag0, false),
            Some(ProbeResult::TooBig(None))
        );
        let unreach = v4_datagram(&v4_error(1, 0, ID_VALUE, SEQ));
        assert_eq!(
            classify_v4(SEQ, &unreach, false),
            Some(ProbeResult::Unreachable)
        );
        let foreign = v4_datagram(&v4_error(4, 1400, 0xb00b, SEQ));
        assert_eq!(
            classify_v4(SEQ, &foreign, false),
            None,
            "someone else's probe"
        );
        let stale = v4_datagram(&v4_error(4, 1400, ID_VALUE, SEQ - 1));
        assert_eq!(
            classify_v4(SEQ, &stale, false),
            None,
            "error for an earlier probe"
        );

        assert_eq!(classify_v4(SEQ, &[], true), None);
        assert_eq!(classify_v4(SEQ, &[0x45, 0, 0], true), None, "truncated");
    }

    #[test]
    fn classify_v6_replies_and_errors() {
        let reply = echo(129, ID_VALUE, SEQ);
        assert_eq!(classify_v6(SEQ, &reply, true), Some(ProbeResult::Reply));
        assert_eq!(classify_v6(SEQ, &reply, false), None);

        let too_big = v6_error(2, 1280, ID_VALUE, SEQ);
        assert_eq!(
            classify_v6(SEQ, &too_big, false),
            Some(ProbeResult::TooBig(Some(1280)))
        );
        let huge = v6_error(2, 70_000, ID_VALUE, SEQ);
        assert_eq!(
            classify_v6(SEQ, &huge, false),
            Some(ProbeResult::TooBig(None))
        );
        let unreach = v6_error(1, 0, ID_VALUE, SEQ);
        assert_eq!(
            classify_v6(SEQ, &unreach, false),
            Some(ProbeResult::Unreachable)
        );
        let foreign = v6_error(2, 1280, 0xb00b, SEQ);
        assert_eq!(classify_v6(SEQ, &foreign, false), None);
        assert_eq!(classify_v6(SEQ, &[2, 0, 0], false), None, "truncated");
    }

    /// Run the search against a simulated path; returns (result, probe sizes sent).
    fn simulate(
        min: u16,
        upper: u16,
        path: impl Fn(u16) -> ProbeResult,
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
        assert_eq!(sent, vec![1500, MIN_MTU_V4]);
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
        assert_eq!(sent, vec![1500, MIN_MTU_V4]);
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

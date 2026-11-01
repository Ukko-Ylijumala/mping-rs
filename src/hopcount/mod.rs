// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{strings::*, structs::DEFAULT_PAYLOAD_SIZE};
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
    ip::IpNextHeaderProtocols,
};
use rand::random;
use socket2::{self, Domain, Protocol, Socket, Type};
use std::{
    io::ErrorKind,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

const SEQ_VALUE: u16 = 1; // sequence number for the single probe
/**
Top bit of the (otherwise random) Echo identifier: clear on hopcount probes,
set on path MTU probes. Enter fires both at once towards the same target and
both start at sequence 1, so their identifiers must never collide.
*/
pub(crate) const IDENT_PMTU_BIT: u16 = 0x8000;
pub(crate) const ICMP_HEADER_SIZE: usize = 8; // ICMP(v4/v6) header size
pub(crate) const IPV4_HEADER_MIN: usize = 20; // IPv4 header size without options
pub(crate) const IPV6_HEADER_SIZE: usize = 40; // IPv6 header size (fixed)
const INNER_V4_PROTO: usize = 9; // protocol field offset in an IPv4 header
pub(crate) const INNER_V4_DST: usize = 16; // destination address offset in an IPv4 header
const INNER_V6_NEXT: usize = 6; // next header field offset in an IPv6 header
pub(crate) const INNER_V6_DST: usize = 24; // destination address offset in an IPv6 header
const RECV_BUF_SIZE: usize = 1500;
/// Shortest read timeout we hand to the socket. Anything under 1 µs becomes
/// `timeval {0, 0}`, which `SO_RCVTIMEO` treats as "block forever".
const MIN_READ_TIMEOUT: Duration = Duration::from_millis(1);

/**
Estimate hop count for a single target using one ICMP Echo Request/Reply.
Returns `(estimated_hops, received_ttl)` on success.

A raw ICMP socket receives *all* inbound ICMP traffic, so replies are
filtered: only an Echo Reply from `target` carrying our (random per call)
identifier and sequence number is accepted, and a Destination Unreachable
only when it quotes that probe towards `target` (other packets are skipped
until `timeout`).

NOTE: IPv4 reads the TTL from the included IP header; IPv6 raw sockets do
not include the IP header in received data, so the hop limit is obtained
via `IPV6_RECVHOPLIMIT` ancillary data instead.
*/
pub fn determine_hops(target: IpAddr, timeout: Duration, debug: bool) -> Result<(u8, u8), String> {
    match target {
        IpAddr::V4(v4) => determine_hops_v4(v4, timeout, debug),
        IpAddr::V6(v6) => determine_hops_v6(v6, timeout, debug),
    }
}

/// Estimate the sender's initial TTL / hop limit from the received value.
/// Common initial values are 64 (Linux, macOS), 128 (Windows) and 255 (network gear).
#[inline]
pub(crate) fn estimate_hops(received_ttl: u8) -> u8 {
    if received_ttl > 128 {
        255 - received_ttl
    } else if received_ttl > 64 {
        128 - received_ttl
    } else {
        64 - received_ttl
    }
}

/// Time left until `deadline`, or a timeout error if (nearly) passed.
/// See [MIN_READ_TIMEOUT] for why "nearly" matters.
#[inline]
pub(crate) fn time_left(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| *d >= MIN_READ_TIMEOUT)
        .ok_or_else(|| TIMEOUT.to_string())
}

/// Map a receive error: timeouts get the canonical string, the rest are wrapped.
#[inline]
fn recv_err(e: std::io::Error) -> String {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => TIMEOUT.to_string(),
        _ => format!("{ERR_RECV}: {e}"),
    }
}

/// Create and bind a raw ICMP socket for the given IP version.
pub(crate) fn make_socket(v4: bool, debug: bool) -> Result<Socket, String> {
    let (domain, proto, bind) = if v4 {
        (Domain::IPV4, Protocol::ICMPV4, BIND_SOCKET_IPV4)
    } else {
        (Domain::IPV6, Protocol::ICMPV6, BIND_SOCKET_IPV6)
    };
    let local_addr: SocketAddr = bind.parse().unwrap();

    // Create raw ICMP socket (requires CAP_NET_RAW or root)
    let socket =
        Socket::new(domain, Type::RAW, Some(proto)).map_err(|e| format!("{ERR_SOCK_RAW}: {e}"))?;
    socket
        .bind(&local_addr.into())
        .map_err(|e| format!("{ERR_SOCK_BIND}: {e}"))?;

    if debug {
        eprintln!("{INFO_SOCKET}{}: {socket:?}", if v4 { "4" } else { "6" });
    }
    Ok(socket)
}

/* -------------------------------- IPv4 ----------------------------------- */

/**
Identifier and sequence number of the ICMP Echo Request embedded in an
ICMPv4 error message (`[8B ICMP error hdr][inner IPv4 hdr][inner ICMP hdr...]`),
if parseable. `None` when the quoted packet is not an ICMP Echo Request at
all (e.g. TCP or UDP, whose bytes 4..8 would otherwise pass for an ident).
*/
pub(crate) fn embedded_echo_v4(icmp_msg: &[u8]) -> Option<(u16, u16)> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    let ihl: usize = ((*inner_ip.first()? & 0x0f) as usize) * 4;
    if ihl < IPV4_HEADER_MIN || *inner_ip.get(INNER_V4_PROTO)? != IpNextHeaderProtocols::Icmp.0 {
        return None;
    }
    let inner_icmp: &[u8] = inner_ip.get(ihl..ihl + ICMP_HEADER_SIZE)?;
    if inner_icmp[0] != IcmpTypes::EchoRequest.0 {
        return None;
    }
    Some((
        u16::from_be_bytes([inner_icmp[4], inner_icmp[5]]),
        u16::from_be_bytes([inner_icmp[6], inner_icmp[7]]),
    ))
}

/// Destination of the IPv4 packet quoted in an ICMPv4 error message.
fn embedded_dst_v4(icmp_msg: &[u8]) -> Option<Ipv4Addr> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    let bytes: &[u8] = inner_ip.get(INNER_V4_DST..INNER_V4_DST + 4)?;
    Some(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?))
}

/**
Does this ICMPv4 error message quote our Echo Request `(ident, seq)` towards
`target`? A raw socket sees every ICMP error on the host, including those for
other targets' probes with the same identifier and sequence number, so the
quoted destination must match too. Shared with the PMTU prober and traceroute.
*/
pub(crate) fn quotes_probe_v4(icmp_msg: &[u8], target: Ipv4Addr, ident: u16, seq: u16) -> bool {
    embedded_echo_v4(icmp_msg) == Some((ident, seq)) && embedded_dst_v4(icmp_msg) == Some(target)
}

fn determine_hops_v4(target: Ipv4Addr, timeout: Duration, debug: bool) -> Result<(u8, u8), String> {
    let ident: u16 = random::<u16>() & !IDENT_PMTU_BIT;

    // Build an ICMP Echo Request packet in a stack buffer
    let mut icmp_buffer = [0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    let mut echo_packet = MutableEchoRequestPacket::new(&mut icmp_buffer).ok_or(ERR_PACKET)?;
    echo_packet.set_icmp_type(IcmpTypes::EchoRequest);
    echo_packet.set_identifier(ident);
    echo_packet.set_sequence_number(SEQ_VALUE);

    // pnet's checksum expects an IcmpPacket wrapper, so build one from the echo packet bytes
    let icmp_packet = IcmpPacket::new(echo_packet.packet()).ok_or(ERR_CKSUM)?;
    let checksum = icmp::checksum(&icmp_packet);
    echo_packet.set_checksum(checksum);
    drop(echo_packet); // drop the wrapper

    if debug {
        eprintln!("ICMP hdr: {:x?}", &icmp_buffer[..ICMP_HEADER_SIZE]);
    }

    let socket: Socket = make_socket(true, debug)?;
    let target_sockaddr: socket2::SockAddr = SocketAddr::new(IpAddr::V4(target), 0).into();

    if debug {
        eprintln!("{INFO_SEND} {target}");
    }
    socket
        .send_to(&icmp_buffer, &target_sockaddr)
        .map_err(|e| format!("{ERR_SEND}: {e}"))?;

    // Receive loop: skip unrelated ICMP traffic until our reply or the deadline.
    let deadline: Instant = Instant::now() + timeout;
    let mut recv_buffer = [const { MaybeUninit::<u8>::uninit() }; RECV_BUF_SIZE];
    loop {
        socket
            .set_read_timeout(Some(time_left(deadline)?))
            .map_err(|e| format!("{ERR_SOCK_TIMEOUT}: {e}"))?;
        let (bytes_read, from) = socket.recv_from(&mut recv_buffer).map_err(recv_err)?;

        // SAFETY: recv_from initialized the first `bytes_read` bytes.
        let data: &[u8] =
            unsafe { std::slice::from_raw_parts(recv_buffer.as_ptr().cast(), bytes_read) };

        // IPv4 raw sockets include the IP header; honor the actual IHL field.
        let Some(first) = data.first() else { continue };
        let ihl: usize = ((first & 0x0f) as usize) * 4;
        if ihl < IPV4_HEADER_MIN || data.len() < ihl + ICMP_HEADER_SIZE {
            continue; // truncated or not for us
        }
        let icmp_msg: &[u8] = &data[ihl..];
        let Some(resp) = IcmpPacket::new(icmp_msg) else {
            continue;
        };

        match resp.get_icmp_type() {
            IcmpTypes::EchoReply => {
                let Some(reply) = EchoReplyPacket::new(icmp_msg) else {
                    continue;
                };
                let from_target: bool = from
                    .as_socket()
                    .is_some_and(|sa| sa.ip() == IpAddr::V4(target));
                if !from_target
                    || reply.get_identifier() != ident
                    || reply.get_sequence_number() != SEQ_VALUE
                {
                    if debug {
                        eprintln!("skipping unrelated echo reply from {from:?}");
                    }
                    continue;
                }
                if debug {
                    eprintln!("Received {bytes_read} bytes back: {data:x?}");
                }
                let received_ttl: u8 = data[8]; // TTL is at offset 8 in the IPv4 header
                return Ok((estimate_hops(received_ttl), received_ttl));
            }
            IcmpTypes::DestinationUnreachable => {
                // Only ours if the embedded original packet is our probe to our target
                if quotes_probe_v4(icmp_msg, target, ident, SEQ_VALUE) {
                    return Err(ERR_UNREACH.to_string());
                }
            }
            other => {
                if debug {
                    eprintln!("{ERR_ICMPTYPE} {other:?} - skipping");
                }
            }
        }
    }
}

/* -------------------------------- IPv6 ----------------------------------- */

/**
Identifier and sequence number of the ICMPv6 Echo Request embedded in an
ICMPv6 error message (`[8B ICMPv6 error hdr][inner IPv6 hdr][inner ICMPv6 hdr...]`),
if parseable. `None` when the quoted packet is not an ICMPv6 Echo Request
directly after the IPv6 header (our probes carry no extension headers).
*/
pub(crate) fn embedded_echo_v6(icmp_msg: &[u8]) -> Option<(u16, u16)> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    if *inner_ip.get(INNER_V6_NEXT)? != IpNextHeaderProtocols::Icmpv6.0 {
        return None;
    }
    let inner_icmp: &[u8] = inner_ip.get(IPV6_HEADER_SIZE..IPV6_HEADER_SIZE + ICMP_HEADER_SIZE)?;
    if inner_icmp[0] != Icmpv6Types::EchoRequest.0 {
        return None;
    }
    Some((
        u16::from_be_bytes([inner_icmp[4], inner_icmp[5]]),
        u16::from_be_bytes([inner_icmp[6], inner_icmp[7]]),
    ))
}

/// Destination of the IPv6 packet quoted in an ICMPv6 error message.
fn embedded_dst_v6(icmp_msg: &[u8]) -> Option<Ipv6Addr> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    let bytes: &[u8] = inner_ip.get(INNER_V6_DST..INNER_V6_DST + 16)?;
    Some(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?))
}

/// Same as [quotes_probe_v4] for an ICMPv6 error message.
pub(crate) fn quotes_probe_v6(icmp_msg: &[u8], target: Ipv6Addr, ident: u16, seq: u16) -> bool {
    embedded_echo_v6(icmp_msg) == Some((ident, seq)) && embedded_dst_v6(icmp_msg) == Some(target)
}

/// `setsockopt` with an `int` value, error text included. Shared with the
/// path MTU prober for its Don't Fragment options.
pub(crate) fn set_sockopt_int(
    socket: &Socket,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> Result<(), String> {
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            (&raw const value).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    match ret {
        0 => Ok(()),
        _ => Err(format!("{ERR_SOCK_OPT}: {}", std::io::Error::last_os_error())),
    }
}

/// Ask the kernel to deliver the hop limit of received packets as ancillary data.
fn enable_recv_hoplimit(socket: &Socket) -> Result<(), String> {
    set_sockopt_int(socket, libc::IPPROTO_IPV6, libc::IPV6_RECVHOPLIMIT, 1)
}

/**
`recvmsg` wrapper for ICMPv6 raw sockets. Returns the number of bytes read,
the source address (if it was an IPv6 one) and the received packet's hop
limit from the `IPV6_HOPLIMIT` control message (if present).

This goes through libc because socket2 exposes no portable way to parse
control messages, and the hop limit is only available as ancillary data.
*/
fn recvmsg_v6(socket: &Socket, buf: &mut [u8]) -> std::io::Result<(usize, Option<Ipv6Addr>, Option<u8>)> {
    // u64 array => suitably aligned for cmsghdr on both Linux and macOS
    let mut control = [0u64; 16];
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = (&raw mut addr).cast();
    msg.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;

    let n: isize = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let src: Option<Ipv6Addr> = (addr.ss_family == libc::AF_INET6 as libc::sa_family_t)
        .then(|| {
            let sin6: &libc::sockaddr_in6 = unsafe { &*(&raw const addr).cast() };
            Ipv6Addr::from(sin6.sin6_addr.s6_addr)
        });

    let mut hoplimit: Option<u8> = None;
    unsafe {
        let mut cmsg: *mut libc::cmsghdr = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::IPPROTO_IPV6
                && (*cmsg).cmsg_type == libc::IPV6_HOPLIMIT
            {
                let data = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                hoplimit = Some((*data).clamp(0, 255) as u8);
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    Ok((n as usize, src, hoplimit))
}

fn determine_hops_v6(target: Ipv6Addr, timeout: Duration, debug: bool) -> Result<(u8, u8), String> {
    /*
    Build an ICMPv6 Echo Request. The checksum is left at zero on purpose:
    it covers an IPv6 pseudo-header we don't know yet, and the kernel always
    computes/inserts it for ICMPv6 raw sockets (RFC 3542).
    */
    let ident: u16 = random::<u16>() & !IDENT_PMTU_BIT;
    let mut icmp_buffer = [0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    let mut echo_packet = MutableEchoRequestPacketV6::new(&mut icmp_buffer).ok_or(ERR_PACKET)?;
    echo_packet.set_icmpv6_type(Icmpv6Types::EchoRequest);
    echo_packet.set_identifier(ident);
    echo_packet.set_sequence_number(SEQ_VALUE);
    drop(echo_packet); // drop the wrapper

    if debug {
        eprintln!("ICMPv6 hdr: {:x?}", &icmp_buffer[..ICMP_HEADER_SIZE]);
    }

    let socket: Socket = make_socket(false, debug)?;
    enable_recv_hoplimit(&socket)?;
    let target_sockaddr: socket2::SockAddr = SocketAddr::new(IpAddr::V6(target), 0).into();

    if debug {
        eprintln!("{INFO_SEND} {target}");
    }
    socket
        .send_to(&icmp_buffer, &target_sockaddr)
        .map_err(|e| format!("{ERR_SEND}: {e}"))?;

    // Receive loop. NOTE: ICMPv6 raw sockets deliver *only* the ICMPv6 message,
    // without the IPv6 header — the hop limit comes from ancillary data.
    let deadline: Instant = Instant::now() + timeout;
    let mut recv_buffer = [0u8; RECV_BUF_SIZE];
    loop {
        socket
            .set_read_timeout(Some(time_left(deadline)?))
            .map_err(|e| format!("{ERR_SOCK_TIMEOUT}: {e}"))?;
        let (bytes_read, src, hoplimit) =
            recvmsg_v6(&socket, &mut recv_buffer).map_err(recv_err)?;

        let icmp_msg: &[u8] = &recv_buffer[..bytes_read];
        let Some(resp) = Icmpv6Packet::new(icmp_msg) else {
            continue;
        };

        match resp.get_icmpv6_type() {
            Icmpv6Types::EchoReply => {
                let Some(reply) = EchoReplyPacketV6::new(icmp_msg) else {
                    continue;
                };
                if src != Some(target)
                    || reply.get_identifier() != ident
                    || reply.get_sequence_number() != SEQ_VALUE
                {
                    if debug {
                        eprintln!("skipping unrelated ICMPv6 packet from {src:?}");
                    }
                    continue;
                }
                if debug {
                    eprintln!("Received {bytes_read} bytes back: {icmp_msg:x?}");
                }
                let received_ttl: u8 = hoplimit.ok_or(ERR_NO_HOPLIMIT)?;
                return Ok((estimate_hops(received_ttl), received_ttl));
            }
            Icmpv6Types::DestinationUnreachable => {
                // Only ours if the embedded original packet is our probe to our target
                if quotes_probe_v6(icmp_msg, target, ident, SEQ_VALUE) {
                    return Err(ERR_UNREACH.to_string());
                }
            }
            other => {
                if debug {
                    eprintln!("{ERR_ICMPTYPE} {other:?} - skipping");
                }
            }
        }
    }
}

/*
AI slop below, disregard this block! I'm leaving it here for posterity. Teaches
me right for trusting AI to generate code for something I don't know well enough.

To be fair, it generated a passable function scaffold, but it also wanted to add
an IP header around the ICMP packet, which appears to be unnecessary here.
Technically, it seems that the wrapping works, but it leads to a malformed ICMP
packet on the wire, f.ex. tcpdump shows this:

    ICMP type-#69, length 76

which is wrong; ICMP type should be 8 (Echo Req). The generated headers start with:

    [45, 0, 0, 4c, b0, a, 0, 0, 40, 1, ...]

and this 69 (0x45) is the first byte of the IP header (version + IHL). Looks like
the kernel wraps this in yet another IP header, leading to confusion.

*sigh* a few hours wasted chasing this down...
--------

use pnet_packet::{ip::IpNextHeaderProtocols, ipv4::{self, MutableIpv4Packet}};

const IP_SOURCE_ADDR: &str = "0.0.0.0"; // OS will fill correct source address
const IP_PACKET_SIZE: usize = IP_HEADER_SIZE + ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE;

    // For IPv4 we need to wrap in IP header (required for raw sockets on some platforms)
    // For simplicity, we'll handle IPv4 explicitly; IPv6 raw sockets often work without IP header.
    let packet_to_send: Vec<u8> = if target.is_ipv4() {
        let mut ip_buffer = vec![0u8; IP_PACKET_SIZE];
        let mut ip_packet =
            MutableIpv4Packet::new(&mut ip_buffer).ok_or("Failed to create IPv4 packet")?;

        ip_packet.set_version(4);
        ip_packet.set_header_length(5); // no options
        ip_packet.set_total_length(IP_PACKET_SIZE as u16);
        ip_packet.set_ttl(SYSTEM_TTL); // outgoing TTL
        ip_packet.set_identification(ID_VALUE);
        ip_packet.set_next_level_protocol(IpNextHeaderProtocols::Icmp);

        ip_packet.set_source(IP_SOURCE_ADDR.parse().unwrap());
        ip_packet.set_destination(target.to_string().parse().unwrap());

        ip_packet.set_payload(&icmp_buffer);
        ip_packet.set_checksum(ipv4::checksum(&ip_packet.to_immutable()));

        ip_packet.packet().to_vec()
    } else {
        // IPv6 raw sockets usually accept just the ICMPv6 payload
        icmp_buffer.to_vec()
    };
    */

/* ================================= tests ================================== */

/// Hand-built packet fixtures, shared with the PMTU and traceroute tests.
#[cfg(test)]
pub(crate) mod test_packets {
    use super::*;

    /// An IPv4 header (no options) carrying ICMP, with the given destination.
    pub(crate) fn ip4_header(dst: Ipv4Addr) -> Vec<u8> {
        let mut h = vec![0u8; IPV4_HEADER_MIN];
        h[0] = 0x45;
        h[INNER_V4_PROTO] = IpNextHeaderProtocols::Icmp.0;
        h[INNER_V4_DST..INNER_V4_DST + 4].copy_from_slice(&dst.octets());
        h
    }

    /// An IPv6 header carrying ICMPv6, with the given destination.
    pub(crate) fn ip6_header(dst: Ipv6Addr) -> Vec<u8> {
        let mut h = vec![0u8; IPV6_HEADER_SIZE];
        h[0] = 0x60;
        h[INNER_V6_NEXT] = IpNextHeaderProtocols::Icmpv6.0;
        h[INNER_V6_DST..INNER_V6_DST + 16].copy_from_slice(&dst.octets());
        h
    }

    /// An ICMP(v6) echo header `[type, 0, csum, ident, seq]`.
    pub(crate) fn echo(kind: u8, ident: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 0];
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&seq.to_be_bytes());
        m
    }

    /// ICMPv4 error message `[type, code, csum, word]` quoting `inner` behind
    /// an IPv4 header to `dst` (no outer IP header).
    pub(crate) fn v4_error_msg(
        kind: u8,
        code: u8,
        word: u32,
        dst: Ipv4Addr,
        inner: &[u8],
    ) -> Vec<u8> {
        let mut m = vec![kind, code, 0, 0];
        m.extend_from_slice(&word.to_be_bytes());
        m.extend(ip4_header(dst));
        m.extend_from_slice(inner);
        m
    }

    /// ICMPv6 error message `[type, code, csum, word]` quoting `inner` behind
    /// an IPv6 header to `dst`.
    pub(crate) fn v6_error_msg(
        kind: u8,
        code: u8,
        word: u32,
        dst: Ipv6Addr,
        inner: &[u8],
    ) -> Vec<u8> {
        let mut m = vec![kind, code, 0, 0];
        m.extend_from_slice(&word.to_be_bytes());
        m.extend(ip6_header(dst));
        m.extend_from_slice(inner);
        m
    }
}

#[cfg(test)]
mod tests {
    use super::{test_packets::*, *};

    const IDENT: u16 = 0x1234;
    const T4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const T6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

    #[test]
    fn embedded_v4_quotes_and_destination() {
        let msg = v4_error_msg(3, 1, 0, T4, &echo(8, IDENT, 5));
        assert_eq!(embedded_echo_v4(&msg), Some((IDENT, 5)));
        assert_eq!(embedded_dst_v4(&msg), Some(T4));
        assert!(quotes_probe_v4(&msg, T4, IDENT, 5));
        assert!(!quotes_probe_v4(&msg, T4, IDENT, 6), "other seq");
        assert!(!quotes_probe_v4(&msg, T4, IDENT + 1, 5), "other ident");
        let other = v4_error_msg(3, 1, 0, Ipv4Addr::new(192, 0, 2, 2), &echo(8, IDENT, 5));
        assert!(!quotes_probe_v4(&other, T4, IDENT, 5), "other target");

        // the quoted packet must be an ICMP Echo Request
        let reply = v4_error_msg(3, 1, 0, T4, &echo(0, IDENT, 5));
        assert_eq!(embedded_echo_v4(&reply), None, "quoted echo reply");
        let mut tcp = v4_error_msg(3, 1, 0, T4, &echo(8, IDENT, 5));
        tcp[ICMP_HEADER_SIZE + INNER_V4_PROTO] = IpNextHeaderProtocols::Tcp.0;
        assert_eq!(embedded_echo_v4(&tcp), None, "quoted TCP segment");
        assert!(!quotes_probe_v4(&tcp, T4, IDENT, 5));

        assert_eq!(embedded_echo_v4(&msg[..msg.len() - 1]), None, "truncated");
        assert_eq!(embedded_dst_v4(&msg[..ICMP_HEADER_SIZE + 4]), None);
    }

    #[test]
    fn embedded_v6_quotes_and_destination() {
        let msg = v6_error_msg(1, 0, 0, T6, &echo(128, IDENT, 5));
        assert_eq!(embedded_echo_v6(&msg), Some((IDENT, 5)));
        assert_eq!(embedded_dst_v6(&msg), Some(T6));
        assert!(quotes_probe_v6(&msg, T6, IDENT, 5));
        assert!(!quotes_probe_v6(&msg, T6, IDENT, 6), "other seq");
        let other = v6_error_msg(1, 0, 0, Ipv6Addr::LOCALHOST, &echo(128, IDENT, 5));
        assert!(!quotes_probe_v6(&other, T6, IDENT, 5), "other target");

        let reply = v6_error_msg(1, 0, 0, T6, &echo(129, IDENT, 5));
        assert_eq!(embedded_echo_v6(&reply), None, "quoted echo reply");
        let mut udp = v6_error_msg(1, 0, 0, T6, &echo(128, IDENT, 5));
        udp[ICMP_HEADER_SIZE + INNER_V6_NEXT] = IpNextHeaderProtocols::Udp.0;
        assert_eq!(embedded_echo_v6(&udp), None, "quoted UDP datagram");

        assert_eq!(embedded_echo_v6(&msg[..msg.len() - 1]), None, "truncated");
    }
}

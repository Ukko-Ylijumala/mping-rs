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
};
use socket2::{self, Domain, Protocol, Socket, Type};
use std::{
    io::ErrorKind,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

const ID_VALUE: u16 = 0xb00b; // (very!) arbitrary identifier
const SEQ_VALUE: u16 = 1; // sequence number for the single probe
const ICMP_HEADER_SIZE: usize = 8; // ICMP(v4/v6) header size
const IPV4_HEADER_MIN: usize = 20; // IPv4 header size without options
const IPV6_HEADER_SIZE: usize = 40; // IPv6 header size (fixed)
const RECV_BUF_SIZE: usize = 1500;

/**
Estimate hop count for a single target using one ICMP Echo Request/Reply.
Returns `(estimated_hops, received_ttl)` on success.

A raw ICMP socket receives *all* inbound ICMP traffic, so replies are
filtered: only an Echo Reply from `target` carrying our identifier and
sequence number is accepted (other packets are skipped until `timeout`).

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
fn estimate_hops(received_ttl: u8) -> u8 {
    if received_ttl > 128 {
        255 - received_ttl
    } else if received_ttl > 64 {
        128 - received_ttl
    } else {
        64 - received_ttl
    }
}

/// Time left until `deadline`, or a timeout error if it has passed.
#[inline]
fn time_left(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
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
fn make_socket(v4: bool, debug: bool) -> Result<Socket, String> {
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

/// Identifier of the ICMP Echo Request embedded in an ICMPv4 error message
/// (`[8B ICMP error hdr][inner IPv4 hdr][inner ICMP hdr...]`), if parseable.
fn embedded_ident_v4(icmp_msg: &[u8]) -> Option<u16> {
    let inner_ip: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE..)?;
    let ihl: usize = ((*inner_ip.first()? & 0x0f) as usize) * 4;
    if ihl < IPV4_HEADER_MIN {
        return None;
    }
    let inner_icmp: &[u8] = inner_ip.get(ihl..ihl + ICMP_HEADER_SIZE)?;
    Some(u16::from_be_bytes([inner_icmp[4], inner_icmp[5]]))
}

fn determine_hops_v4(target: Ipv4Addr, timeout: Duration, debug: bool) -> Result<(u8, u8), String> {
    // Build an ICMP Echo Request packet in a stack buffer
    let mut icmp_buffer = [0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    let mut echo_packet = MutableEchoRequestPacket::new(&mut icmp_buffer).ok_or(ERR_PACKET)?;
    echo_packet.set_icmp_type(IcmpTypes::EchoRequest);
    echo_packet.set_identifier(ID_VALUE);
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
                    || reply.get_identifier() != ID_VALUE
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
                // Only ours if the embedded original packet carries our identifier
                if embedded_ident_v4(icmp_msg) == Some(ID_VALUE) {
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

/// Identifier of the ICMPv6 Echo Request embedded in an ICMPv6 error message
/// (`[8B ICMPv6 error hdr][inner IPv6 hdr][inner ICMPv6 hdr...]`), if parseable.
fn embedded_ident_v6(icmp_msg: &[u8]) -> Option<u16> {
    let inner_icmp: &[u8] = icmp_msg.get(ICMP_HEADER_SIZE + IPV6_HEADER_SIZE..)?;
    let bytes: &[u8] = inner_icmp.get(4..6)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Ask the kernel to deliver the hop limit of received packets as ancillary data.
fn enable_recv_hoplimit(socket: &Socket) -> Result<(), String> {
    let on: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVHOPLIMIT,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    match ret {
        0 => Ok(()),
        _ => Err(format!("{ERR_SOCK_OPT}: {}", std::io::Error::last_os_error())),
    }
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
    let mut icmp_buffer = [0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    let mut echo_packet = MutableEchoRequestPacketV6::new(&mut icmp_buffer).ok_or(ERR_PACKET)?;
    echo_packet.set_icmpv6_type(Icmpv6Types::EchoRequest);
    echo_packet.set_identifier(ID_VALUE);
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
                    || reply.get_identifier() != ID_VALUE
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
                // Only ours if the embedded original packet carries our identifier
                if embedded_ident_v6(icmp_msg) == Some(ID_VALUE) {
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

// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::structs::DEFAULT_PAYLOAD_SIZE;
use pnet_packet::{
    Packet,
    icmp::{self, IcmpPacket, IcmpTypes, echo_request::MutableEchoRequestPacket},
};
use socket2::{self, Domain, Protocol, Socket, Type};
use std::{
    mem::{MaybeUninit, transmute},
    net::{IpAddr, SocketAddr},
    time::Duration,
};

const BIND_SOCKET_IPV4: &str = "0.0.0.0:0";
const BIND_SOCKET_IPV6: &str = "[::]:0";
const ID_VALUE: u16 = 0xb00b; // (very!) arbitrary identifier
const ICMP_HEADER_SIZE: usize = 8; // ICMP header size
const IP_HEADER_SIZE: usize = 20; // IPv4 header size without options

/// Estimate hop count for a single target using one ICMP Echo Request/Reply.
/// Returns (estimated_hops, received_ttl) on success.
pub fn determine_hops(target: IpAddr, timeout: Duration, debug: bool) -> Result<(u8, u8), String> {
    // Create a mutable buffer and build an ICMP Echo Request packet in it
    let mut icmp_buffer = [0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    // Create a mutable wrapper around the buffer
    let mut echo_packet =
        MutableEchoRequestPacket::new(&mut icmp_buffer).ok_or("Failed to create echo packet")?;

    echo_packet.set_icmp_type(IcmpTypes::EchoRequest);
    echo_packet.set_identifier(ID_VALUE);
    echo_packet.set_sequence_number(1);

    // pnet's checksum expects an IcmpPacket wrapper, so build one from the echo packet bytes
    let icmp_packet = IcmpPacket::new(echo_packet.packet())
        .ok_or("Failed to create IcmpPacket for checksumming")?;
    let checksum = icmp::checksum(&icmp_packet);
    echo_packet.set_checksum(checksum);
    drop(echo_packet); // drop the wrapper

    if debug {
        eprintln!("ICMP hdr: {:x?}", &icmp_buffer[..ICMP_HEADER_SIZE]);
    }

    // Bind to unspecified address (let OS choose source IP)
    let local_addr: SocketAddr = if target.is_ipv4() {
        BIND_SOCKET_IPV4.parse().unwrap()
    } else {
        BIND_SOCKET_IPV6.parse().unwrap()
    };
    let target_sockaddr: socket2::SockAddr = SocketAddr::new(target, 0).into();

    if debug {
        eprintln!(
            "Local address: {}, remote address: {:?}",
            local_addr, target_sockaddr
        );
    }

    // Create raw ICMP socket (requires CAP_NET_RAW or root)
    let (domain, proto) = if target.is_ipv4() {
        (Domain::IPV4, Protocol::ICMPV4)
    } else {
        (Domain::IPV6, Protocol::ICMPV6)
    };
    let socket = Socket::new(domain, Type::RAW, Some(proto))
        .map_err(|e| format!("Failed to create raw socket: {e}"))?;

    if debug {
        eprintln!(
            "Raw socket created for ICMP{}: {:?}",
            if target.is_ipv4() { "v4" } else { "v6" },
            socket
        );
    }

    // Set receive timeout and bind to the local socket
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("Failed to set timeout: {e}"))?;
    socket
        .bind(&local_addr.into())
        .map_err(|e| format!("Bind failed: {e}"))?;

    if debug {
        eprintln!("Sending ICMP Echo Request to {}", target);
    }

    // Send the packet
    socket
        .send_to(&icmp_buffer, &target_sockaddr)
        .map_err(|e| format!("Send failed: {e}"))?;

    let mut recv_buffer: [MaybeUninit<u8>; 1500] = [const { MaybeUninit::uninit() }; 1500];
    let (bytes_read, _from) = socket.recv_from(&mut recv_buffer).map_err(|e| {
        let err_str = e.to_string();
        if err_str.to_lowercase().contains("unavailable") {
            "Timeout".to_string()
        } else {
            format!("Receive failed: {err_str}")
        }
    })?;

    // We can safely assume that the bytes in `recv_buffer` are initialized up to `bytes_read`.
    let recv_data: &[u8] =
        unsafe { transmute::<&[MaybeUninit<u8>], &[u8]>(&recv_buffer[..bytes_read]) };

    if debug {
        eprintln!("Received {} bytes back: {:x?}", bytes_read, &recv_data);
    }

    // Parse the received ICMP packet (from after the IP header)
    let resp = IcmpPacket::new(&recv_data[IP_HEADER_SIZE..]).ok_or("Malformed ICMP packet")?;

    // Error out if it's not an Echo Reply
    if resp.get_icmp_type() != IcmpTypes::EchoReply {
        match resp.get_icmp_type() {
            IcmpTypes::DestinationUnreachable => {
                return Err("Destination Unreachable".to_string());
            }
            _ => return Err(format!("Wanted Echo Reply, got{:?}", resp.get_icmp_type())),
        }
    }

    // Parse IP header to get TTL.
    let received_ttl = if target.is_ipv4() {
        // IPv4: IP header is first 20 bytes (assuming no options)
        if bytes_read < 20 {
            return Err("Truncated IPv4 header".to_string());
        }
        unsafe { recv_buffer[8].assume_init() } // TTL is at offset 8 in IPv4 header
    } else {
        if bytes_read < 40 {
            return Err("Truncated IPv6 header".to_string());
        }
        unsafe { recv_buffer[7].assume_init() } // Hop Limit is at offset 7 in IPv6 header
    };

    // Estimate original TTL and hop count
    let estimated_hops = if received_ttl > 128 {
        255 - received_ttl
    } else if received_ttl > 64 {
        128 - received_ttl
    } else {
        64 - received_ttl
    };

    Ok((estimated_hops, received_ttl))
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

// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::structs::{DEFAULT_PAYLOAD_SIZE, SYSTEM_TTL};
use pnet_packet::{
    Packet,
    icmp::{IcmpTypes, echo_request::MutableEchoRequestPacket},
    ipv4::MutableIpv4Packet,
};
use socket2::{self, Domain, Protocol, Socket, Type};
use std::{
    mem::MaybeUninit,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

const BIND_IPV4: &str = "0.0.0.0:0";
const BIND_IPV6: &str = "[::]:0";
const ID_VALUE: u16 = 0xb00b; // (very!) arbitrary identifier
const ICMP_HEADER_SIZE: usize = 8; // ICMP header size
const IP_HEADER_SIZE: usize = 20; // IPv4 header size without options

/// Estimate hop count for a single target using one ICMP Echo Request/Reply.
/// Returns (estimated_hops, received_ttl) on success.
pub fn determine_hops(target: IpAddr, timeout: Duration) -> Result<(u8, u8), String> {
    // Create a mutable buffer and build an ICMP Echo Request packet in it
    let mut icmp_buffer = [0u8; ICMP_HEADER_SIZE + DEFAULT_PAYLOAD_SIZE];
    let mut echo_packet =
        MutableEchoRequestPacket::new(&mut icmp_buffer).ok_or("Failed to create echo packet")?;

    echo_packet.set_icmp_type(IcmpTypes::EchoRequest);
    echo_packet.set_identifier(ID_VALUE);
    echo_packet.set_sequence_number(1);

    //let checksum = pnet_packet::icmp::checksum(&echo_packet.to_immutable());
    //echo_packet.set_checksum(checksum);

    // For IPv4 we need to wrap in IP header (required for raw sockets on some platforms)
    // For simplicity, we'll handle IPv4 explicitly; IPv6 raw sockets often work without IP header.
    let packet_to_send: Vec<u8> = if target.is_ipv4() {
        let mut ip_buffer = vec![0u8; IP_HEADER_SIZE + icmp_buffer.len()];
        let mut ip_packet =
            MutableIpv4Packet::new(&mut ip_buffer).ok_or("Failed to create IPv4 packet")?;

        ip_packet.set_version(4);
        ip_packet.set_header_length(5); // no options
        ip_packet.set_total_length((IP_HEADER_SIZE + icmp_buffer.len()) as u16);
        ip_packet.set_ttl(SYSTEM_TTL); // outgoing TTL

        //ip_packet.set_protocol(pnet_packet::ip::IpNextHeaderProtocols::Icmp);
        //ip_packet.set_source("0.0.0.0".parse().unwrap()); // OS will fill correct source

        ip_packet.set_destination(target.to_string().parse().unwrap());

        ip_packet.set_payload(&icmp_buffer);
        ip_packet.packet().to_vec()
    } else {
        // IPv6 raw sockets usually accept just the ICMPv6 payload
        icmp_buffer.to_vec()
    };

    // Bind to unspecified address (let OS choose source IP)
    let local_addr: SocketAddr = if target.is_ipv4() {
        BIND_IPV4.parse().unwrap()
    } else {
        BIND_IPV6.parse().unwrap()
    };
    let target_sockaddr: socket2::SockAddr = SocketAddr::new(target, 0).into();

    eprintln!("Local address: {}, remote address: {:?}", local_addr, target_sockaddr);

    // Create raw ICMP socket (requires CAP_NET_RAW or root)
    let socket = Socket::new(
        if target.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        },
        Type::RAW,
        Some(Protocol::ICMPV4), // kernel often uses ICMPV4 proto constant on Linux for IPv6
    )
    .map_err(|e| format!("Failed to create raw socket: {e}"))?;

    eprintln!("Raw socket created for ICMP{}: {:?}", if target.is_ipv4() { "v4" } else { "v6" }, socket);

    // Set receive timeout and bind to the local socket
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("Failed to set timeout: {e}"))?;
    socket
        .bind(&local_addr.into())
        .map_err(|e| format!("Bind failed: {e}"))?;

    eprintln!("Socket bound to {:?}", socket.local_addr().unwrap());
    eprintln!("Sending ICMP Echo Request to {}", target);

    // Send the packet
    socket
        .send_to(&packet_to_send, &target_sockaddr)
        .map_err(|e| format!("Send failed: {e}"))?;

    eprintln!("Waiting for ICMP Echo Reply from {}", target);

    let mut recv_buffer: [MaybeUninit<u8>; 1500] = [const { MaybeUninit::uninit() }; 1500];
    let (bytes_read, _from) = socket
        .recv_from(&mut recv_buffer)
        .map_err(|e| format!("Receive failed: {e}"))?;

    // Parse IP header to get TTL.
    // We can safely assume that the data in `recv_buffer`
    // is now initialized at least up to bytes_read.
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

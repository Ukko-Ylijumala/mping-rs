// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Parse an IP address, CIDR, or IP range from a string.
/// Supported formats:
/// - Single IP: 10.10.10.1
/// - CIDR: 10.10.10.0/28
/// - Short range: 10.10.10.1-10 (last octet range)
/// - Full range: 10.10.10.1-10.10.10.10
pub fn parse_ip_or_range(arg: &str) -> Result<Vec<IpAddr>, String> {
    // Try single IP first
    if let Ok(ip) = arg.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }

    // Try CIDR notation
    if let Ok(network) = arg.parse::<IpNet>() {
        let hosts: Vec<IpAddr> = network.hosts().collect();
        if hosts.is_empty() {
            // For /32 or /128, use the network address itself
            return Ok(vec![network.addr()]);
        }
        return Ok(hosts);
    }

    // Try range notation (10.10.10.1-10 or 10.10.10.1-10.10.10.10)
    if arg.contains('-') {
        return parse_ip_range(arg);
    }

    Err(format!("Invalid IP address, CIDR, or range: {arg}"))
}

/// Parse an IP range in the format:
/// - 10.10.10.1-10 (short form, last octet only)
/// - 10.10.10.1-10.10.10.10 (full form)
pub fn parse_ip_range(arg: &str) -> Result<Vec<IpAddr>, String> {
    let parts: Vec<&str> = arg.split('-').collect();
    if parts.len() != 2 {
        return Err(format!("Invalid range format: {arg}"));
    }

    let start_str: &str = parts[0].trim();
    let end_str: &str = parts[1].trim();

    // Parse the start IP
    let start_ip: IpAddr = start_str
        .parse::<IpAddr>()
        .map_err(|_| format!("Invalid start IP in range: {start_str}"))?;

    // Determine if this is short form (just a number) or full IP
    let end_ip: IpAddr = if end_str.contains('.') || end_str.contains(':') {
        // Full IP form
        end_str
            .parse::<IpAddr>()
            .map_err(|_| format!("Invalid end IP in range: {end_str}"))?
    } else {
        // Short form - parse as last octet/hextet
        parse_short_range_end(&start_ip, end_str)?
    };

    // Validate same IP version
    match (start_ip, end_ip) {
        (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => {
            return Err("Cannot mix IPv4 and IPv6 in range".to_string());
        }
        _ => {}
    }

    generate_ip_range(start_ip, end_ip)
}

/// Parse short-form range end (e.g., "10" in "192.168.1.1-10")
fn parse_short_range_end(start_ip: &IpAddr, end_str: &str) -> Result<IpAddr, String> {
    let end_val: u32 = end_str
        .parse()
        .map_err(|_| format!("Invalid range end value: {end_str}"))?;

    match start_ip {
        IpAddr::V4(start_v4) => {
            if end_val > 255 {
                return Err(format!("IPv4 octet must be <= 255, got: {end_val}"));
            }
            let octets: [u8; 4] = start_v4.octets();
            let new_ip: Ipv4Addr = Ipv4Addr::new(octets[0], octets[1], octets[2], end_val as u8);
            Ok(IpAddr::V4(new_ip))
        }
        IpAddr::V6(start_v6) => {
            if end_val > 65535 {
                return Err(format!("IPv6 hextet must be <= 65535, got: {end_val}"));
            }
            let segments: [u16; 8] = start_v6.segments();
            let mut new_segments: [u16; 8] = segments;
            new_segments[7] = end_val as u16;
            let new_ip: Ipv6Addr = Ipv6Addr::from(new_segments);
            Ok(IpAddr::V6(new_ip))
        }
    }
}

/// Generate all IPs between start and end (inclusive)
pub fn generate_ip_range(start: IpAddr, end: IpAddr) -> Result<Vec<IpAddr>, String> {
    match (start, end) {
        (IpAddr::V4(start_v4), IpAddr::V4(end_v4)) => {
            let start_num: u32 = u32::from(start_v4);
            let end_num: u32 = u32::from(end_v4);

            if start_num > end_num {
                return Err(format!("Start IP {start} is greater than end IP {end}"));
            }

            let count: usize = (end_num - start_num + 1) as usize;
            if count > 65536 {
                return Err(format!("Range too large: {count} addresses (max 65536)"));
            }

            Ok((start_num..=end_num)
                .map(|n: u32| IpAddr::V4(Ipv4Addr::from(n)))
                .collect())
        }
        (IpAddr::V6(start_v6), IpAddr::V6(end_v6)) => {
            let start_num: u128 = u128::from(start_v6);
            let end_num: u128 = u128::from(end_v6);

            if start_num > end_num {
                return Err(format!("Start IP {start} is greater than end IP {end}"));
            }

            let count: u128 = end_num.saturating_sub(start_num).saturating_add(1);
            if count > 65536 {
                return Err(format!("Range too large: {count} addresses (max 65536)"));
            }

            Ok((start_num..=end_num)
                .map(|n: u128| IpAddr::V6(Ipv6Addr::from(n)))
                .collect())
        }
        _ => Err("IP version mismatch in range".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_ip() {
        let result: Vec<IpAddr> = parse_ip_or_range("192.168.1.1").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "192.168.1.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_parse_cidr() {
        let result: Vec<IpAddr> = parse_ip_or_range("192.168.1.0/30").unwrap();
        assert_eq!(result.len(), 2); // .1 and .2 (hosts only)
        assert!(result.contains(&"192.168.1.1".parse::<IpAddr>().unwrap()));
        assert!(result.contains(&"192.168.1.2".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_parse_short_range() {
        let result: Vec<IpAddr> = parse_ip_or_range("10.0.0.1-5").unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(result[4], "10.0.0.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_parse_full_range() {
        let result: Vec<IpAddr> = parse_ip_or_range("10.0.0.1-10.0.0.5").unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(result[4], "10.0.0.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_invalid_range() {
        let result: Result<Vec<IpAddr>, String> = parse_ip_or_range("10.0.0.5-10.0.0.1");
        assert!(result.is_err());
    }

    #[test]
    fn test_ipv6_short_range() {
        let result: Vec<IpAddr> = parse_ip_or_range("::1-5").unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], "::1".parse::<IpAddr>().unwrap());
        assert_eq!(result[4], "::5".parse::<IpAddr>().unwrap());
    }
}

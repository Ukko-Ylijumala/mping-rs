// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::strings::*;
use ipnet::IpNet;
use std::{
    error, fmt,
    net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr},
    num::ParseIntError,
};

const MAX_RANGE_SIZE: usize = 65536; // max number of addresses in a range allowed

/**
Parse an IP address, CIDR, or IP range from a string.
Supported formats:
- Single IP: 10.10.10.1
- CIDR: 10.10.10.0/28
- Short range: 10.10.10.1-10 (last octet range)
- Full range: 10.10.10.1-10.10.10.10
*/
pub fn parse_ip_or_range(arg: &str) -> Result<Vec<IpAddr>, AddressError> {
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
    if arg.contains(MISSING) {
        return parse_ip_range(arg, false);
    }

    Err(AddressError::Invalid(arg.to_string()))
}

/**
Parse an IP range in the format:
- 10.10.10.1-10 (short form, last octet only)
- 10.10.10.1-10.10.10.10 (full form)

If `range` > [MAX_RANGE_SIZE], returns an error, unless `allow_large` is true.
*/
pub fn parse_ip_range(arg: &str, allow_large: bool) -> Result<Vec<IpAddr>, AddressError> {
    let parts: Vec<&str> = arg.split('-').collect();
    if parts.len() != 2 {
        return Err(AddressError::InvalidRangeFmt(arg.into()));
    }

    let start_str: &str = parts[0].trim();
    let end_str: &str = parts[1].trim();

    // Parse the start IP
    let start_ip: IpAddr =
        start_str
            .parse::<IpAddr>()
            .map_err(|source| AddressError::InvalidRangeBegIp {
                beg: start_str.into(),
                source,
            })?;

    // Determine if this is short form (just a number) or full IP
    let end_ip: IpAddr = if end_str.contains('.') || end_str.contains(':') {
        // Full IP form
        end_str
            .parse::<IpAddr>()
            .map_err(|source| AddressError::InvalidRangeEndIp {
                end: end_str.into(),
                source,
            })?
    } else {
        // Short form - parse as last octet/hextet
        parse_short_range_end(&start_ip, end_str)?
    };

    // Validate same IP version
    match (start_ip, end_ip) {
        (IpAddr::V4(a), IpAddr::V6(b)) | (IpAddr::V6(b), IpAddr::V4(a)) => {
            return Err(AddressError::Mismatch(a.into(), b.into()));
        }
        _ => {}
    }

    generate_ip_range(start_ip, end_ip, allow_large)
}

/// Parse short-form range end (e.g., "10" in "192.168.1.1-10")
fn parse_short_range_end(start_ip: &IpAddr, end_str: &str) -> Result<IpAddr, AddressError> {
    let end_val: u32 = end_str
        .parse()
        .map_err(|source| AddressError::InvalidRangeEndVal {
            val: end_str.into(),
            source,
        })?;

    match start_ip {
        IpAddr::V4(start_v4) => {
            if end_val > 255 {
                return Err(AddressError::InvalidV4Octet(end_val));
            }
            let octets: [u8; 4] = start_v4.octets();
            let new_ip: Ipv4Addr = Ipv4Addr::new(octets[0], octets[1], octets[2], end_val as u8);
            Ok(IpAddr::V4(new_ip))
        }
        IpAddr::V6(start_v6) => {
            if end_val > 65535 {
                return Err(AddressError::InvalidV6Hextet(end_val));
            }
            let segments: [u16; 8] = start_v6.segments();
            let mut new_segments: [u16; 8] = segments;
            new_segments[7] = end_val as u16;
            let new_ip: Ipv6Addr = Ipv6Addr::from(new_segments);
            Ok(IpAddr::V6(new_ip))
        }
    }
}

/**
Generate all IPs between start and end (inclusive).

If `range` > [MAX_RANGE_SIZE], returns an error, unless `allow_large` is true.
*/
pub fn generate_ip_range(start: IpAddr, end: IpAddr, allow_large: bool) -> Result<Vec<IpAddr>, AddressError> {
    match (start, end) {
        (IpAddr::V4(start_v4), IpAddr::V4(end_v4)) => {
            let start_num: u32 = u32::from(start_v4);
            let end_num: u32 = u32::from(end_v4);

            if start_num > end_num {
                return Err(AddressError::RangeOrder(start, end));
            }

            let count: usize = (end_num - start_num + 1) as usize;
            if count > MAX_RANGE_SIZE && !allow_large {
                return Err(AddressError::RangeTooLarge(count as u128));
            }

            Ok((start_num..=end_num)
                .map(|n: u32| IpAddr::V4(Ipv4Addr::from(n)))
                .collect())
        }
        (IpAddr::V6(start_v6), IpAddr::V6(end_v6)) => {
            let start_num: u128 = u128::from(start_v6);
            let end_num: u128 = u128::from(end_v6);

            if start_num > end_num {
                return Err(AddressError::RangeOrder(start, end));
            }

            let count: u128 = end_num.saturating_sub(start_num).saturating_add(1);
            if count > MAX_RANGE_SIZE as u128 && !allow_large {
                return Err(AddressError::RangeTooLarge(count));
            }

            Ok((start_num..=end_num)
                .map(|n: u128| IpAddr::V6(Ipv6Addr::from(n)))
                .collect())
        }
        _ => Err(AddressError::Mismatch(start, end)),
    }
}

/* -------------------------------------------------------------------------- */

#[rustfmt::skip]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddressError {
    /// invalid IP/range/CIDR
    Invalid(String),
    /// range format is invalid
    InvalidRangeFmt(String),
    InvalidRangeBegIp  { beg: String, source: AddrParseError },
    InvalidRangeEndIp  { end: String, source: AddrParseError },
    InvalidRangeEndVal { val: String, source: ParseIntError },
    InvalidV4Octet(u32),
    InvalidV6Hextet(u32),
    RangeTooLarge(u128),
    RangeOrder(IpAddr, IpAddr),
    /// start and end are not the same IP family (v4 vs v6).
    Mismatch(IpAddr, IpAddr),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddressError::Invalid(ip) => {
                write!(f, "{ERR_INVALID_IP}: '{ip}'")
            }
            AddressError::InvalidRangeFmt(rng) => {
                write!(f, "{ERR_RNG_FMT}: '{rng}'")
            }
            AddressError::InvalidV4Octet(val) => {
                write!(f, "{ERR_V4_OCTET} {val}")
            }
            AddressError::InvalidV6Hextet(val) => {
                write!(f, "{ERR_V6_HEXTET} {val}")
            }
            AddressError::RangeTooLarge(size) => {
                write!(f, "{ERR_TOOLARGE}: {size} (max {MAX_RANGE_SIZE})")
            }
            AddressError::RangeOrder(beg, end) => {
                write!(f, "{ERR_RANGE_ORDER} ({beg} > {end})")
            }
            AddressError::Mismatch(a, b) => {
                write!(f, "{ERR_MISMATCH}: {a} - {b}")
            }
            AddressError::InvalidRangeBegIp { beg, source } => {
                write!(f, "{ERR_START}: '{beg}': {source}")
            }
            AddressError::InvalidRangeEndIp { end, source } => {
                write!(f, "{ERR_END}: '{end}': {source}")
            }
            AddressError::InvalidRangeEndVal { val, source } => {
                write!(f, "{ERR_RNG_END}: '{val}': {source}")
            }
        }
    }
}

impl error::Error for AddressError {}

/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_1: &str = "192.168.1.1";
    const TEST_2: &str = "192.168.1.2";
    const TEST_3: &str = "10.0.0.1";
    const TEST_4: &str = "10.0.0.5";
    const CIDR_1: &str = "192.168.1.0/30";
    const RANGE_1: &str = "10.0.0.1-5";
    const RANGE_2: &str = "10.0.0.1-10.0.0.5";
    const BAD_RANGE: &str = "10.0.0.5-10.0.0.1";
    const BIG_RANGE_V4: &str = "10.0.0.0/12";

    const TEST_V6_1: &str = "::1";
    const TEST_V6_2: &str = "::5";
    const TEST_V6_3: &str = "::ffff";
    const RANGE_V6: &str = "::1-5";
    const BAD_RANGE_V6: &str = "::5-1";
    const BIG_RANGE_V6: &str = "::1-::ffff";
    const TOOBIG_V6: &str = "::1-::ffff:ffff"; // 4B addresses

    #[test]
    fn test_parse_single_ip() {
        let result: Vec<IpAddr> = parse_ip_or_range(TEST_1).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], TEST_1.parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_parse_cidr() {
        let result: Vec<IpAddr> = parse_ip_or_range(CIDR_1).unwrap();
        assert_eq!(result.len(), 2); // .1 and .2 (hosts only)
        assert!(result.contains(&TEST_1.parse::<IpAddr>().unwrap()));
        assert!(result.contains(&TEST_2.parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_parse_short_range() {
        let result: Vec<IpAddr> = parse_ip_or_range(RANGE_1).unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], TEST_3.parse::<IpAddr>().unwrap());
        assert_eq!(result[4], TEST_4.parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_parse_full_range() {
        let result: Vec<IpAddr> = parse_ip_or_range(RANGE_2).unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], TEST_3.parse::<IpAddr>().unwrap());
        assert_eq!(result[4], TEST_4.parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_big_v4() {
        let result: Result<Vec<IpAddr>, AddressError> = parse_ip_or_range(BIG_RANGE_V4);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 2u64.pow(20) as usize - 2);
    }

    #[test]
    fn test_ipv6_short_range() {
        let result: Vec<IpAddr> = parse_ip_or_range(RANGE_V6).unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], TEST_V6_1.parse::<IpAddr>().unwrap());
        assert_eq!(result[4], TEST_V6_2.parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_ipv6_large_range() {
        let result: Vec<IpAddr> = parse_ip_or_range(BIG_RANGE_V6).unwrap();
        assert_eq!(result.len(), 65535);
        assert_eq!(result[0], TEST_V6_1.parse::<IpAddr>().unwrap());
        assert_eq!(result[65534], TEST_V6_3.parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_invalid_range() {
        let result: Result<Vec<IpAddr>, AddressError> = parse_ip_or_range(BAD_RANGE);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_range_v6() {
        let result: Result<Vec<IpAddr>, AddressError> = parse_ip_or_range(BAD_RANGE_V6);
        assert!(result.is_err());
    }

    #[test]
    fn test_toobig_v6() {
        let result: Result<Vec<IpAddr>, AddressError> = parse_ip_or_range(TOOBIG_V6);
        assert!(result.is_err());
    }
}

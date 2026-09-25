// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

/*!
Origin AS lookup via the Team Cymru IP-to-ASN DNS service.

Two TXT queries per target: the reversed address under `origin.asn.cymru.com`
(`origin6.asn.cymru.com` for IPv6) yields the announcing ASN(s), the covering
prefix, country code, RIR and allocation date; a second query per ASN for
`AS<n>.asn.cymru.com` yields the registered AS name. No new dependencies —
the same `hickory_resolver` instance used for target resolution does the
work, so `--dns-servers` / `--dns-timeout` apply here too.

Answer formats (fields separated by `|`, whitespace-padded):

```text
8.8.8.8.origin.asn.cymru.com.  TXT  "15169 | 8.8.8.0/24 | US | arin | 2023-12-28"
AS15169.asn.cymru.com.         TXT  "15169 | US | arin | 2000-03-30 | GOOGLE - Google LLC, US"
```

A multi-origin prefix lists several ASNs in the first field (space
separated), and an address covered by several announced prefixes yields
several TXT records. Unrouted space (RFC 1918, documentation ranges, ...)
answers NXDOMAIN.
*/

use crate::{strings::*, structs::QueryResponse, utils::reversed_addr};
use hickory_resolver::{
    ResolveError, Resolver, lookup::TxtLookup, name_server::TokioConnectionProvider,
};
use itertools::Itertools;
use std::{fmt, net::IpAddr};

const FIELD_SEP: char = '|';
const ORIGIN_MIN_FIELDS: usize = 4; // asn | prefix | cc | rir [| date]
const ASNAME_FIELDS: usize = 5; // asn | cc | rir | date | name

/// Origin AS information for an IP address, as reported by Team Cymru.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AsInfo {
    /// Announcing ASN(s); more than one for multi-origin prefixes.
    pub asns: Vec<u32>,
    /// The covering (announced) prefix, e.g. `8.8.8.0/24`.
    pub prefix: String,
    /// ISO 3166 country code of the prefix registration.
    pub country: String,
    /// Registry the prefix was allocated by (`arin`, `ripencc`, ...).
    pub rir: String,
    /// Allocation date (`YYYY-MM-DD`); may be empty.
    pub allocated: String,
    /// Registered AS name per ASN, in the same order as `asns` (may be empty).
    pub names: Vec<String>,
}

impl AsInfo {
    /// `AS15169 GOOGLE - Google LLC, US` - ASN(s) and name(s) only, for tight columns.
    pub fn short(&self) -> String {
        let asns = self.asns.iter().map(|n| format!("AS{n}")).join(", ");
        let names = self.names.iter().filter(|n| !n.is_empty()).join(" / ");
        if names.is_empty() {
            asns
        } else {
            format!("{asns} {names}")
        }
    }

    /// Fold another origin record into this one (multiple announcements
    /// covering the same address): union of ASNs, first record wins the rest.
    fn merge(mut self, other: AsInfo) -> AsInfo {
        for asn in other.asns {
            if !self.asns.contains(&asn) {
                self.asns.push(asn);
            }
        }
        self
    }
}

/// `AS15169 CLOUDFLARENET, US [1.1.1.0/24 AU/apnic]`
impl fmt::Display for AsInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let asns = self.asns.iter().map(|n| format!("AS{n}")).join(", ");
        let names = self.names.iter().filter(|n| !n.is_empty()).join(" / ");
        write!(f, "{asns}")?;
        if !names.is_empty() {
            write!(f, " {names}")?;
        }
        write!(f, " [{} {}/{}]", self.prefix, self.country, self.rir)
    }
}

/* -------------------------------------------------------------------------- */

/**
Look up origin AS information for `addr` from Team Cymru's DNS service.

Returns [QueryResponse::As] on success, [QueryResponse::TextStr] with
[WARN_AS_NONE] when the address is not announced (NXDOMAIN / no records), or
[QueryResponse::Error] on a resolver failure. AS name lookup failures are not
fatal: the origin data is still returned with an empty name.
*/
pub async fn lookup_as(res: &Resolver<TokioConnectionProvider>, addr: IpAddr) -> QueryResponse {
    let zone: &str = match addr {
        IpAddr::V4(_) => CYMRU_ORIGIN4,
        IpAddr::V6(_) => CYMRU_ORIGIN6,
    };
    let mut info: AsInfo = match res.txt_lookup(reversed_addr(&addr) + zone).await {
        Ok(resp) => match txt_strings(&resp)
            .filter_map(|t| parse_origin(&t))
            .reduce(AsInfo::merge)
        {
            Some(info) => info,
            None => return QueryResponse::ErrorStr(ERR_AS_PARSE),
        },
        Err(e) => return no_records_or_err(e),
    };
    for asn in info.asns.clone() {
        let name: String = match res.txt_lookup(format!("AS{asn}{CYMRU_ASNAME}")).await {
            Ok(resp) => txt_strings(&resp)
                .find_map(|t| parse_asname(&t))
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        info.names.push(name);
    }
    QueryResponse::As(info)
}

/// Each TXT record as one string (character-strings concatenated).
fn txt_strings(resp: &TxtLookup) -> impl Iterator<Item = String> + '_ {
    resp.iter().map(|txt| {
        txt.txt_data()
            .iter()
            .map(|b| String::from_utf8_lossy(b))
            .collect::<String>()
    })
}

/// Map "no such name / no records" to a plain warning, anything else to an error.
fn no_records_or_err(e: ResolveError) -> QueryResponse {
    if e.is_nx_domain() || e.is_no_records_found() {
        QueryResponse::TextStr(WARN_AS_NONE)
    } else {
        QueryResponse::Error(e.to_string())
    }
}

/// Split a Cymru answer into its trimmed `|`-separated fields.
fn split_fields(txt: &str) -> Vec<&str> {
    txt.split(FIELD_SEP).map(str::trim).collect()
}

/// Parse one origin record: `15169 | 8.8.8.0/24 | US | arin | 2023-12-28`.
fn parse_origin(txt: &str) -> Option<AsInfo> {
    let f = split_fields(txt);
    if f.len() < ORIGIN_MIN_FIELDS {
        return None;
    }
    let asns: Vec<u32> = f[0]
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if asns.is_empty() {
        return None;
    }
    Some(AsInfo {
        asns,
        prefix: f[1].to_string(),
        country: f[2].to_string(),
        rir: f[3].to_string(),
        allocated: f.get(4).unwrap_or(&"").to_string(),
        names: Vec::new(),
    })
}

/// Parse an AS name record: `15169 | US | arin | 2000-03-30 | GOOGLE - Google LLC, US`.
fn parse_asname(txt: &str) -> Option<String> {
    let f = split_fields(txt);
    let name: &str = f.get(ASNAME_FIELDS - 1)?;
    (!name.is_empty()).then(|| name.to_string())
}

/* ================================= tests ================================== */

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn origin_record_parses() {
        let info = parse_origin("15169 | 8.8.8.0/24 | US | arin | 2023-12-28").unwrap();
        assert_eq!(info.asns, vec![15169]);
        assert_eq!(info.prefix, "8.8.8.0/24");
        assert_eq!(info.country, "US");
        assert_eq!(info.rir, "arin");
        assert_eq!(info.allocated, "2023-12-28");
        assert!(info.names.is_empty());
    }

    #[test]
    fn origin_record_moas_and_missing_date() {
        let info = parse_origin("15169 3356 | 8.8.8.0/24 | US | arin").unwrap();
        assert_eq!(info.asns, vec![15169, 3356]);
        assert_eq!(info.allocated, "");
    }

    #[test]
    fn origin_record_garbage() {
        assert!(parse_origin("").is_none());
        assert!(parse_origin("NA | 8.8.8.0/24 | US | arin").is_none());
        assert!(parse_origin("15169 | 8.8.8.0/24").is_none());
    }

    #[test]
    fn asname_record_parses() {
        let txt = "15169 | US | arin | 2000-03-30 | GOOGLE - Google LLC, US";
        assert_eq!(
            parse_asname(txt).as_deref(),
            Some("GOOGLE - Google LLC, US")
        );
        assert!(parse_asname("15169 | US | arin | 2000-03-30 |").is_none());
        assert!(parse_asname("15169 | US").is_none());
    }

    #[test]
    fn merge_unions_asns() {
        let a = parse_origin("15169 | 8.8.8.0/24 | US | arin | 2023-12-28").unwrap();
        let b = parse_origin("3356 15169 | 8.8.0.0/16 | US | arin | 1992-12-01").unwrap();
        let m = a.merge(b);
        assert_eq!(m.asns, vec![15169, 3356]);
        assert_eq!(m.prefix, "8.8.8.0/24");
    }

    #[test]
    fn display_format() {
        let mut info = parse_origin("13335 | 1.1.1.0/24 | AU | apnic | 2011-08-11").unwrap();
        assert_eq!(info.to_string(), "AS13335 [1.1.1.0/24 AU/apnic]");
        info.names.push("CLOUDFLARENET, US".to_string());
        assert_eq!(
            info.to_string(),
            "AS13335 CLOUDFLARENET, US [1.1.1.0/24 AU/apnic]"
        );
    }

    #[test]
    fn query_names() {
        let v4 = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        assert_eq!(
            reversed_addr(&v4) + CYMRU_ORIGIN4,
            "8.8.8.8.origin.asn.cymru.com."
        );
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888));
        assert_eq!(
            reversed_addr(&v6) + CYMRU_ORIGIN6,
            "8.8.8.8.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.6.8.4.0.6.8.4.1.0.0.2.origin6.asn.cymru.com."
        );
    }

    /// Live lookup against Team Cymru — needs network; run with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_lookup() {
        let res = Resolver::builder_tokio().unwrap().build();
        match lookup_as(&res, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).await {
            QueryResponse::As(info) => {
                assert!(info.asns.contains(&15169), "{info:?}");
                assert_eq!(info.names.len(), info.asns.len());
                assert!(info.names[0].contains("GOOGLE"), "{info:?}");
                eprintln!("8.8.8.8 -> {info}");
            }
            other => panic!("unexpected response: {other}"),
        }
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888));
        match lookup_as(&res, v6).await {
            QueryResponse::As(info) => {
                assert!(info.asns.contains(&15169), "{info:?}");
                eprintln!("{v6} -> {info}");
            }
            other => panic!("unexpected response: {other}"),
        }
        let private = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let resp = lookup_as(&res, private).await;
        assert!(
            matches!(resp, QueryResponse::TextStr(s) if s == WARN_AS_NONE),
            "{resp}"
        );
        eprintln!("{private} -> {resp}");
    }
}

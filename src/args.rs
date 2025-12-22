// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    strings::*,
    structs::{DEFAULT_PAYLOAD_SIZE, MessageBuffer},
    utils::{parse_float_into_duration, parse_ip_addresses},
};
use clap::{Parser, crate_authors, crate_description, crate_name, crate_version, value_parser};
use hickory_resolver::{
    ResolveError, Resolver, TokioResolver,
    config::{NameServerConfigGroup, ResolverConfig, ResolverOpts},
    name_server::TokioConnectionProvider,
    system_conf::read_system_conf,
};
use std::{
    collections::HashSet,
    fmt::Debug,
    net::IpAddr,
    sync::{Arc, LazyLock},
    time::Duration,
};

const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_secs(5);
// Default payload size as a string constant for clap default_value.
// Wow, this is mega convoluted...
static DEFAULT_PAYLOAD_STR: LazyLock<&'static str> =
    LazyLock::new(|| Box::leak(DEFAULT_PAYLOAD_SIZE.to_string().into_boxed_str()));

/// Configuration struct for the program.
#[derive(Parser, Default, Debug, Clone)]
#[command(name = crate_name!(), version = crate_version!(), author = crate_authors!(), about = crate_description!())]
pub(crate) struct MpConfig {
    #[arg(required = true, value_name = "IP1 [IP2...]", help = HELP_TARGETS)]
    pub targets: Vec<String>,

    #[arg(
        long,
        value_name = "IP1[,IP2...]",
        value_delimiter = ',',
        require_equals = true,
        help = HELP_EXCLUDE
    )]
    pub exclude: Vec<String>,

    #[arg(
        long,
        short = 'I',
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "1",
        help = HELP_INTERVAL
    )]
    pub interval: Duration,

    #[arg(
        long,
        short = 'T',
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "2",
        help = HELP_TIMEOUT
    )]
    pub timeout: Duration,

    #[arg(
        long,
        short = 's',
        value_name = "BYTES",
        required = false,
        value_parser = value_parser!(u16).range(32..32760),
        default_value = *DEFAULT_PAYLOAD_STR,
        help = HELP_SIZE
    )]
    pub size: u16,

    #[arg(long, short = 'R', help = HELP_RANDOMIZE)]
    pub randomize: bool,

    #[arg(
        long,
        short = 'H',
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u32).range(60..65536),
        default_value = "3600",
        help = HELP_HISTSIZE
    )]
    pub histsize: u32,

    #[arg(
        long,
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u16).range(10..1000),
        default_value = "100",
        help = HELP_DETAILED
    )]
    pub detailed: u16,

    #[arg(long, help = HELP_PAUSED)]
    pub paused: bool,

    #[arg(
        long,
        value_name = "ms",
        required = false,
        value_parser = value_parser!(u64).range(50..5000),
        default_value = "250",
        help = HELP_REFRESH
    )]
    pub refresh: u64,

    #[arg(
        long,
        value_name = "IP1[,IP2...]",
        value_delimiter = ',',
        require_equals = true,
        value_parser = value_parser!(IpAddr),
        help = HELP_DNS_SERVERS
    )]
    pub dns_servers: Vec<IpAddr>,

    #[arg(
        long,
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "5",
        help = HELP_DNS_TIMEOUT
    )]
    pub dns_timeout: Duration,

    #[arg(
        long,
        value_name = "FLOAT",
        required = false,
        value_parser = value_parser!(f64),
        default_value = "1.0",
        help = HELP_STRETCH_FACTOR
    )]
    pub stretch_factor: f64,

    #[arg(long, help = HELP_PERF)]
    pub perf: bool,

    #[arg(long, short = 'v', help = HELP_VERBOSE)]
    pub verbose: bool,

    #[arg(long, help = HELP_DEBUG)]
    pub debug: bool,

    #[arg(skip)]
    pub addrs: Vec<IpAddr>,

    #[arg(skip)]
    pub seen: HashSet<IpAddr>,

    #[arg(skip)]
    pub ver: String,

    #[arg(skip)]
    pub resolver: Option<Arc<TokioResolver>>,

    #[arg(skip)]
    pub buf: Arc<MessageBuffer>,
}

impl MpConfig {
    /// Parses command line arguments and returns a [MpConfig] struct.
    pub async fn parse() -> Result<MpConfig, ResolveError> {
        let mut config: MpConfig = <MpConfig as Parser>::parse();
        config.ver = crate_version!().to_string();
        config
            .buf
            .push(format!("mping v{} - starting initialization", config.ver));

        // clamp DNS timeout between 1 and 10 seconds
        config.dns_timeout = match config.dns_timeout {
            d if d < Duration::from_secs(1) => Duration::from_secs(1),
            d if d > Duration::from_secs(10) => Duration::from_secs(10),
            d => d,
        };

        // We unfortunately need to build the resolver with all the boilerplate from scratch.
        // hickory-resolver has quite a convoluted initialization process for custom configs...
        let wants_default_conf: bool = config.dns_servers.is_empty();
        let wants_default_opts: bool = config.dns_timeout == DEFAULT_DNS_TIMEOUT;
        let (resolver_config, resolver_opts) = {
            if wants_default_conf && wants_default_opts {
                config.buf.push("Using system DNS configuration");
                read_system_conf()? // use system defaults
            } else {
                let mut res_opts = ResolverOpts::default();
                if !wants_default_opts {
                    config.buf.push(&format!(
                        "Setting custom DNS timeout: {:.2}s",
                        config.dns_timeout.as_secs_f64()
                    ));
                    res_opts.timeout = config.dns_timeout;
                }
                let res_conf = match wants_default_conf {
                    true => {
                        let (sys_conf, _) = read_system_conf()?;
                        sys_conf
                    }
                    false => {
                        config.buf.push(&format!(
                            "Using custom DNS server(s): {}",
                            config
                                .dns_servers
                                .iter()
                                .map(|ip| ip.to_string())
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                        ResolverConfig::from_parts(
                            None,
                            vec![],
                            NameServerConfigGroup::from_ips_clear(&config.dns_servers, 53, true),
                        )
                    }
                };
                (res_conf, res_opts)
            }
        };

        // Initialize the resolver already here so we can use it during target parsing
        let provider = TokioConnectionProvider::default();
        config.resolver = Some(
            Resolver::builder_with_config(resolver_config, provider)
                .with_options(resolver_opts)
                .build()
                .into(),
        );

        // Parse the addresses which are straight up IPs/ranges first.
        let (mut addrs, mut seen, failed) =
            parse_ip_addresses(&config.targets, Some(&config.exclude), config.verbose);

        // Now try to resolve any entries that failed to parse as IPs.
        let mut resolved: Vec<IpAddr> = Vec::with_capacity(failed.len()); // assume all would resolve 1:1
        if let Some(resolver) = &config.resolver {
            for name in &failed {
                match resolver.lookup_ip(name).await {
                    Ok(lookup) => {
                        let mut ips: Vec<IpAddr> = lookup.iter().collect();
                        if !ips.is_empty() {
                            let msg = config
                                .buf
                                .push(format!("{INFO_RESOLVE_ONE} '{name}': {}", ips.len()));
                            if config.verbose {
                                eprintln!("{msg}");
                            }
                        }
                        resolved.append(&mut ips);
                    }
                    Err(e) => {
                        let msg = config.buf.push(format!("{ERR_RESOLVE} '{name}': {e}"));
                        if config.verbose {
                            eprintln!("{msg}");
                        }
                    }
                }
            }
        }

        // Did we resolve any new addresses?
        if !resolved.is_empty() {
            // Remove duplicates by comparing against already parsed addresses.
            resolved.retain(|ip: &IpAddr| seen.insert(*ip));
            let msg = config
                .buf
                .push(format!("{INFO_RESOLVED}: {}", resolved.len()));
            if config.verbose {
                eprintln!("{msg}");
            }
            addrs.append(&mut resolved);
        }

        // Yes, I, the mighty programmer, am aware that I could sometimes be INCORRECT!
        #[cfg(debug_assertions)]
        {
            assert_eq!(
                HashSet::from_iter(addrs.iter().cloned()),
                seen,
                "\"seen\" set does not match parsed addresses! Fix da code, ya dumbass!"
            );
        }

        // Present the Vec and HashSet of addresses to the main code for consumption.
        config.addrs = addrs;
        config.seen = seen;
        if config.addrs.is_empty() {
            let msg = config.buf.push(format!("{WARN_NO_VALID_IPS}"));
            eprintln!("{msg}");
        } else {
            let msg = config
                .buf
                .push(format!("{INFO_UNIQUE}: {}", config.addrs.len()));
            if config.verbose {
                eprintln!("{msg}");
            }
        }

        // clamp interval between 10ms and 10s...
        config.interval = match config.interval {
            d if d < Duration::from_millis(10) => Duration::from_millis(10),
            d if d > Duration::from_secs(10) => Duration::from_secs(10),
            d => d,
        };
        // ... and timeout between 10ms and 5s
        config.timeout = match config.timeout {
            d if d < Duration::from_millis(10) => Duration::from_millis(10),
            d if d > Duration::from_secs(5) => Duration::from_secs(5),
            d => d,
        };

        /*
        If necessary, tweak the timeout so that we can't have an excessive number of
        pending pings (tasks) to the same target. This is a simple heuristic to avoid
        overwhelming the application with too many concurrent pings if the user has
        set an unreasonably high timeout combined with a very low interval.
        */
        let limit: Duration = config.interval * 4; // max. 4 pending pings per target
        if config.timeout > limit {
            let msg = config.buf.push(format!(
                "{INFO_ADJUST} ({:.2}s -> {:.2}s, interval: {:.2}s)",
                config.timeout.as_secs_f64(),
                limit.as_secs_f64(),
                config.interval.as_secs_f64(),
            ));
            if config.verbose {
                eprintln!("{msg}");
            }
            config.timeout = limit;
        }

        Ok(config)
    }
}

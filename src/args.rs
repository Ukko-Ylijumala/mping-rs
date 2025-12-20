// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    strings::*,
    structs::DEFAULT_PAYLOAD_SIZE,
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

    #[arg(long, help = HELP_PERF)]
    pub perf: bool,

    #[arg(long, short = 'v', help = HELP_VERBOSE)]
    pub verbose: bool,

    #[arg(long, help = HELP_DEBUG)]
    pub debug: bool,

    #[arg(skip)]
    pub addrs: Vec<IpAddr>,

    #[arg(skip)]
    pub ver: String,

    #[arg(skip)]
    pub resolver: Option<Arc<TokioResolver>>,
}

impl MpConfig {
    /// Parses command line arguments and returns a [MpConfig] struct.
    pub fn parse() -> Result<MpConfig, ResolveError> {
        let mut config: MpConfig = <MpConfig as Parser>::parse();
        config.ver = crate_version!().to_string();

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
                read_system_conf()? // use system defaults
            } else {
                let mut res_opts = ResolverOpts::default();
                if !wants_default_opts {
                    res_opts.timeout = config.dns_timeout;
                }
                let res_conf = match wants_default_conf {
                    true => {
                        let (sys_conf, _) = read_system_conf()?;
                        sys_conf
                    }
                    false => ResolverConfig::from_parts(
                        None,
                        vec![],
                        NameServerConfigGroup::from_ips_clear(&config.dns_servers, 53, true),
                    ),
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

        config.addrs = parse_ip_addresses(&config.targets, Some(&config.exclude), config.verbose);
        if config.addrs.is_empty() {
            eprintln!("{WARN_NO_VALID_IPS}");
        } else if config.verbose {
            eprintln!("{INFO_UNIQUE}: {}", config.addrs.len());
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
            if config.verbose {
                eprintln!(
                    "{INFO_ADJUST} ({:.2}s -> {:.2}s, interval: {:.2}s)",
                    config.timeout.as_secs_f64(),
                    limit.as_secs_f64(),
                    config.interval.as_secs_f64(),
                );
            }
            config.timeout = limit;
        }

        Ok(config)
    }
}

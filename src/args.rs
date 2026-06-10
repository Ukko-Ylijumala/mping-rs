// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    logging::{LogLevel, MessageBuffer},
    strings::*,
    structs::{DEFAULT_DETAILED, DEFAULT_HISTSIZE, DEFAULT_PAYLOAD_SIZE, Resolved},
    utils::{collect_targets, parse_float_into_duration},
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
// Some default sizes as string constants for clap default_value.
// Wow, this is mega convoluted...
static DEFAULT_PAYLOAD_STR: LazyLock<&'static str> =
    LazyLock::new(|| Box::leak(DEFAULT_PAYLOAD_SIZE.to_string().into_boxed_str()));
static DEFAULT_HISTSIZE_STR: LazyLock<&'static str> =
    LazyLock::new(|| Box::leak(DEFAULT_HISTSIZE.to_string().into_boxed_str()));
static DEFAULT_DETAILED_STR: LazyLock<&'static str> =
    LazyLock::new(|| Box::leak(DEFAULT_DETAILED.to_string().into_boxed_str()));

/// Configuration struct for the program.
#[derive(Parser, Default, Debug, Clone)]
#[command(name = crate_name!(), version = crate_version!(), author = crate_authors!(), about = crate_description!())]
pub(crate) struct MpConfig {
    #[arg(value_name = IP_LIST, help = HELP_TARGETS)]
    pub targets: Vec<String>,

    #[arg(
        long,
        value_name = IP_LIST_COMMA,
        value_delimiter = ',',
        require_equals = true,
        help = HELP_EXCLUDE
    )]
    pub exclude: Vec<String>,

    #[arg(
        long,
        short = 'I',
        value_name = SECS,
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "1",
        help = HELP_INTERVAL
    )]
    pub interval: Duration,

    #[arg(
        long,
        short = 'T',
        value_name = SECS,
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "2",
        help = HELP_TIMEOUT
    )]
    pub timeout: Duration,

    #[arg(
        long,
        short = 's',
        value_name = BYTES,
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
        value_name = NUM,
        required = false,
        value_parser = value_parser!(u32).range(60..65536),
        default_value = *DEFAULT_HISTSIZE_STR,
        help = HELP_HISTSIZE
    )]
    pub histsize: u32,

    #[arg(
        long,
        value_name = NUM,
        required = false,
        value_parser = value_parser!(u16).range(10..1000),
        default_value = *DEFAULT_DETAILED_STR,
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
        value_name = IP_LIST_COMMA,
        value_delimiter = ',',
        require_equals = true,
        value_parser = value_parser!(IpAddr),
        help = HELP_DNS_SERVERS
    )]
    pub dns_servers: Vec<IpAddr>,

    #[arg(
        long,
        value_name = SECS,
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "5",
        help = HELP_DNS_TIMEOUT
    )]
    pub dns_timeout: Duration,

    #[arg(
        long,
        value_name = FLOAT,
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

    #[arg(skip)]
    pub resolved: Resolved,
}

impl MpConfig {
    /// Parses command line arguments and returns a [MpConfig] struct.
    pub async fn parse() -> Result<MpConfig, ResolveError> {
        let mut config: MpConfig = <MpConfig as Parser>::parse();
        config.ver = crate_version!().to_string();
        config
            .buf
            .push(format!("mping v{} - starting initialization", config.ver));

        if config.verbose || config.debug {
            // Adjust stderr logging priority. Debug takes precedence.
            let lvl = if config.debug {
                LogLevel::Debug
            } else {
                LogLevel::Info
            };
            // Have to perform a bit of a dance here due to Arc immutability...
            Arc::get_mut(&mut config.buf).unwrap().to_stderr(lvl);
        }

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
                config.buf.debug(INFO_DNS);
                read_system_conf()? // use system defaults
            } else {
                let mut res_opts = ResolverOpts::default();
                if !wants_default_opts {
                    config.buf.debug(&format!(
                        "{INFO_DNS_TIMEO}: {:.1}s",
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
                        config.buf.debug(&format!(
                            "{INFO_DNS_CUSTOM}: {}",
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

        // Parse straight IP/range/CIDR entries, then try to resolve the leftovers as DNS names.
        let collected = collect_targets(
            &config.targets,
            Some(&config.exclude),
            config.resolver.as_ref().unwrap(),
            &*config.buf,
        )
        .await;

        // Fold resolved name->IP pairs into the master Resolved map for later display.
        for (name, ips) in &collected.resolved {
            config.resolved.add(name, ips);
        }
        if !config.resolved.is_empty() {
            config
                .buf
                .notice(format!("{INFO_RESOLVED}: {}", config.resolved.len()));
        }

        // Yes, I, the mighty programmer, am aware that I could sometimes be INCORRECT!
        // NOTE: `seen` mirrors `addrs` exactly: exclusions are already applied to it,
        // and DNS-resolved IPs (which exclusions deliberately don't touch) are in both.
        #[cfg(debug_assertions)]
        {
            assert_eq!(
                HashSet::from_iter(collected.addrs.iter().cloned()),
                collected.seen,
                "\"seen\" set does not match parsed addresses! Fix da code, ya dumbass!"
            );
        }

        // Present the Vec and HashSet of addresses to the main code for consumption.
        config.addrs = collected.addrs;
        config.seen = collected.seen;
        if config.addrs.is_empty() {
            config.buf.notice(format!("{WARN_NO_VALID_IPS}"));
        } else {
            config
                .buf
                .push(format!("{INFO_UNIQUE}: {}", config.addrs.len()));
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
            config.buf.notice(format!(
                "{INFO_ADJUST} ({:.2}s -> {:.2}s, interval: {:.2}s)",
                config.timeout.as_secs_f64(),
                limit.as_secs_f64(),
                config.interval.as_secs_f64(),
            ));
            config.timeout = limit;
        }

        Ok(config)
    }
}

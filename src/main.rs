// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod ip_addresses;
mod latencywin;
mod simplecolor;
mod structs;
mod tabulator;
mod utils;
use crate::{
    args::MpConfig,
    latencywin::LatencyWindow,
    simplecolor::*,
    structs::{PingStatus, PingTarget, PingTargetInner, StatsSnapshot},
    tabulator::simple_tabulate,
    utils::{
        curses_setup, curses_teardown, nice_permission_error, panic_handler, setup_signal_handler,
    },
};

use futures::future::join_all;
use ncurses::*;
use rand::{fill, random};
use std::{
    net::IpAddr,
    panic,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use surge_ping::{
    Client, Config, ICMP, IcmpPacket, PingIdentifier, PingSequence, Pinger, SurgeError,
};
use tokio::{sync::Mutex, time, time::Interval};

/// Create PingTarget instances for each IP address.
fn make_targets(addrs: &[IpAddr], histsize: usize) -> Vec<Arc<PingTarget>> {
    addrs
        .into_iter()
        .map(|addr| {
            Arc::new(PingTarget {
                addr: *addr,
                data: Mutex::new(PingTargetInner {
                    sent: 0,
                    recv: 0,
                    rtts: LatencyWindow::new(histsize),
                    status: PingStatus::None,
                }),
            })
        })
        .collect()
}

/// Update ping statistics based on the result. Separated into fn for target lock granularity.
async fn update_ping_stats(tgt: &Arc<PingTarget>, res: Result<(IcmpPacket, Duration), SurgeError>) {
    let mut stats = tgt.data.lock().await;
    match res {
        Ok((_, dur)) => {
            stats.recv += 1;
            stats.rtts.push(dur.as_micros() as u32);
            stats.status = PingStatus::Ok;
        }
        Err(e) => {
            stats.status = match e {
                SurgeError::Timeout { .. } => {
                    if stats.sent > 10 && stats.recv == 0 {
                        PingStatus::NotReachable
                    } else {
                        PingStatus::Timeout
                    }
                }
                _ => PingStatus::Error(e),
            };
        }
    };
}

/// Set up a ping loop for each target.
async fn ping_loop(
    tgt: Arc<PingTarget>,
    client: Arc<Client>,
    quit: Arc<AtomicBool>,
    conf: Arc<MpConfig>,
    payload: Arc<[u8]>,
) {
    let id: PingIdentifier = PingIdentifier(random());
    let mut ticker: Interval = time::interval(Duration::from_millis(100));
    let loop_interval: Duration = conf.interval;
    let mut next_ping: time::Instant = tokio::time::Instant::now();
    let mut payload: Arc<[u8]> = match conf.randomize {
        // create a new payload for the ping loop which we can randomize
        true => payload.as_ref().to_vec().into(),
        false => payload.clone(),
    };

    loop {
        ticker.tick().await;
        if quit.load(Ordering::Relaxed) {
            break;
        }

        if tokio::time::Instant::now() >= next_ping {
            let seq: u16 = {
                let mut stats = tgt.data.lock().await;
                // update sent count here to make sure it's incremented before
                // sending so that the main sent count stays accurate even if
                // ping fails or we get out of order replies etc
                let sent: u64 = stats.sent;
                stats.sent += 1;
                // calculate the 16-bit sequence number from sent count,
                // since 2^16 is the max for ICMP sequence numbers
                (sent % 65536) as u16
            };

            // The async ping task can be spawned either using a closure, or an
            // async fn block. Both should be functionally equivalent.
            // In either case the pinger is created anew for each async context.
            //
            // Function style (saved for reference):
            // tokio::spawn(ping(client.clone(), tgt.clone(), conf.timeout, id, seq));
            //
            let mut pinger: Pinger = client.pinger(tgt.addr, id).await;
            pinger.timeout(conf.timeout);
            let tgt_clone: Arc<PingTarget> = tgt.clone();
            let pl: Arc<[u8]> = match conf.randomize {
                true => {
                    let payload: &mut [u8] = Arc::make_mut(&mut payload);
                    // Can't use a thread-local RNG here (for performance)
                    // because it's not Send'able across await points.
                    // However, we can spare CPU time by randomizing only
                    // the first 32 bytes of the payload, which should be plenty.
                    // And we already know the payload must be 32 bytes minimum.
                    fill(&mut payload[..32]);
                    payload.into()
                }
                false => payload.clone(),
            };
            tokio::spawn(async move {
                let res = pinger.ping(PingSequence(seq), &pl).await;
                update_ping_stats(&tgt_clone, res).await;
            });
            next_ping += loop_interval;
        }
    }
}

/// Extract statistics data from a target's inner data.
async fn extract_stats(tgt: &Arc<PingTarget>) -> (StatsSnapshot, String) {
    // Holding the lock inside this function only should minimize contention.
    // Do all the expensive string formatting in the caller.
    let stats = tgt.data.lock().await;
    let snap: StatsSnapshot = StatsSnapshot::new_from(&stats);
    // status formatting is cheap relative to float formatting
    (snap, format!("{}", stats.status))
}

/// Gather current data from all targets.
async fn gather_target_data(targets: &[Arc<PingTarget>]) -> Vec<[String; 10]> {
    let mut data: Vec<[String; 10]> = Vec::new();

    for tgt in targets {
        let (snap, stat) = extract_stats(tgt).await;
        // Do all the (expensive) string formatting after releasing the lock.
        data.push([
            tgt.addr.to_string(),
            snap.sent.to_string(),
            snap.recv.to_string(),
            snap.loss_str(),
            snap.last_str(),
            snap.mean_str(),
            snap.min_str(),
            snap.max_str(),
            snap.stdev_str(),
            stat,
        ]);
    }

    data
}

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: Arc<MpConfig> = MpConfig::parse().into();

    // Pinger clients
    // sharing a client across multiple targets is safe and allows socket reuse
    let client_v4: Option<Arc<Client>> = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv4()) {
        match Client::new(&Config::default()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => return Err(nice_permission_error(&e, "v4")),
        }
    } else {
        None
    };
    let client_v6: Option<Arc<Client>> = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv6()) {
        let cfg: Config = Config::builder().kind(ICMP::V6).build();
        match Client::new(&cfg) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => return Err(nice_permission_error(&e, "v6")),
        }
    } else {
        None
    };

    // Spawn ping tasks
    let payload: Arc<[u8]> = vec![0u8; conf.size as usize].into();
    let targets: Vec<Arc<PingTarget>> = make_targets(&conf.addrs, conf.histsize as usize);
    let quit: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for tgt in &targets {
        let client = match tgt.addr {
            IpAddr::V4(_) => client_v4.as_ref().expect("IPv4 client missing"),
            IpAddr::V6(_) => client_v6.as_ref().expect("IPv6 client missing"),
        };
        tasks.push(tokio::spawn(ping_loop(
            tgt.clone(),
            client.clone(),
            quit.clone(),
            conf.clone(),
            payload.clone(),
        )));
    }

    // Curses initialization
    setup_signal_handler(quit.clone());
    panic::set_hook(Box::new(panic_handler));
    curses_setup(conf.debug);

    // Main display loop
    let mut ui_tick: Interval = time::interval(Duration::from_millis(500));
    let headers: Vec<&str> = vec![
        "Address", "Sent", "Recv", "Loss", "Last", "Mean", "Min", "Max", "Stdev", "Status",
    ];

    // FIXME:
    // This works for applying ANSI codes to headers for final printout, but Curses
    // rendering needs to be done differently as Curses simply does not care about ANSI codes.
    let headers_ansi: Vec<String> = apply_ansi_to_all(
        headers.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        bold,
    );
    let headers_ansi: Vec<&str> = headers_ansi.iter().map(|s: &String| s.as_str()).collect();

    while !quit.load(Ordering::Relaxed) {
        ui_tick.tick().await;

        // Render the table with dynamic tabulation, correct column widths etc
        let data: Vec<[String; 10]> = gather_target_data(&targets).await;
        for (i, line) in simple_tabulate(data, Some(&headers)).iter().enumerate() {
            // mvprintw could segfault if the string contains "%" characters, use mvaddstr instead
            mvaddstr(i as i32, 0, line);
        }

        refresh();
        if getch() == 'q' as i32 {
            quit.store(true, Ordering::Relaxed);
        }
    }

    // Cleanup
    curses_teardown(conf.debug);
    if conf.verbose || conf.debug {
        eprintln!("Main thread quitting. Waiting for tasks to terminate...");
    }
    join_all(tasks).await;

    // Print final stats
    let data: Vec<[String; 10]> = gather_target_data(&targets).await;
    for line in simple_tabulate(data, Some(&headers_ansi)) {
        println!("{line}");
    }
    Ok(())
}

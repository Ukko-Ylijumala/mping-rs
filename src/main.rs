// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod ip_addresses;
mod latencywin;
mod structs;
mod tabulator;
mod tui;
mod utils;

use crate::{
    args::MpConfig,
    structs::{AppState, PacketRecord, PingStatus, PingTarget, StatsSnapshot},
    tabulator::simple_tabulate,
    tui::{TableRow, TerminalGuard, key_event_handler},
    utils::setup_signal_handler,
};

use futures::future::join_all;
use miniutils::ToDisplay;
use rand::{fill, random};
use ratatui::{prelude::*, widgets::*};
use std::{net::IpAddr, sync::Arc, thread, time::Duration};
use surge_ping::{Client, IcmpPacket, PingIdentifier, PingSequence, Pinger, SurgeError};
use tokio::time::{self, Instant, Interval};

const PAYLOAD_RND_BYTES: usize = 32;

////////////////////////////////////////////////////////////////////////////////

/// Create [PingTarget] instances for each IP address.
fn make_targets(addrs: &[IpAddr], histsize: u32, detailed: u16) -> Vec<PingTarget> {
    addrs
        .iter()
        .map(|addr| PingTarget::new(*addr, histsize as usize, detailed as usize))
        .collect()
}

/// Update ping statistics based on the result. Separated into fn for target lock granularity.
async fn update_ping_stats(
    tgt: Arc<PingTarget>,
    res: Result<(IcmpPacket, Duration), SurgeError>,
    mut rec: PacketRecord,
) {
    let mut stats = tgt.data.write();
    match res {
        Ok((_, dur)) => {
            stats.recv += 1;
            stats.rtts.push(dur.as_micros() as u32);
            stats.status = PingStatus::Ok;
            rec.set_rtt(dur);
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
                _ => PingStatus::Error(e.to_display()),
            };
        }
    };
    stats.recent.push(rec);

    // Update "paused" status here if necessary, as it's the overriding status.
    // In theory the paused state could have been changed by the task spawned by ping_loop()
    // calling this function in the previous iteration before the flag toggle took effect.
    if tgt.is_paused() && !matches!(stats.status, PingStatus::Paused) {
        stats.status = PingStatus::Paused;
    }

    // Update status based on recent history if applicable
    if matches!(stats.status, PingStatus::Ok | PingStatus::Timeout) {
        if stats.is_flappy(10, 5) {
            stats.status = PingStatus::Flappy
        } else if stats.is_lossy(5, 0.5) {
            stats.status = PingStatus::Lossy
        } else if stats.is_laggy(10, 2.0).unwrap_or(false) {
            stats.status = PingStatus::Laggy
        }
    }
}

/// Set up a ping loop for each target.
async fn ping_loop(tgt: Arc<PingTarget>, client: Arc<Client>, app: Arc<AppState>) {
    let id: PingIdentifier = PingIdentifier(random());
    let mut ticker: Interval = time::interval(app.ping_interval);
    let mut next_ping: Instant = tokio::time::Instant::now();

    while !app.is_quitting() {
        ticker.tick().await;
        if tgt.is_paused() {
            // Adjust next ping time to not build a backlog while paused.
            // When unpaused, the next ping should be pretty much immediate
            // and subsequent pings will resume at the normal interval.
            next_ping = tokio::time::Instant::now();
            continue;
        }
        if tokio::time::Instant::now() < next_ping {
            continue;
        }

        // We must create a new Pinger for each async context, since otherwise we'll have
        // to wait for the previous ping to complete before sending the next one.
        let mut pinger: Pinger = client.pinger(tgt.addr, id).await;
        pinger.timeout(app.ping_timeout);
        let tgt_clone: Arc<PingTarget> = tgt.clone();

        // Clone the payload slice into a new one, which we can randomize if needed.
        // Arc::make_mut() will perform a clone-on-write if necessary.
        let pl: Arc<[u8]> = match app.randomize {
            true => {
                let mut payload: Arc<[u8]> = app.payload.clone();
                let payload: &mut [u8] = Arc::make_mut(&mut payload);
                // Can't use a thread-local RNG here (for performance)
                // because it's not Send'able across await points.
                // However, we can spare CPU time by randomizing only
                // the first 32 bytes of the payload, which should be plenty.
                // And we already know the payload must be 32 bytes minimum.
                fill(&mut payload[..PAYLOAD_RND_BYTES]);
                payload.into()
            }
            false => app.payload.clone(),
        };

        // Update sent timestamp as late as possible before sending
        // so that the time difference is minimized. There will still
        // be some delay due to task scheduling etc, but this should
        // be negligible compared to network latencies.
        let seq: u16 = {
            let mut stats = tgt.data.write();
            // update sent count here to make sure it's incremented before
            // sending so that the main sent count stays accurate even if
            // ping fails or we get out of order replies etc
            let sent: u64 = stats.sent;
            stats.sent += 1;
            // calculate the 16-bit sequence number from sent count,
            // since 2^16 is the max for ICMP sequence numbers
            let seq: u16 = (sent % 65536) as u16;
            // store last sent seq and timestamp for master reference
            stats.last_seq = seq;
            stats.last_sent = Some(std::time::Instant::now());
            seq
        };

        tokio::spawn(async move {
            let rec: PacketRecord = PacketRecord::new(seq);
            let res = pinger.ping(PingSequence(seq), &pl).await;
            update_ping_stats(tgt_clone, res, rec).await;
        });

        next_ping += app.ping_interval;
    }
}

/// Format a single target's data into a [TableRow]. Separate fn for ease of parallelization.
async fn format_row(t: &Arc<PingTarget>, debug: bool, timeout: Duration) -> TableRow {
    let snap: StatsSnapshot = {
        // Holding the lock inside this block only should minimize contention.
        // Do all the expensive string formatting afterwards.
        StatsSnapshot::new_from(&t.data.read(), timeout)
    };
    let status: String = match &snap.status {
        PingStatus::Error(e) if debug => e.to_string(),
        _ => snap.status.to_display(),
    };

    // Do all the (expensive) string formatting after releasing the lock.
    let mut row: TableRow = TableRow::from_iter([
        t.addr.to_string(),
        snap.sent.to_string(),
        snap.recv.to_string(),
        snap.loss_str(),
        snap.last_str(),
        snap.mean_str(),
        snap.min_str(),
        snap.max_str(),
        snap.stdev_str(),
        status,
    ]);
    if debug {
        row.add_item(snap.latest_seq.to_string());
    }

    // Add full-row styling based on statuses
    if t.is_paused() {
        row.set_style_all(Style::new().dim().italic());
    } else {
        match t.data.read().status {
            PingStatus::Error(_) => {
                row.set_style_all(Style::new().on_red());
            }
            PingStatus::NotReachable => {
                row.set_style_all(Style::new().light_red());
            }
            PingStatus::Timeout => {
                row.set_style_all(Style::new().light_magenta());
            }
            PingStatus::Lossy => {
                row.set_style_all(Style::new().light_yellow());
            }
            PingStatus::Laggy | PingStatus::Flappy => {
                row.set_style_all(Style::new().yellow());
            }
            _ => {}
        }
    }
    row
}

/// Gather current data from all targets.
#[inline]
async fn gather_target_data(tgts: &[Arc<PingTarget>], debug: bool, to: Duration) -> Vec<TableRow> {
    // Run all row formatter tasks concurrently
    join_all(tgts.iter().map(|t| format_row(t, debug, to))).await
}

/// Render the current frame. Display will be updated as soon as this function completes.
fn render_frame(frame: &mut Frame, state: &AppState, data: &[TableRow]) {
    let layout = &mut state.layout.write();
    layout.update(frame.area(), &data);

    let block = Block::bordered().title_bottom(Line::from(format!(" Targets: {} ", state.len())));
    let headers = state.headers.read();
    let table = Table::new(
        data.iter().map(|r| Row::new(r.cells())),
        &layout.tbl_constraints,
    )
    .header(Row::new(headers.cells()))
    .column_spacing(layout.tbl_colspacing)
    .block(block)
    .row_highlight_style(Style::new().reversed())
    .column_highlight_style(Style::new().bg(Color::Indexed(240)));

    let procinfo = Paragraph::new(format!(
        "CPU: {:>7} | mem: {} | pid: {}",
        state.pi.cpu_str(),
        state.pi.mem_str(),
        state.pi.pid,
    ))
    .alignment(Alignment::Right);

    frame.render_widget(&state.title, layout.title);
    frame.render_stateful_widget(table, layout.table, &mut layout.tablestate);
    frame.render_widget(procinfo, layout.status);
}

////////////////////////////////////////////////////////////////////////////////

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: Arc<MpConfig> = MpConfig::parse().into();

    let title = Line::from(format!("Multi-pinger v{}", conf.ver));
    let app: Arc<AppState> = AppState {
        title: Some(title.centered().style(Style::new().bold().on_green())),
        ..Default::default()
    }
    .build(
        &conf,
        make_targets(&conf.addrs, conf.histsize, conf.detailed),
    )?;

    // Setup table header style and cache widths. Layout is locked for this block only.
    {
        let mut headers = app.headers.write();
        headers.set_style_all(Style::new().bold().yellow());
        let mut layout = app.layout.write();
        layout.tbl_hdr_widths = headers.widths();
        layout.tbl_colspacing = 2;
    }

    // Spawn ping tasks
    {
        let mut tasks = app.tasks.write();
        for tgt in app.targets.read().iter() {
            let client = match tgt.addr {
                IpAddr::V4(_) => app.c_v4.as_ref().expect("IPv4 client missing"),
                IpAddr::V6(_) => app.c_v6.as_ref().expect("IPv6 client missing"),
            };
            tasks.push(tokio::spawn(ping_loop(
                tgt.clone(),
                client.clone(),
                app.clone(),
            )));
        }
    }

    // Full-console TUI initialization - the RAII guard will clean up on drop
    setup_signal_handler(app.quit.clone());
    let mut guard: TerminalGuard = TerminalGuard::new(app.ui_interval.as_millis(), app.verbose)?;
    let mut tick: Interval = time::interval(app.ui_interval);

    // Start the key event handling thread
    let app_clone: Arc<AppState> = app.clone();
    let kev_handle = thread::spawn(move || key_event_handler(app_clone));

    // Main display loop
    while !app.is_quitting() {
        tick.tick().await;
        if tokio::time::Instant::now() < *app.ui_next_refresh.read() {
            continue;
        }

        // Gather data for display and render the frame
        let data: Vec<TableRow> =
            gather_target_data(&app.targets.read(), app.debug, app.ping_timeout).await;
        guard
            .term
            .draw(|frame: &mut Frame| render_frame(frame, &app, &data))?;

        app.ui_tick();
    }

    // Cleanup
    drop(guard); // explicitly drop TUI guard to restore terminal so we can print
    if app.debug {
        eprintln!("Main thread quitting. Waiting for tasks to terminate...");
    }
    kev_handle.join().expect("Error joining key event handler thread");
    let mut tasks = app.tasks.write();
    join_all(tasks.iter_mut()).await;

    // Print final stats
    for line in simple_tabulate(
        &gather_target_data(&app.targets.read(), app.debug, app.ping_timeout).await,
        Some(&app.headers.read().strings()),
    ) {
        println!("{line}");
    }
    Ok(())
}

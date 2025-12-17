// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod ip_addresses;
mod latencywin;
mod macros;
mod pingdata;
mod strings;
mod structs;
mod tabulator;
mod tui;
mod utils;

use crate::{
    args::MpConfig,
    pingdata::{PacketRecord, PingStatus, PingTarget, StatsSnapshot},
    strings::*,
    structs::AppState,
    tabulator::simple_tabulate,
    tui::{TableRow, TerminalGuard, key_event_handler},
    utils::setup_signal_handler,
};

use futures::{
    future::join_all,
    stream::{FuturesUnordered, StreamExt},
};
use miniutils::ToDisplay;
use rand::{fill, random};
use ratatui::{prelude::*, widgets::*};
use std::{future::Future, net::IpAddr, sync::Arc, thread, time::Duration};
use surge_ping::{Client, PingIdentifier, PingSequence, Pinger};
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

/// Helper to mark a ping as sent and calculate the next sequence number.
/// Update sent timestamp as late as possible before sending so that the time
/// difference is minimized. There will still  be some delay due to task
/// scheduling etc, but this should be negligible compared to network latencies.
#[inline]
fn mark_sent_and_next_seq(tgt: &PingTarget) -> u16 {
    let mut stats = tgt.data.write();
    // update sent count here to make sure it's incremented before
    // sending so that the main sent count stays accurate even if
    // ping fails or we get out of order replies etc
    let sent: u64 = stats.sent;
    stats.sent += 1;

    // calculate the 16-bit sequence number from sent count,
    // since 2^16 is the max for ICMP sequence numbers
    let seq: u16 = (sent % 65_536) as u16;

    // store last sent seq and timestamp for master reference
    stats.last_seq = seq;
    stats.last_sent = Some(std::time::Instant::now());
    seq
}

/// Helper to clone the payload slice into a new one, which we can randomize if
/// needed. Internally, [Arc::make_mut] will perform a clone-on-write if necessary.
#[inline]
fn build_payload(app: &AppState) -> Arc<[u8]> {
    match app.randomize {
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
    }
}

/// Build a future that performs a single ping for the given target.
fn build_ping_future(
    tgt: Arc<PingTarget>,
    c: Arc<Client>,
    app: Arc<AppState>,
    id: PingIdentifier,
) -> impl Future<Output = ()> + Send + 'static {
    let pl: Arc<[u8]> = build_payload(&app);

    async move {
        let mut pinger: Pinger = c.pinger(tgt.addr, id).await;
        pinger.timeout(app.ping_timeout);

        let seq: u16 = mark_sent_and_next_seq(&tgt);
        let rec: PacketRecord = PacketRecord::new(seq);
        let res = pinger.ping(PingSequence(seq), &pl).await;
        tgt.update_stats(res, rec).await;
    }
}

/// Prepare and spawn a single ping task for the given target.
async fn ping_task(tgt: Arc<PingTarget>, c: &Arc<Client>, app: &Arc<AppState>, id: PingIdentifier) {
    // We must create a new Pinger for each async context, since otherwise we'll have
    // to wait for the previous ping to complete before sending the next one.
    let mut pinger: Pinger = c.pinger(tgt.addr, id).await;
    pinger.timeout(app.ping_timeout);

    let pl: Arc<[u8]> = build_payload(&app);
    let seq: u16 = mark_sent_and_next_seq(&tgt);

    app.inc_spawned_tasks();
    tokio::spawn(async move {
        let rec: PacketRecord = PacketRecord::new(seq);
        let res = pinger.ping(PingSequence(seq), &pl).await;
        tgt.update_stats(res, rec).await;
    });
}

/// Set up a ping loop for each target.
async fn ping_loop(tgt: Arc<PingTarget>, app: Arc<AppState>) {
    let client = match tgt.addr {
        IpAddr::V4(_) => app.c_v4.as_ref().expect(ERR_V4_MISSING),
        IpAddr::V6(_) => app.c_v6.as_ref().expect(ERR_V6_MISSING),
    };
    let id: PingIdentifier = PingIdentifier(random());
    let mut ticker: Interval = time::interval(app.internal_tick);
    let mut next_ping: Instant = tokio::time::Instant::now();

    // These variables are used only if perf mode is enabled
    let interval: f64 = app.ping_interval.as_secs_f64().max(1e-6); // 1 us min to avoid div by zero
    let timeout: f64 = app.ping_timeout.as_secs_f64();
    let max_inflight: usize = ((timeout / interval).ceil() as usize).clamp(1, 4);
    let mut inflight = FuturesUnordered::new();

    loop {
        tokio::select! {
            biased;
            true = app.is_quitting_async() => break,
            true = tgt.is_stopped_async() => break,

            Some(_) = inflight.next(), if app.perf() => { /* stats updated inside future */ }

            _ = ticker.tick() => {
                let now = tokio::time::Instant::now();
                if tgt.is_paused() {
                    // Adjust next ping time to not build a backlog while paused.
                    // When unpaused, the next ping should be pretty much immediate
                    // and subsequent pings will resume at the normal interval.
                    next_ping = now;
                    continue;
                } else if now < next_ping {
                    continue;
                }

                if app.perf() {
                    if inflight.len() >= max_inflight {
                        next_ping = now + app.ping_interval;
                        continue;
                    }
                    inflight.push(build_ping_future(tgt.clone(), client.clone(), app.clone(), id));
                } else {
                    ping_task(tgt.clone(), &client, &app, id).await;
                }
                next_ping += app.ping_interval;
            }
        }
    }

    // Drain outstanding pings (bounded by timeout, same practical behavior as with spawned tasks)
    while inflight.next().await.is_some() {}
}

/// Format a single target's data into a [TableRow]. Separate fn for ease of parallelization.
async fn format_row(tgt: &Arc<PingTarget>, debug: bool, timeout: Duration) -> TableRow {
    let snap: StatsSnapshot = StatsSnapshot::new_from(&tgt, timeout);
    let status: String = match &snap.status {
        PingStatus::Error(e) if debug => e.to_string(),
        _ => snap.status.to_display(),
    };

    // Do all the (expensive) string formatting after releasing the lock.
    let mut row: TableRow = TableRow::from_iter([
        tgt.addr.to_string(),
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
    if tgt.is_stopped() {
        row.set_style_all(Style::new().dim().italic().crossed_out());
    } else if tgt.is_paused() {
        row.set_style_all(Style::new().dim().italic());
    } else {
        match snap.status {
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

/// Gather current stringified data from some or all targets.
///
/// For large target lists, gathering data for all can be slow, hence
/// it makes sense to only gather data for the currently visible targets
/// in the TUI table. This function supports both modes via the `all` param.
async fn gather_target_data(state: &AppState, all: bool) -> Vec<TableRow> {
    let tgts = state.targets.read();
    let items: usize = tgts.len();
    let rows: usize = state.layout.read().tbl_usable_rows();

    // Full list requested, or we have fewer targets than rows
    if all || items <= rows {
        return join_all(
            tgts.iter()
                .map(|t| format_row(t, state.debug, state.ping_timeout)),
        )
        .await;
    }

    // Since the target list is longer than the table height, we have to fake
    // empty rows for Ratatui to render the full table. We create those to fill
    // the space. Depending on the offset, we need to do this at the start, end
    // or both. Chaining the Vec iterators makes this fairly straightforward.
    let offset: usize = state.layout.read().tablestate.offset();
    let end_pos: usize = offset + rows;
    let pre: Vec<TableRow> = vec![TableRow::new(); offset];
    let post: Vec<TableRow> = vec![TableRow::new(); items.saturating_sub(end_pos)];
    let visible: &[Arc<PingTarget>] = &tgts[offset..items.min(end_pos)];

    // Combine the Vec iterators
    pre.into_iter()
        .chain(
            join_all(
                visible
                    .iter()
                    .map(|t| format_row(t, state.debug, state.ping_timeout)),
            )
            .await
            .into_iter(),
        )
        .chain(post.into_iter())
        .collect()
}

/// Render the current frame. Display will be updated as soon as this function completes.
fn render_frame(frame: &mut Frame, state: &AppState, data: &[TableRow]) {
    let layout = &mut state.layout.write();
    layout.maybe_update(frame.area(), &data);
    let n: usize = state.len();

    // Border blocks with titles
    let block = Block::bordered();
    let b_tbl = block
        .clone()
        .title_bottom(Line::from(format!(" Targets: {} ", n)));
    let b_info_upper = block.clone().title_top(" Info ");

    // Data table
    let table = Table::new(
        data.iter().map(|r| <&TableRow as Into<Row>>::into(r)),
        &layout.tbl_constraints,
    )
    .header((&state.headers).into())
    .column_spacing(layout.tbl_colspacing)
    .block(b_tbl)
    .row_highlight_style(Style::new().reversed())
    .column_highlight_style(Style::new().bg(Color::Indexed(240)));

    // Info areas
    let info_upper = Paragraph::new(format!(
        " Selected: {}",
        &layout
            .tablestate
            .selected()
            .map_or("none".into(), |i| state.targets.read()[i.min(n - 1)]
                .addr
                .to_string())
    ))
    .block(b_info_upper);

    let info_lower = Paragraph::new(format!(
        " Interval: {} ms\n Timeout : {} ms\n Payload : {} bytes\n Tasks   : {} ",
        state.ping_interval.as_millis(),
        state.ping_timeout.as_millis(),
        state.payload.len(),
        state.spawned_tasks()
    ));

    let procinfo = Line::from(format!(
        "CPU: {:>7} | mem: {} | pid: {} ",
        state.pi.cpu_str(),
        state.pi.mem_str(),
        state.pi.pid,
    ))
    .alignment(Alignment::Right);

    state.status_line.replace(match state.debug {
        true => format!(
            " Data: {}, offset: {}, selected: {}",
            data.len(),
            &layout.tablestate.offset(),
            &layout
                .tablestate
                .selected()
                .map_or("none".into(), |i| i.to_string())
        ),
        false => " mping initialized and running. Press 'q' to quit, 'h' for help.".into(),
    });

    // Render all components. Order matters for layering; later ones overwrite earlier ones,
    // faking z-index behavior even though we're not working with "real" windows.
    frame.render_widget(&state.title, layout.title);
    frame.render_stateful_widget(table, layout.table, &mut layout.tablestate);
    frame.render_widget(procinfo, layout.status_r);
    frame.render_widget(info_upper, layout.info_upper);
    frame.render_widget(info_lower, layout.info_lower);
    frame.render_widget(state.status_line.clone().bold().as_line(), layout.status_l);

    // Render popup if visible and has contents
    if layout.popup_visible {
        if let Some(popup) = &*state.popup_contents.read() {
            frame.render_widget(Clear, layout.popup);
            frame.render_widget(
                popup.to_para().block(block.padding(Padding::horizontal(1))),
                layout.popup,
            );
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: MpConfig = MpConfig::parse();
    let app: Arc<AppState> = AppState::default().build(
        &conf,
        make_targets(&conf.addrs, conf.histsize, conf.detailed),
    )?;

    // Spawn ping tasks
    {
        let mut tasks = app.tasks.write();
        for tgt in app.targets.read().iter() {
            tasks.push(tokio::spawn(ping_loop(tgt.clone(), app.clone())));
            app.inc_spawned_tasks();
        }
    }

    // Full-console TUI initialization - the RAII guard will clean up on drop
    setup_signal_handler(app.quit.clone());
    let mut guard: TerminalGuard = TerminalGuard::new(app.ui_interval.as_millis(), app.verbose)?;
    let mut tick: Interval = time::interval(app.internal_tick);

    // Start the key event handling thread
    let app_clone: Arc<AppState> = app.clone();
    let kev_handle = thread::spawn(move || key_event_handler(app_clone));

    // Main display loop
    loop {
        tokio::select! {
            biased; // preferentially handle quit condition first, then rest in order
            true = app.is_quitting_async() => break,
            true = app.ui_refresh_elapsed_async() => {
                // Gather data for display and render the frame
                let data = gather_target_data(&app, false).await;
                guard.term.draw(|frame: &mut Frame| render_frame(frame, &app, &data))?;
                app.ui_schedule_next_refresh();
            },
            _ = app.key_event.notified() => {
                // Immediate refresh on key event. NOTE: don't reschedule next refresh!
                let data = gather_target_data(&app, false).await;
                guard.term.draw(|frame: &mut Frame| render_frame(frame, &app, &data))?;
                // sleep a little to avoid busy looping during key event bursts
                tokio::time::sleep(Duration::from_millis(5)).await;
            },
            _ = tick.tick() => { /* no-op, just to keep the select! happy */ }
        }
    }

    // Cleanup
    drop(guard); // explicitly drop TUI guard to restore terminal so we can print
    if app.debug {
        eprintln!("{INFO_QUITTING}");
    }
    kev_handle.join().expect(ERR_KEV_JOIN);
    let mut tasks = app.tasks.write();
    join_all(tasks.iter_mut()).await;

    // Print final stats
    let data: Vec<TableRow> = gather_target_data(&app, true).await;
    let rows: usize = app.layout.read().tbl_usable_rows().min(data.len());
    for line in simple_tabulate(&data[..rows], Some(&app.headers.strings())) {
        println!("{line}");
    }
    Ok(())
}

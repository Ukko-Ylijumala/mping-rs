// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod hopcount;
mod latencywin;
mod logging;
mod macros;
mod pingdata;
mod pinger;
mod strings;
mod structs;
mod ui;
mod utils;

use crate::{
    args::MpConfig,
    pingdata::{PingStatus, PingTarget, StatsSnapshot},
    pinger::ping_loop,
    strings::*,
    structs::{AppState, TargetDefaults},
    ui::{PopupContents, TerminalGuard, TuiState, keyboard::key_event_handler, tui::TableRow},
    utils::{human_rate, make_histogram_buckets, setup_signal_handler},
};

use futures::future::join_all;
use miniutils::{ToDisplay, inject, simple_tabulate, templater};
use ratatui::{prelude::*, widgets::*};
use std::{
    fmt::Display,
    net::IpAddr,
    sync::{Arc, LazyLock},
    thread,
    time::Duration,
};
use tokio::time::{self, Interval};

const GRAPH_SAMPLES: usize = 180; // 3 minutes @ default interval
const ICMP_HEADER_BYTES: usize = 8; // for send rate estimation (excludes IP overhead)

type WritableLayout<'a> =
    parking_lot::lock_api::RwLockWriteGuard<'a, parking_lot::RawRwLock, ui::tui::AppLayout>;

/* -------------------------------------------------------------------------- */

/// Create [PingTarget] instances for each IP address.
fn make_targets(addrs: &[IpAddr], defaults: &TargetDefaults) -> Vec<PingTarget> {
    addrs
        .iter()
        .map(|addr| PingTarget::new(*addr, defaults.histsize, defaults.detailed, defaults.paused))
        .collect()
}

/// Format a single target's data into a [TableRow]. Separate fn for ease of parallelization.
fn format_row(tgt: &Arc<PingTarget>, debug: bool, timeout: Duration) -> TableRow {
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

/**
Gather current stringified data from some or all targets.

For large target lists, gathering data for all can be slow, hence
it makes sense to only gather data for the currently visible targets
in the TUI table. This function supports both modes via the `all` param.
*/
fn gather_target_data(state: &AppState, tui: &TuiState, all: bool) -> Vec<TableRow> {
    /*
    Snapshot the Arc handles so the `targets` guard is dropped before any
    other lock is taken. Lock-order invariant: `targets` must never be held
    while acquiring `layout` (the keyboard thread takes them in the opposite
    order), or the two threads can deadlock.
    */
    let tgts: Vec<Arc<PingTarget>> = state.targets.read().iter().cloned().collect();
    let vp = tui.viewport(tgts.len());

    // Full list requested, or we have fewer targets than rows
    if all || !vp.needs_paging() {
        return tgts
            .iter()
            .map(|t| format_row(t, state.debug, state.defaults.timeout))
            .collect();
    }

    /*
    Since the target list is longer than the table height, we have to fake
    empty rows for Ratatui to render the full table. We create those to fill
    the space. Depending on the offset, we need to do this at the start, end
    or both. Chaining the Vec iterators makes this fairly straightforward.
    */
    let pre: Vec<TableRow> = vec![TableRow::new(); vp.offset];
    let post: Vec<TableRow> = vec![TableRow::new(); vp.targets - vp.end_pos];
    let visible: &[Arc<PingTarget>] = &tgts[vp.offset..vp.end_pos];

    // Combine the Vec iterators
    pre.into_iter()
        .chain(
            visible
                .iter()
                .map(|t| format_row(t, state.debug, state.defaults.timeout)),
        )
        .chain(post)
        .collect()
}

/* -------- Static Ratatui constructs for render_frame() -------- */
static ST_REV: Style = Style::new().reversed();
static ST_COL: Style = Style::new().bg(Color::Indexed(240));
static ST_CYAN: Style = Style::new().fg(Color::Cyan);
static ST_GREEN: Style = Style::new().fg(Color::Green);
static ST_YELLOW: Style = Style::new().fg(Color::Yellow);
static BORDERS: Block = Block::bordered();
static BLK_PAD_L1: Block = Block::new().padding(Padding::left(1));
static BLK_NFO_INNER: Block = Block::new()
    .borders(Borders::TOP)
    .border_type(BorderType::QuadrantOutside)
    .title_alignment(Alignment::Center);
static SCROLLBAR: Scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
static BLK_NFO: LazyLock<Block> = LazyLock::new(|| BORDERS.clone().title_top(INFO_INFO));
static BLK_POPUP: LazyLock<Block> = LazyLock::new(|| {
    BORDERS
        .clone()
        .padding(Padding::horizontal(1))
        .border_type(BorderType::Double)
        .border_style(Color::Indexed(190))
});
static BLK_HELP: LazyLock<Block> = LazyLock::new(|| {
    BORDERS
        .clone()
        .title(INFO_HELP)
        .padding(Padding::horizontal(1))
        .border_type(BorderType::QuadrantOutside)
        .border_style(Color::Indexed(216))
});
static PARA_NFO_SEL: LazyLock<Paragraph> = LazyLock::new(|| Paragraph::new(INFO_SELECT));
static PARA_NO_TGTS: LazyLock<Paragraph> = LazyLock::new(|| Paragraph::new(INFO_NO_TGTS));
static PARA_NO_DATA: LazyLock<Paragraph> = LazyLock::new(|| Paragraph::new(INFO_NO_RTT));
static DATASET_BASE: LazyLock<Dataset> = LazyLock::new(|| {
    Dataset::default()
        .name(INFO_RTT)
        .marker(symbols::Marker::Braille)
        .style(ST_CYAN)
        .graph_type(GraphType::Line)
});
static DATASET_DELTA: LazyLock<Dataset> = LazyLock::new(|| {
    Dataset::default()
        .name(INFO_RTT_D)
        .marker(symbols::Marker::Braille)
        .style(ST_YELLOW)
        .graph_type(GraphType::Line)
});
static HISTOGRAM_BASE: LazyLock<BarChart> = LazyLock::new(|| {
    BarChart::default()
        .bar_width(1)
        .bar_gap(0)
        .direction(Direction::Horizontal)
});

/**
Render the current frame. Display will be updated as soon as this function completes.

NOTE: the order of statements inside the function body is arranged such that
the time of holding the targets lock is minimized to reduce contention.
*/
fn render_frame(frame: &mut Frame, state: &AppState, tui: &TuiState, data: &[TableRow]) {
    let layout = &mut tui.layout.write();
    layout.maybe_update(frame.area(), &data);
    let tgts = state.targets.read();
    let num_tgts: usize = tgts.len();
    // actively pinged targets, for send rate / bandwidth display (cheap atomics)
    let active: usize = tgts
        .iter()
        .filter(|t| !t.is_paused() && !t.is_stopped())
        .count();
    let selected = layout
        .tablestate
        .selected()
        // TableState::selected can return an out-of-bounds index, so clamp it here.
        .and_then(|i: usize| tgts.get(i.min(num_tgts.saturating_sub(1))));

    /* -------- Info areas -------- */
    let w_info_upper = if let Some(t) = selected {
        let outages = t.outage_summary();
        Paragraph::new(templater!(
            INFO_TARGET,
            t.to_string(),
            &t.rev,
            t.ptr().to_string(),
            t.rev_ptr().to_string(),
            t.est_distance_str(state.distance_stretch_factor),
            t.hops().to_string(),
            t.jitter_str(),
            outages.counts_str(),
            outages.availability_str(),
        ))
    } else {
        PARA_NFO_SEL.clone()
    }
    .block(BLK_NFO.clone());

    /* -------- Recent RTT graph and histogram for selected target. -------- */
    if let Some(target) = selected {
        let rtt_data: Vec<(f64, f64)> = target.get_recent_rtts(GRAPH_SAMPLES);
        // release targets read lock here, not needed anymore
        drop(tgts);

        // Carve out areas for graph and histogram and define the inner Block.
        // We need to do this since the outer enclosing Block includes borders.
        let a_graph = BLK_NFO.inner(layout.i_upper_graph);
        let a_histo = BLK_NFO.inner(layout.i_upper_histo);
        let b_graph = BLK_NFO_INNER.clone().title(INFO_RTT_G);
        let b_histo = BLK_NFO_INNER.clone().title(INFO_RTT_H);

        if !rtt_data.is_empty() {
            let samples: usize = rtt_data.len();
            let sample_t: f64 = samples as f64 * state.defaults.interval.as_secs_f64();
            let mut values: Vec<f64> = Vec::with_capacity(samples);

            // get values and min/max RTT (for graph scaling) in one pass
            let (mut min_rtt, mut max_rtt) = rtt_data
                .iter()
                .map(|&(_, y)| {
                    values.push(y);
                    y
                })
                .fold((f64::INFINITY, 0.0), |(min, max), y| {
                    (f64::min(min, y), f64::max(max, y))
                });

            // round max_rtt up and min_rtt down for cleaner graph axis labels
            max_rtt = (max_rtt * 10.0).ceil() / 10.0;
            if min_rtt < 0.5 {
                min_rtt = 0.0;
            } else {
                min_rtt = (min_rtt * 10.0).floor() / 10.0;
            };

            /*
            Jitter overlay: |ΔRTT| between consecutive samples, hung from the
            chart ceiling growing downward (inverted). The y-axis usually
            starts at min RTT where raw deltas would be off-scale, and the
            RTT line itself wanders near its minimum - the top of the chart
            is the emptiest real estate. Calm = flat line along the ceiling;
            jitter events drip down from it at true ms scale (clamped to the
            floor).
            */
            let delta_data: Vec<(f64, f64)> = rtt_data
                .windows(2)
                .enumerate()
                .map(|(i, w)| {
                    (i as f64 + 1.0, (max_rtt - (w[1].1 - w[0].1).abs()).max(min_rtt))
                })
                .collect();

            // RTT line graph widget (with the jitter band overlaid)
            let mut datasets: Vec<Dataset> = vec![DATASET_BASE.clone().data(&rtt_data)];
            if delta_data.len() > 1 {
                datasets.push(DATASET_DELTA.clone().data(&delta_data));
            }
            let w_rtt_chart = Chart::new(datasets)
                .block(b_graph)
                .x_axis(
                    Axis::default()
                        .bounds([0.0, samples as f64 - 1.0])
                        .labels([format!("-{sample_t:.0}s").bold(), INFO_NOW.bold()]),
                )
                .y_axis(Axis::default().bounds([min_rtt, max_rtt]).labels([
                    format!("{min_rtt:.1}").bold().cyan(),
                    format!("{max_rtt:.1}").bold().cyan(),
                ]));

            // RTT histogram widget
            let bars = make_histogram_buckets(values, 8)
                .iter()
                .map(|b| {
                    Bar::default()
                        .value(b.count)
                        .label(format!("{:.2} - {:.2}", b.low, b.high))
                        .style(ST_GREEN)
                })
                .collect::<Vec<_>>();
            let w_histogram = HISTOGRAM_BASE
                .clone()
                .block(b_histo)
                .data(BarGroup::new(bars));

            frame.render_widget(w_rtt_chart, a_graph);
            frame.render_widget(w_histogram, a_histo);
        } else {
            frame.render_widget(PARA_NO_DATA.clone().block(b_graph), a_graph);
            frame.render_widget(PARA_NO_DATA.clone().block(b_histo), a_histo);
        }
    } else {
        drop(tgts);
    }

    /* -------- Lower info area - bottom right corner -------- */
    // Nominal ICMP send rate over the actively pinged targets. The actual
    // rate can be lower (perf-mode inflight cap, scheduling), but so can
    // the displayed interval/timeout be off - this is an info box.
    let pps: f64 = active as f64 / state.defaults.interval.as_secs_f64();
    let bw: String = human_rate(pps * (state.payload.len() + ICMP_HEADER_BYTES) as f64);
    #[rustfmt::skip]
    let w_info_lower = Paragraph::new(templater!(
        INFO_STATE,
        state.defaults.interval.as_millis(),
        state.defaults.timeout.as_millis(),
        state.payload.len(),
        if state.defaults.randomize { INFO_RAND } else { "" },
        state.spawned_tasks(),
        format!("{pps:.1}"),
        bw
    ))
    .block(BLK_PAD_L1.clone());

    /* -------- CPU & process info - bottom line, right side -------- */
    let w_procinfo = Line::from(templater!(
        INFO_CPU,
        format!("{:>7}", state.pi.cpu_str()),
        state.pi.mem_str(),
        state.pi.pid,
    ))
    .alignment(Alignment::Right);

    /* -------- Status line - bottom line, left side -------- */
    tui.status_line.replace(match state.debug {
        true => templater!(
            INFO_DEBUG,
            data.len(),
            &layout.tablestate.offset(),
            &layout
                .tablestate
                .selected()
                .map_or("none".into(), |i| i.to_string())
        ),
        false => APP_RUNNING.into(),
    });

    /* -------- Data table -------- */
    if num_tgts == 0 {
        frame.render_widget(PARA_NO_TGTS.clone().block(BORDERS.clone()), layout.table);
    } else {
        // table title shows the active sort column + direction, if any
        let tbl_title: String = match layout.sort_state {
            Some((col, desc)) => templater!(
                INFO_TGTS_SORTED,
                num_tgts,
                tui.headers.strings().get(col).copied().unwrap_or(MISSING),
                if desc { ARROW_DOWN } else { ARROW_UP }
            ),
            None => templater!(INFO_TGTS, num_tgts),
        };
        let b_tbl = BORDERS.clone().title_bottom(Line::from(tbl_title));
        let w_table = Table::new(
            data.iter().map(|r| <&TableRow as Into<Row>>::into(r)),
            &layout.tbl_constraints,
        )
        .header((&tui.headers).into())
        .column_spacing(layout.tbl_colspacing)
        .block(b_tbl)
        .row_highlight_style(ST_REV)
        .column_highlight_style(ST_COL);
        frame.render_stateful_widget(w_table, layout.table, &mut layout.tablestate);

        /*
        Render scrollbar if we have enough targets. Drawn over the right-side table border
        since we use `layout.table` and not `b_tbl.inner()` as the area (saves 1 column! :D).
        The scrollbar state is throwaway since there's no benefit to storing it in the layout:
        - the setters consume `self`, so we can't reuse the same instance anyway
        - doesn't support scrolling by N rows, which we need
        - would introduce more complexity in the keyboard handler
        - we already have the relevant info in the table state
        */
        if num_tgts > layout.tbl_usable_rows() {
            let bar_pos = layout
                .tablestate
                .selected()
                .unwrap_or_else(|| layout.tablestate.offset());
            frame.render_stateful_widget(
                SCROLLBAR.clone().style(ST_CYAN),
                layout.table,
                &mut ScrollbarState::new(num_tgts).position(bar_pos),
            );
        }
    }

    // Render all components. Order matters for layering; later ones overwrite earlier ones,
    // faking z-index behavior even though we're not working with "real" windows.
    frame.render_widget(&tui.title, layout.title);
    frame.render_widget(w_procinfo, layout.status_r);
    // NOTE: we're writing to `info_upper` instead of `i_upper_text` to get the border around it all.
    frame.render_widget(w_info_upper, layout.info_upper);
    frame.render_widget(w_info_lower, layout.info_lower);
    frame.render_widget(tui.status_line.clone().bold().as_line(), layout.status_l);

    // Finally render the popus if any are visible (on top of everything else)
    render_popups(frame, tui, layout);
}

/// Render the popups: text box, help.
fn render_popups(frame: &mut Frame, tui: &TuiState, layout: &mut WritableLayout) {
    /* -------- Text popup -------- */
    if layout.popup_visible {
        let contents = &*tui.popup_contents.read();
        if !contents.is_empty() {
            frame.render_widget(Clear, layout.popup);

            match contents {
                PopupContents::Buffer(buf) => {
                    let num = buf.len();
                    let b_popup = BLK_POPUP.clone().title(templater!(INFO_LOG, num));

                    if num <= layout.popup_usable_rows() {
                        // no statefulness needed here
                        frame.render_widget(contents.to_list().block(b_popup), layout.popup)
                    } else {
                        frame.render_stateful_widget(
                            contents.to_list().block(b_popup),
                            layout.popup,
                            &mut layout.liststate,
                        );

                        // Render scrollbar for log popup
                        let bar_pos = layout
                            .liststate
                            .selected()
                            .unwrap_or_else(|| layout.liststate.offset());
                        frame.render_stateful_widget(
                            SCROLLBAR.clone(),
                            layout.popup,
                            &mut ScrollbarState::new(num).position(bar_pos),
                        );
                    }
                }

                PopupContents::Multiline(_) => {
                    frame.render_widget(contents.to_list().block(BLK_POPUP.clone()), layout.popup)
                }

                // Pre-styled lines (event timeline): stateful so PageUp/Down scroll works
                PopupContents::Lines(_) => {
                    let num = contents.len();
                    if num <= layout.popup_usable_rows() {
                        frame.render_widget(contents.to_list().block(BLK_POPUP.clone()), layout.popup)
                    } else {
                        frame.render_stateful_widget(
                            contents.to_list().block(BLK_POPUP.clone()),
                            layout.popup,
                            &mut layout.liststate,
                        );
                        let bar_pos = layout
                            .liststate
                            .selected()
                            .unwrap_or_else(|| layout.liststate.offset());
                        frame.render_stateful_widget(
                            SCROLLBAR.clone(),
                            layout.popup,
                            &mut ScrollbarState::new(num).position(bar_pos),
                        );
                    }
                }

                _ => frame.render_widget(contents.to_para().block(BLK_POPUP.clone()), layout.popup),
            }
        }
    }

    /* -------- Input dialog -------- */
    if layout.input_visible {
        let state = &mut *tui.input_state.write();
        frame.render_stateful_widget(&layout.input, layout.input.area, state);
        if let Some(pos) = layout.input.cursor_position(state) {
            frame.set_cursor_position(pos);
        }
    }

    /* -------- Help popup -------- */
    if layout.help_visible {
        frame.render_widget(Clear, layout.help);
        frame.render_widget(
            tui.help_contents.to_para().block(BLK_HELP.clone()),
            layout.help,
        );
    }
}

/* -------------------------------------------------------------------------- */

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: MpConfig = MpConfig::parse().await?;
    let app: AppState = AppState::from_conf(&conf);
    let tgts: Vec<PingTarget> = make_targets(&conf.addrs, &app.defaults);
    let app: Arc<AppState> = app.build(tgts)?;
    let tui: Arc<TuiState> = TuiState::from_conf(&conf).build();

    // Spawn ping tasks
    {
        let mut tasks = app.tasks.write();
        for tgt in app.targets.read().iter() {
            tasks.push(app.spawn(ping_loop(tgt.clone(), app.clone())));
        }
    }

    // Full-console TUI initialization - the RAII guard will clean up on drop
    setup_signal_handler(app.quit.clone());
    let mut guard: TerminalGuard = TerminalGuard::new(tui.ui_interval, app.logger.clone())?;
    // The tick gates how often the select! loop re-evaluates the refresh
    // deadline, so it must not be coarser than the UI refresh interval.
    let mut tick: Interval = time::interval(app.internal_tick.min(tui.ui_interval));

    // Start the key event handling thread
    let app_clone: Arc<AppState> = app.clone();
    let tui_clone: Arc<TuiState> = tui.clone();
    let kev_handle = thread::spawn(move || key_event_handler(app_clone, tui_clone));

    // Main display loop
    loop {
        tokio::select! {
            biased; // preferentially handle quit condition first, then rest in order
            true = app.is_quitting_async() => break,
            true = tui.ui_refresh_elapsed_async() => {
                // Gather data for display and render the frame
                let data = gather_target_data(&app, &tui, false);
                guard.term.draw(|frame: &mut Frame| render_frame(frame, &app, &tui, &data))?;
                tui.ui_schedule_next_refresh();
            },
            _ = app.key_event.notified() => {
                // Immediate refresh on key event. NOTE: don't reschedule next refresh!
                let data = gather_target_data(&app, &tui, false);
                guard.term.draw(|frame: &mut Frame| render_frame(frame, &app, &tui, &data))?;
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
    // Take the handles out of the lock so the guard isn't held across the await.
    let tasks: Vec<tokio::task::JoinHandle<()>> = std::mem::take(&mut *app.tasks.write());
    join_all(tasks).await;

    // Print final stats
    let data: Vec<TableRow> = gather_target_data(&app, &tui, true);
    // Display the same rows as were visible in the TUI
    let vp = tui.viewport(app.len());
    simple_tabulate(&data[vp.offset..vp.end_pos], Some(&tui.headers.strings()))
        .iter()
        .for_each(|line| println!("{line}"));
    Ok(())
}

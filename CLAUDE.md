# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / run / test

- Build: `cargo build` (debug) or `cargo build --release` (LTO + opt-level 3).
- Run the main TUI: `./target/release/mping 8.8.8.8 1.1.1.1 10.0.0.0/28 172.16.1.1-10 ::1 dns.google`
- Run the hopcount helper binary: `./target/release/hopcount <ip>` (defined as a second `[[bin]]` in `Cargo.toml`).
- Tests: there is no test suite yet — `cargo test` is a no-op. The doc-comment in `latencywin.rs` shows the public API surface intended for future doctests.
- Lint: `cargo clippy --all-targets`. Format: `cargo fmt`.
- Raw ICMP sockets are required. Either run as root or grant `cap_net_raw` to the binary (`sudo setcap cap_net_raw=eip target/release/mping`); `nice_permission_error` in `utils.rs` rewrites the EPERM message to point at this.
- Rust edition is 2024 — code can use 2024-edition features (let-chains, `gen` keyword, etc.).
- Linux-only at present (`#[cfg(target_os = "linux")]` branches in `structs.rs` for things like `SYSTEM_TTL`).
- Two dependencies are pulled directly from GitHub (`miniutils`, `timesince` under `Ukko-Ylijumala`). A network-restricted build env will fail to fetch them.

## Architecture

The program is a Tokio-based concurrent multi-pinger with a Ratatui TUI. Three execution contexts run in parallel and coordinate through `AppState`:

1. **Per-target async ping loops** (`ping_loop` in `main.rs`) — one per `PingTarget`, scheduled on the multi-thread Tokio runtime (8 worker threads, configured in the `#[tokio::main]` attribute). Each loop drives a `tokio::time::Interval` and either spawns one task per ping (default) or pushes futures into a bounded `FuturesUnordered` (`--perf` mode, capped by `max_inflight = ceil(timeout/interval)` clamped to `[1,4]`). Both modes share the same `surge_ping::Client` per IP family (`c_v4`, `c_v6` on `AppState`) so the kernel ICMP socket is reused.
2. **Main render loop** (`main` in `main.rs`) — a `tokio::select!` over the quit flag, a refresh ticker, and a `Notify` woken by the keyboard thread. On each refresh it calls `gather_target_data` (which only formats the *visible* viewport rows unless `all=true`) and then `render_frame`. Final stats are tabulated to stdout after the TUI tears down.
3. **Keyboard event thread** (`key_event_handler` in `ui/keyboard.rs`) — a plain `std::thread` (not a Tokio task) that polls Crossterm events and translates them into `Command` variants dispatched via `AppState::execute`. Because this thread is *outside* the Tokio runtime, any code path it triggers that needs to spawn async work must go through `AppState::spawn` / `spawn_blocking`, which use the stored `runtime: tokio::runtime::Handle`. There is a load-bearing comment in `update_target_info` explaining this — don't replace those calls with bare `tokio::spawn`.

### Shared state

- `AppState` (in `structs.rs`) is the single shared object, wrapped in `Arc` after `build()`. It owns the targets, the ICMP clients, the Tokio runtime handle, the logger, the resolver, the quit flag, the key-event `Notify`, and a counter of spawned tasks. Construction is two-phase: `from_conf()` then `.build(targets)` — this exists so `build()` can return an error from `Client::new` while keeping the rest of construction infallible.
- `TuiState` (in `ui/tui.rs`) holds Ratatui layout (`AppLayout` behind a `parking_lot::RwLock`), popup contents, input dialog state, headers, and the refresh schedule. The layout is recomputed via `maybe_update` only when the frame area or row count changes.
- `PingTarget` (in `pingdata.rs`) wraps `PingTargetInner` (sent/recv counters, `LatencyWindow`, `PacketHistory`, raw status, last sequence) inside a `parking_lot::RwLock`, plus separate atomic flags for paused/stopped/unreachable. `LatencyWindow` (in `latencywin.rs`) is an O(1) amortized rolling window over the last N RTTs.

### Locking and concurrency conventions

- All locks are `parking_lot::RwLock` (no poisoning, faster than std). `render_frame` is deliberately structured to drop the `targets` read lock as early as possible — preserve that ordering when editing it.
- Atomics use `Ordering::Relaxed` for visibility-only flags (quit, paused, stopped, perf, spawned-task counter). Don't tighten these without a reason.
- `mark_sent_and_next_seq` increments `sent` *before* the actual `pinger.ping(...)` call so the sent count stays accurate even if a ping fails or replies arrive out of order.
- `ping_loop` uses `tokio::select!` with `biased` so the quit/stop checks always win over ticks — keep that ordering when adding new branches.

### Signal handling and terminal cleanup

`TerminalGuard` (in `ui/tui.rs`) is RAII over enter/leave alternate screen + raw mode and also installs a panic hook that restores the terminal before unwinding. `setup_signal_handler` (in `utils.rs`) installs a dedicated thread that flips the quit flag on SIGINT/SIGTERM/SIGQUIT (raw mode swallows Ctrl-C, so the keyboard handler also maps Ctrl-C → `Command::Quit`). SIGKILL cannot be caught — the README's `tput reset` note is the user-facing recovery.

### Module layout

- `args.rs` — clap config (`MpConfig`), CLI parsing including DNS resolution of unparseable targets via `hickory_resolver`.
- `pingdata.rs` — `PingTarget`, `PingTargetInner`, `PingStatus`, `PacketRecord`, `PacketHistory`, `StatsSnapshot`, distance/hop estimation.
- `latencywin.rs` — rolling-window stats; `pub` because it appears in a doctest example.
- `hopcount/` — both a module (`pub use determine_hops` from `lib.rs`) and a separate binary (`src/hopcount/main.rs`).
- `ui/` — `tui.rs` (layout + `TerminalGuard` + `TuiState`), `keyboard.rs` (event handler thread), `input.rs` (the add-target dialog as a `StatefulWidget`).
- `strings.rs` — all user-facing string constants; many other modules use `crate::strings::*`.
- `logging.rs` — in-memory `MessageBuffer` with syslog-style levels, surfaced via the log popup.
- `macros.rs` — `delegate_read!` / `delegate_write!` for forwarding methods to `RwLock`-wrapped inner types.

### Adding a new keyboard command

Add a `Command` variant in `structs.rs`, handle it in `AppState::execute`, then map a key in `ui/keyboard.rs`. The key handler should call `app.execute(...)`; the main loop's `key_event` `Notify` causes an immediate redraw without waiting for the refresh tick.

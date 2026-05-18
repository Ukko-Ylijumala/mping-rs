# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Design docs (read these first)

The architecture is documented in [`doc/design/`](doc/design/) — one file per
significant feature. Start with [`doc/design/README.md`](doc/design/README.md)
for the index. Before making non-trivial changes, find the design doc(s)
covering the area you're touching:

| Topic | Doc |
|---|---|
| Concurrency, async ping loop, perf/F10 toggle | [concurrency.md](doc/design/concurrency.md) |
| Locking conventions, `AppState`, `CancellationToken` | [shared-state.md](doc/design/shared-state.md) |
| `PingTarget` data model & status derivation | [per-target-data.md](doc/design/per-target-data.md) |
| `LatencyWindow` rolling stats | [latency-window.md](doc/design/latency-window.md) |
| TUI render loop & `AppLayout` | [tui-rendering.md](doc/design/tui-rendering.md) |
| Keyboard handling, `Command` dispatch, how to add commands | [keyboard-and-commands.md](doc/design/keyboard-and-commands.md) |
| `TerminalGuard`, signals, panic hook | [signal-and-terminal.md](doc/design/signal-and-terminal.md) |
| IP / CIDR / range / DNS parsing | [target-parsing.md](doc/design/target-parsing.md) |
| Runtime add-target dialog | [add-target-dialog.md](doc/design/add-target-dialog.md) |
| `MessageBuffer` and the log popup | [logging.md](doc/design/logging.md) |
| Hop-count estimation (library + `hopcount` bin) | [hopcount.md](doc/design/hopcount.md) |
| RTT → distance estimate | [distance-estimation.md](doc/design/distance-estimation.md) |

When the design docs and the code disagree, the code is authoritative — but
update the doc in the same change.

## Build / run / test

- Build: `cargo build` (debug) or `cargo build --release` (LTO + opt-level 3).
- Run the main TUI: `./target/release/mping 8.8.8.8 1.1.1.1 10.0.0.0/28 172.16.1.1-10 ::1 dns.google`
- Run the hopcount helper binary: `./target/release/hopcount <ip>` (defined as a second `[[bin]]` in `Cargo.toml`).
- Tests: there is no test suite yet — `cargo test` exercises only the unit tests inside `src/latencywin.rs`. The doc-comment in `latencywin.rs` shows the public API surface intended for future doctests.
- Lint: `cargo clippy --all-targets`. Format: `cargo fmt`.
- Raw ICMP sockets are required. On Linux, either run as root or grant `cap_net_raw` to the binary (`sudo setcap cap_net_raw=eip target/release/mping`). On macOS, run as root. `nice_permission_error` in `utils.rs` rewrites the EPERM message to point at this.
- Rust edition is 2024 — code can use 2024-edition features (let-chains, `gen` keyword, etc.).
- Linux and macOS are supported (`#[cfg(target_os = "...")]` branches in `structs.rs`/`utils.rs` cover platform-specific bits like `SYSTEM_TTL` and the permission-error advice). Windows support is on the wishlist but not started.
- Two dependencies are pulled directly from GitHub (`miniutils`, `timesince` under `Ukko-Ylijumala`). A network-restricted build env will fail to fetch them.

## Module layout (quick reference)

- `args.rs` — clap config (`MpConfig`), CLI parsing including DNS resolution of unparseable targets via `hickory_resolver`.
- `pingdata.rs` — `PingTarget`, `PingTargetInner`, `PingStatus`, `PacketRecord`, `PacketHistory`, `StatsSnapshot`, distance/hop estimation.
- `pinger.rs` — `ping_loop`, the perf-mode `FuturesUnordered` path, and `collect_and_spawn` for the add-target dialog.
- `latencywin.rs` — rolling-window stats; `pub` because it appears in a doctest example.
- `hopcount/` — both a module (`pub use determine_hops` from `lib.rs`) and a separate binary (`src/hopcount/main.rs`).
- `ui/` — `tui.rs` (layout + `TerminalGuard` + `TuiState`), `keyboard.rs` (event handler thread), `input.rs` (the add-target dialog as a `StatefulWidget`).
- `strings.rs` — all user-facing string constants; many other modules use `crate::strings::*`.
- `logging.rs` — in-memory `MessageBuffer` with syslog-style levels, surfaced via the log popup.
- `macros.rs` — `delegate_read!` / `delegate_write!` for forwarding methods to `RwLock`-wrapped inner types, plus `eprintln_nomangle!` for alt-screen-aware logging.
- `structs.rs` — `AppState`, `Command` enum, `TargetDefaults`, `Resolved`, `QueryResponse`.

## A few invariants worth not breaking

These are spelled out in the design docs; this is the cheat sheet.

- **The keyboard thread is *not* in the Tokio runtime.** Any async work it triggers must go through `AppState::spawn` / `spawn_blocking` (which use the stored `runtime: Handle`). Bare `tokio::spawn` will panic. See [keyboard-and-commands.md](doc/design/keyboard-and-commands.md) and the load-bearing comment in `structs.rs:328`.
- **`ping_loop`'s `tokio::select!` is `biased`.** Quit/stop checks must always come before tick handling. See [concurrency.md](doc/design/concurrency.md).
- **`render_frame` is ordered to drop the `targets` read lock as early as possible.** All expensive string formatting happens *after* releasing the lock. See [shared-state.md](doc/design/shared-state.md).
- **`mark_sent_and_next_seq` increments `sent` *before* the network call** so the count stays accurate on errors / out-of-order replies. See [per-target-data.md](doc/design/per-target-data.md).
- **`TerminalGuard` sets the panic hook *before* enabling raw mode.** Keep that order in `TerminalGuard::new`. See [signal-and-terminal.md](doc/design/signal-and-terminal.md).

## Adding a new keyboard command

See [keyboard-and-commands.md](doc/design/keyboard-and-commands.md#adding-a-new-keyboard-command) — short version: add a `Command` variant, handle it in `AppState::execute`, map a key in `ui/keyboard.rs`. The `key_event` `Notify` causes an immediate redraw with no further plumbing.

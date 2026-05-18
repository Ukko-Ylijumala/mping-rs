# Signal handling & terminal cleanup

The TUI puts the terminal into raw mode on an alternate screen. Any path
that ends the program — clean exit, signal, or panic — must put it back, or
the user is left with no echo, no cursor, and no working line editor.
Three pieces cooperate to make that bulletproof: `TerminalGuard`, the
signal thread, and a panic hook.

## `TerminalGuard` (RAII)

Defined at `src/ui/tui.rs:820`. Constructed once near the top of `main`
(`src/main.rs:479`).

```text
TerminalGuard::new(interval, logger)
  ↓ panic::set_hook(panic_handler)          ── set the hook FIRST
  ↓ enable_raw_mode()
  ↓ execute!(stdout, EnterAlternateScreen, Hide)
  ↓ set_alt_screen_active(true)
  ↓ return Self { term, logger }

Drop for TerminalGuard
  ↓ terminal_teardown()
      ├ disable_raw_mode()
      ├ execute!(stdout, LeaveAlternateScreen, Show)
      └ set_alt_screen_active(false)
```

The panic hook is installed **before** raw mode is enabled (`tui.rs:833`)
so any panic during initialization, however unlikely, still gets cleanup.
Cleanup on drop is automatic for normal exit; `main` explicitly
`drop(guard)` before printing the final stats so the post-TUI output
appears on the normal terminal (`main.rs:510`).

## The signal thread

`setup_signal_handler(quit)` (`src/utils.rs:43-61`) installs a separate
`std::thread` that loops over signals coming through `signal_hook::Signals`:

```text
for sig in signals.forever() {
    eprintln!("got {sig}");
    quit.store(true, Relaxed);
}
```

We listen for `SIGINT`, `SIGTERM`, and `SIGQUIT`. The thread does not call
back into the application — it just flips `AppState::quit`, which both
the render loop (`is_quitting_async`) and every `ping_loop` notice on
their next `select!` iteration and break out cleanly. Cleanup then runs
through the normal exit path: ping tasks join, the `TerminalGuard` drops,
the terminal is restored.

`SIGQUIT` normally cores; here we just treat it like a graceful quit.
`SIGKILL` cannot be caught — the README's `tput reset` note is the
user-facing recovery.

## Why the keyboard handler also catches Ctrl-C

Many terminal emulators do *not* deliver `SIGINT` to a process in raw
mode. So `key_event_poll` includes an explicit
`(KeyCode::Char('c'), CONTROL) => Command::Quit` arm
(`src/ui/keyboard.rs:50`) for the common case where the user expects
Ctrl-C to work and the signal never arrives.

If the signal *does* arrive (e.g. from `kill -INT` from another terminal),
the signal thread handles it. Both paths converge on the same quit flag.

## Panic hook

`panic_handler` (`src/ui/tui.rs:862`) calls `terminal_teardown()` and then
re-emits the panic info on stderr. Because the alt-screen flag is set
back to `false` before the panic message prints, the message lands on the
real terminal rather than getting eaten by the alternate buffer.

## `ALT_SCREEN_ACTIVE` and `eprintln_safe`

`ALT_SCREEN_ACTIVE` (`tui.rs:44`) is a static `AtomicBool` that mirrors
"are we currently in the alternate screen?" The `eprintln_safe` helper
(`tui.rs:881`) only prints when the flag is `false`, so log messages that
would otherwise pollute the TUI alt-screen during runtime are suppressed
until teardown. `MessageBuffer::add` uses the `eprintln_nomangle!` macro
which routes through this guard (`logging.rs:245`).

## `tput reset` as last resort

If the program is killed with `SIGKILL`, or crashes in a way that
bypasses the panic hook (e.g. an abort from FFI), the terminal will be
left in raw mode with the cursor hidden. The README documents the
recovery: type `tput reset` blindly into the terminal. There's no way to
do better here — a process that can't run code can't undo its terminal
state.

## File map

- `src/ui/tui.rs:820-865` — `TerminalGuard`, `terminal_teardown`,
  `panic_handler`, `set_alt_screen_active`, `eprintln_safe`.
- `src/utils.rs:43-61` — `setup_signal_handler`.
- `src/main.rs:478-510` — wiring of the guard, signal handler, and
  explicit `drop(guard)` before final stats.
- `src/ui/keyboard.rs:50` — the Ctrl-C arm.

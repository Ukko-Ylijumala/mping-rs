# mping

mping is a small concurrent multi-pinger with a curses UI that displays live RTT stats for multiple IPv4/IPv6 targets.

- Source: [src/main.rs](src/main.rs) (entry: [`main`](src/main.rs))
- Manifest: [Cargo.toml](Cargo.toml)

Quick features
- Concurrent pings per target using Tokio and [`surge-ping`](Cargo.toml).
- Live ncurses UI showing Sent, Recv, Latest, Mean, Min, Max and Status.
- IPv4 and IPv6 support.
- Graceful Ctrl-C handling (see [`setup_signal_handler`](src/main.rs)).

Key implementation points
- Targets are represented by [`PingTarget`](src/main.rs) and created with [`make_targets`](src/main.rs).
- Per-target ping loop: [`ping_loop`](src/main.rs) which spawns [`ping`](src/main.rs) tasks.
- CLI parsing uses [`parse_ip_addrs`](src/main.rs).

Build and run
```sh
cargo build --release
./target/release/mping 8.8.8.8 1.1.1.1
```
Note: raw ICMP sockets not required, but appropriate capabilities may be needed.

Usage
- Provide one or more IP addresses as arguments.
- Press Ctrl-C to exit; the program restores the terminal before quitting.

License
- See the crate metadata in [Cargo.toml](Cargo.toml) (license: MIT OR Apache-2.0).

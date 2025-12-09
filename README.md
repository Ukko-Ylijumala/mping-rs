# mping

mping is a small concurrent TUI multi-pinger that displays live RTT stats for multiple IPv4/IPv6 targets.

- Source: [src/main.rs](src/main.rs) (entry: [`main`](src/main.rs))
- Manifest: [Cargo.toml](Cargo.toml)

### Quick features
- Concurrent async pings per target using Tokio and [`surge-ping`](Cargo.toml).
- Live TUI showing Sent, Recv, Latest, Mean, Min, Max and Status.
- Uses Ratatui for display and Crossterm for terminal control.
- IPv4 and IPv6 support.
- Graceful signal handling (see [`setup_signal_handler`](src/utils.rs)).
- Configurable interval, timeout and ICMP payload size/randomization.

### Key implementation points
- Targets are represented by [`PingTarget`](src/main.rs) and created with [`make_targets`](src/main.rs).
- Per-target ping loop: [`ping_loop`](src/main.rs) which spawns async pinger tasks.
- CLI IP address parsing uses [`parse_ip_or_range`](src/ip_addresses.rs).
- Panics and SIGINT/SIGTERM/SIGQUIT are handled such that console state is restored.

### Install Rust toolchain
```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Build and run
```sh
cargo build --release
./target/release/mping 8.8.8.8 1.1.1.1 10.0.0.0/28 172.16.1.1-10 ::1
```
Note: raw ICMP sockets are required and appropriate capabilities may be needed.

### Usage
- Provide one or more IP addresses and/or ranges as arguments.
- Press Ctrl-C or "q" to exit; the program restores the terminal before quitting.
- Target list can be scrolled with arrow up/down, page up/down and home/end
- Selected target can be paused with <space> and pinging resetted with "R"
- Clear selection with <backspace>

SIGKILL cannot be caught, hence console may be left in an unusable state after it because Curses cleanup code has no chance to executed. For example
```sh
tput reset
```
can be blindly entered in the terminal in that case to restore a working console.

### Help message (v0.2.9)
```
Multi-pinger utility written in Rust

Usage: mping [OPTIONS] <IP1 [IP2...]>...

Arguments:
  <IP1 [IP2...]>...  Space separated list of IP addresses or ranges to monitor

Options:
      --exclude=<IP1[,IP2...]>  Comma-separated IP addresses (and/or ranges) to exclude
  -I, --interval <SECS>         Interval between pings to each target [0.01-10] [default: 1]
  -T, --timeout <SECS>          Timeout for each ping request [0.01-5] [default: 2]
  -s, --size <BYTES>            Size of ICMP payload (minus the 8-byte ICMP header) [32-32760] [default: 32]
  -R, --randomize               Randomize ICMP payload data [default: no]
  -H, --histsize <NUM>          Full history size (number of ping results to keep per target) [60-65536] [default: 3600]
      --detailed <NUM>          Detailed recent history size (for laggy/flappy detection etc) [10-1000] [default: 100]
      --refresh <ms>            TUI refresh interval in milliseconds [100-5000] [default: 250]
  -v, --verbose                 Increase output verbosity
      --debug                   Print debug information where applicable
  -h, --help                    Print help
  -V, --version                 Print version
```

### License
- See the crate metadata in [Cargo.toml](Cargo.toml) (license: MIT OR Apache-2.0).

### NOTE
This application is a WIP and bugs are to be expected. YMMV, caveat emptor etc.

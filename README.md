# mping

mping is a small concurrent multi-pinger TUI app that displays live RTT stats, graphs and histograms for multiple IPv4/IPv6 targets.

### Quick features
- Concurrent async pings per target using [`Tokio`](https://crates.io/crates/tokio) and [`surge-ping`](https://crates.io/crates/surge-ping).
- Live TUI showing Sent, Recv, Latest, Mean, Min, Max and Status.
- Uses [`Ratatui`](https://crates.io/crates/ratatui) for display and [`Crossterm`](https://crates.io/crates/crossterm) for low-level terminal control.
- IPv4 and IPv6 support.
- Graceful signal handling (see [`setup_signal_handler`](src/utils.rs)).
- Configurable interval, timeout and ICMP payload size/randomization.
- Tries to resolve targets which fail to parse as IPs/ranges/CIDRs as DNS names
- Configurable DNS servers and query timeouts

### Key implementation points
- Targets are represented by [`PingTarget`](src/pingdata.rs) struct.
- Per-target ping loop: [`ping_loop`](src/main.rs) which spawns async pinger tasks (or inlined futures with `--perf`).
- CLI IP address parsing uses [`parse_ip_addresses`](src/utils.rs).
- DNS resolution uses [`hickory_resolver`](https://crates.io/crates/hickory-resolver)
- Panics and SIGINT/SIGTERM/SIGQUIT are handled such that previous console state is restored.

See the manifest file for dependencies: [Cargo.toml](Cargo.toml)

Currently only Linux is supported, but MacOS and Windows support is planned at some point.

### Install Rust toolchain
```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Build and run
```sh
cargo build --release
./target/release/mping 8.8.8.8 1.1.1.1 10.0.0.0/28 172.16.1.1-10 ::1 dns.google
```
Note: raw ICMP sockets are required and appropriate capabilities may be needed.

### Usage
- Provide one or more IP addresses and/or ranges (or DNS names) as arguments.
- Press Ctrl-C or "q" to exit; the program restores the terminal before quitting.
- Target list can be scrolled with arrow up/down, page up/down and home/end
- Selected target can be paused with <space> and pinging stats resetted with "R"
- Clear selection with `<backspace>`

SIGKILL cannot be caught, hence the console may be left in an unusable state afterwards as terminal cleanup code has no chance to run. If that happens,
```sh
tput reset
```
can be blindly entered in the terminal to restore it to a sane state.

### Help message (v0.2.9 as of 2026-01-27)
```
Multi-pinger utility written in Rust

Usage: mping [OPTIONS] [IP1 [IP2...]]...

Arguments:
  [IP1 [IP2...]]...  Space separated list of IP addresses or ranges to monitor

Options:
      --exclude=<IP1[,IP2...]>      Comma separated IP addresses (and/or ranges) to exclude
  -I, --interval <SECS>             Interval between pings to each target [0.01 - 10] [default: 1]
  -T, --timeout <SECS>              Timeout for each ping request [0.01 - 5] [default: 2]
  -s, --size <BYTES>                Size of ICMP payload (minus the 8-byte ICMP header) [32 - 32760] [default: 48]
  -R, --randomize                   Randomize ICMP payload data [default: no]
  -H, --histsize <NUM>              Full history size (number of ping results to keep per target) [60 - 65536] [default: 3600]
      --detailed <NUM>              Detailed recent history size (for laggy/flappy detection etc) [10 - 1000] [default: 100]
      --paused                      Start with pinging paused for all targets [default: no]
      --refresh <ms>                TUI refresh interval in milliseconds [50 - 5000] [default: 250]
      --dns-servers=<IP1[,IP2...]>  Comma separated list of DNS servers to use instead of system defaults
      --dns-timeout <SECS>          DNS resolution timeout in seconds [1 - 10] [default: 5] [default: 5]
      --stretch-factor <FLOAT>      Stretch factor for distance estimation (over 1.0 => compress distances) [default: 1.0]
      --perf                        Try to be more performant by reducing task spawn overhead
  -v, --verbose                     Increase output verbosity
      --debug                       Print debug information where applicable
  -h, --help                        Print help
  -V, --version                     Print version
```

### License
- See the crate metadata in [Cargo.toml](Cargo.toml) (license: MIT OR Apache-2.0).

### NOTE
This application is a WIP and bugs are to be expected. YMMV, caveat emptor etc. Also, this README is incomplete...

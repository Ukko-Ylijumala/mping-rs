use futures::future::join_all;
use ncurses::*;
use rand::random;
use std::{
    collections::VecDeque,
    fmt::Display,
    net::IpAddr,
    process::exit,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use surge_ping::{Client, Config, ICMP, PingIdentifier, PingSequence, Pinger, SurgeError};
use tokio::{sync::Mutex, time::sleep};

const MAX_HISTORY: usize = 65536;
const PING_INTERVAL: Duration = Duration::from_secs(1);
const PING_TIMEOUT: Duration = Duration::from_millis(900);
const PING_DATA: &[u8] = &[0; 32];

#[derive(Debug)]
enum PingStatus {
    Ok,
    Timeout,
    Error,
    None,
}

impl Display for PingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "timeout"),
            PingStatus::Error => write!(f, "err"),
            PingStatus::None => write!(f, "-"),
        }
    }
}

#[derive(Debug)]
struct PingTargetInner {
    sent: u64,
    recv: u64,
    rtts: VecDeque<u32>, // RTTs in microseconds
    status: PingStatus,
}

#[derive(Debug)]
struct PingTarget {
    addr: IpAddr,
    data: Mutex<PingTargetInner>,
}

fn parse_ip_addrs(args: &[String]) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = Vec::new();

    for arg in args.iter().skip(1) {
        if let Ok(ip) = arg.parse::<IpAddr>() {
            ips.push(ip);
        } else {
            eprintln!("Invalid IP address: {}", arg);
        }
    }
    ips
}

fn make_targets(addrs: Vec<IpAddr>) -> Vec<Arc<PingTarget>> {
    addrs
        .into_iter()
        .map(|addr| {
            Arc::new(PingTarget {
                addr,
                data: Mutex::new(PingTargetInner {
                    sent: 0,
                    recv: 0,
                    rtts: VecDeque::with_capacity(MAX_HISTORY),
                    status: PingStatus::None,
                }),
            })
        })
        .collect()
}

async fn ping(pinger: Arc<Mutex<Pinger>>, tgt: Arc<PingTarget>, seq: u16) {
    let mut pinger = pinger.lock().await;
    match pinger.ping(PingSequence(seq), &PING_DATA).await {
        Ok((_, dur)) => {
            let mut stats = tgt.data.lock().await;
            stats.recv += 1;
            stats.rtts.push_back(dur.as_micros() as u32);
            if stats.rtts.len() > MAX_HISTORY {
                stats.rtts.pop_front();
            }
            stats.status = PingStatus::Ok;
        }
        Err(e) => {
            let mut stats = tgt.data.lock().await;
            stats.status = match e {
                SurgeError::Timeout { .. } => PingStatus::Timeout,
                _ => PingStatus::Error,
            };
        }
    };
}

async fn ping_loop(tgt: Arc<PingTarget>, client: Client, quit: Arc<AtomicBool>) {
    let id: PingIdentifier = PingIdentifier(random());
    let mut pinger: Pinger = client.pinger(tgt.addr, id).await;
    pinger.timeout(PING_TIMEOUT);
    // Wrap pinger in Arc<Mutex<>> for shared async access
    let pinger: Arc<Mutex<Pinger>> = Arc::new(Mutex::new(pinger));

    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }

        let seq: u16 = {
            let mut stats = tgt.data.lock().await;
            // update sent count here to make sure it's incremented before
            // sending so that the main sent count stays accurate even if
            // ping fails or we get out of order replies etc
            let sent: u64 = stats.sent;
            stats.sent += 1;
            // calculate the 16-bit sequence number from sent count,
            // since 2^16 is the max for ICMP sequence numbers
            (sent % u16::MAX as u64) as u16
        };
        tokio::spawn(ping(pinger.clone(), tgt.clone(), seq));
        sleep(PING_INTERVAL).await;
    }
}

/// Set up the signal handler to catch Ctrl-C
fn setup_signal_handler(quit: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        quit.store(true, Ordering::Relaxed);
        curs_set(CURSOR_VISIBILITY::CURSOR_VISIBLE);
        echo();
        endwin();
        println!("Exiting, Ctrl-C pressed...");
    })
    .expect("Error setting Ctrl-C handler");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        let msg: String = format!("Usage: {} <IP1> [IP2] ...", args[0]);
        eprintln!("{}", msg);
        exit(1)
    }

    let addrs: Vec<IpAddr> = parse_ip_addrs(&args);
    if addrs.is_empty() {
        return Err("No valid IP addresses provided.".into());
    }
    let targets: Vec<Arc<PingTarget>> = make_targets(addrs);
    let quit: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Pinger clients
    let client_v4: Client = Client::new(&Config::default())?;
    let client_v6: Client = Client::new(&Config::builder().kind(ICMP::V6).build())?;

    // Spawn ping tasks
    for tgt in &targets {
        let client = match tgt.addr {
            IpAddr::V4(_) => client_v4.clone(),
            IpAddr::V6(_) => client_v6.clone(),
        };
        tasks.push(tokio::spawn(ping_loop(tgt.clone(), client, quit.clone())))
    }

    // Curses initialization
    initscr();
    noecho();
    curs_set(CURSOR_VISIBILITY::CURSOR_INVISIBLE);
    setup_signal_handler(quit.clone());

    // Main display loop
    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        clear();
        mvprintw(
            0,
            0,
            "Address\t\tSent\tRecv\tLatest\tMean\tMin\tMax\tStatus",
        );

        for (row, tgt) in targets.iter().enumerate() {
            let stats = tgt.data.lock().await;

            let (latest, mean, min, max) = if stats.rtts.is_empty() {
                (
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                )
            } else {
                let last: f64 = *stats.rtts.back().unwrap() as f64 / 1e3; // convert to ms
                let sum: f64 = stats.rtts.iter().map(|&x| (x as f64 / 1e3)).sum();
                let m: f64 = sum / stats.rtts.len() as f64;
                let min_v: f64 = *stats.rtts.iter().min().unwrap() as f64 / 1e3;
                let max_v: f64 = *stats.rtts.iter().max().unwrap() as f64 / 1e3;
                (
                    format!("{:.2}", last),
                    format!("{:.2}", m),
                    format!("{:.2}", min_v),
                    format!("{:.2}", max_v),
                )
            };

            mvprintw(
                (row + 1) as i32,
                0,
                &format!(
                    "{:<12}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    tgt.addr, stats.sent, stats.recv, latest, mean, min, max, stats.status
                ),
            );
        }

        refresh();
        sleep(Duration::from_millis(200)).await;
    }

    join_all(tasks).await;
    Ok(())
}

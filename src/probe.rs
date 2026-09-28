//! Probe runners. Each endpoint gets its own task that emits events on a channel.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use surge_ping::{Client, Config, PingIdentifier, PingSequence};
use tokio::sync::{Semaphore, mpsc};

use crate::targets::{Endpoint, Probe};

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Started(usize),
    /// `None` = timeout / unreachable.
    Sample(usize, Option<f64>),
}

#[derive(Clone, Copy)]
pub struct Settings {
    pub interval: Duration,
    pub timeout: Duration,
    /// Stop after this many samples per endpoint (None = run forever).
    pub count: Option<usize>,
}

/// Most probes allowed in flight at once. Probing ~100 targets simultaneously
/// congests a home uplink and inflates the very latency we're measuring.
const MAX_IN_FLIGHT: usize = 12;

/// Which endpoints should currently be probed (the TUI flips these per tab).
pub type Active = Arc<[AtomicBool]>;

pub fn active_set(len: usize, on: impl Fn(usize) -> bool) -> Active {
    (0..len).map(|i| AtomicBool::new(on(i))).collect()
}

pub fn spawn_all(
    endpoints: &[Endpoint],
    used: &[bool],
    active: Active,
    settings: Settings,
    tx: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    // Fail fast if ICMP sockets aren't available at all.
    Client::new(&Config::default())?;
    let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let mut handles = Vec::new();
    for (id, ep) in endpoints.iter().enumerate().filter(|(i, _)| used[*i]) {
        let (tx, ep, active, permits) = (tx.clone(), ep.clone(), active.clone(), permits.clone());
        // Stagger starts so probes don't fire in the same millisecond.
        let offset = settings.interval.mul_f64((id as f64 * 0.137) % 1.0);
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(offset).await;
            run(id, ep, settings, active, permits, tx).await;
        }));
    }
    Ok(handles)
}

async fn run(
    id: usize,
    ep: Endpoint,
    s: Settings,
    active: Active,
    permits: Arc<Semaphore>,
    tx: mpsc::UnboundedSender<Event>,
) {
    let mut prober: Option<Prober> = None;
    let mut ticker = tokio::time::interval(s.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seq: u16 = 0;
    loop {
        ticker.tick().await;
        if tx.is_closed() {
            return;
        }
        if !active[id].load(Ordering::Relaxed) {
            continue;
        }
        let p = match &mut prober {
            Some(p) => p,
            None => {
                let Ok(_permit) = permits.acquire().await else {
                    return;
                };
                match Prober::new(&ep, s.timeout).await {
                    Some(p) => prober.insert(p),
                    None => {
                        // Unresolvable: report losses so the UI shows it as unreachable.
                        for _ in 0..s.count.unwrap_or(1) {
                            let _ = tx.send(Event::Sample(id, None));
                        }
                        return;
                    }
                }
            }
        };
        let Ok(permit) = permits.acquire().await else {
            return;
        };
        let _ = tx.send(Event::Started(id));
        let rtt = p.probe(seq).await;
        drop(permit);
        if tx.send(Event::Sample(id, rtt)).is_err() {
            return;
        }
        seq = seq.wrapping_add(1);
        if s.count.is_some_and(|c| seq as usize >= c) {
            return;
        }
    }
}

enum Prober {
    /// The client owns the socket's receive task, so it must outlive the pinger.
    Icmp {
        _client: Client,
        pinger: surge_ping::Pinger,
    },
    Tcp(SocketAddr, Duration),
    Http(reqwest::Client, String),
}

impl Prober {
    async fn new(ep: &Endpoint, timeout: Duration) -> Option<Self> {
        Some(match &ep.probe {
            Probe::Icmp(_) => {
                let ip: IpAddr = ep.addr?;
                // One socket per target: unprivileged (DGRAM) ICMP sockets let the kernel
                // rewrite the echo identifier, so a shared socket mis-matches replies.
                let client = Client::new(&Config::default()).ok()?;
                let mut p = client.pinger(ip, PingIdentifier(rand_u16())).await;
                p.timeout(timeout);
                Prober::Icmp {
                    _client: client,
                    pinger: p,
                }
            }
            Probe::Tcp(_, port) => Prober::Tcp(SocketAddr::new(ep.addr?, *port), timeout),
            Probe::Http(url) => {
                let client = reqwest::Client::builder()
                    .timeout(timeout)
                    .pool_max_idle_per_host(1)
                    .http1_only()
                    .build()
                    .ok()?;
                // Warm the connection so samples measure request RTT, not TCP+TLS setup.
                let _ = client
                    .get(url)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status());
                Prober::Http(client, url.clone())
            }
        })
    }

    async fn probe(&mut self, seq: u16) -> Option<f64> {
        match self {
            Prober::Icmp { pinger, .. } => {
                let (_, rtt) = pinger.ping(PingSequence(seq), &[0u8; 16]).await.ok()?;
                Some(ms(rtt))
            }
            Prober::Tcp(addr, timeout) => {
                let t = Instant::now();
                let conn = tokio::time::timeout(*timeout, tokio::net::TcpStream::connect(*addr));
                conn.await.ok()?.ok()?;
                Some(ms(t.elapsed()))
            }
            Prober::Http(client, url) => {
                let t = Instant::now();
                let resp = client.get(url.as_str()).send().await.ok()?;
                let rtt = t.elapsed();
                // Drain the body so the connection returns to the pool for reuse.
                let _ = resp.bytes().await;
                Some(ms(rtt))
            }
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn rand_u16() -> u16 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(Instant::now()) as u16
}

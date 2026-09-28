//! Expands game target specs into concrete, deduplicated probe endpoints.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::config::Game;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Probe {
    Icmp(String),
    Tcp(String, u16),
    Http(String),
}

/// A unique thing we measure. Several games can share one endpoint.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub probe: Probe,
    pub label: String,
    pub direct: bool,
    /// Resolved address, filled in before probing.
    pub addr: Option<IpAddr>,
}

/// A row in a game's view: which endpoint and how to label it there.
#[derive(Debug, Clone)]
pub struct GameView {
    pub id: String,
    pub name: String,
    pub rows: Vec<usize>,
}

pub struct Plan {
    pub endpoints: Vec<Endpoint>,
    pub games: Vec<GameView>,
    pub warnings: Vec<String>,
}

const AWS_REGIONS: &[(&str, &str)] = &[
    ("ap-south-1", "Mumbai"),
    ("ap-south-2", "Hyderabad"),
    ("ap-southeast-1", "Singapore"),
    ("ap-southeast-2", "Sydney"),
    ("ap-southeast-3", "Jakarta"),
    ("ap-east-1", "Hong Kong"),
    ("ap-northeast-1", "Tokyo"),
    ("ap-northeast-2", "Seoul"),
    ("ap-northeast-3", "Osaka"),
    ("me-south-1", "Bahrain"),
    ("me-central-1", "UAE"),
    ("il-central-1", "Tel Aviv"),
    ("af-south-1", "Cape Town"),
    ("eu-central-1", "Frankfurt"),
    ("eu-central-2", "Zurich"),
    ("eu-west-1", "Ireland"),
    ("eu-west-2", "London"),
    ("eu-west-3", "Paris"),
    ("eu-north-1", "Stockholm"),
    ("eu-south-1", "Milan"),
    ("us-east-1", "N. Virginia"),
    ("us-east-2", "Ohio"),
    ("us-west-1", "N. California"),
    ("us-west-2", "Oregon"),
    ("ca-central-1", "Montreal"),
    ("sa-east-1", "São Paulo"),
];

#[derive(Deserialize)]
struct GcpRegion {
    #[serde(rename = "RegionName")]
    region_name: String,
    #[serde(rename = "URL")]
    url: String,
}

#[derive(Deserialize)]
struct SdrConfig {
    pops: HashMap<String, SdrPop>,
}

#[derive(Deserialize)]
struct SdrPop {
    desc: String,
    #[serde(default)]
    relays: Vec<SdrRelay>,
}

#[derive(Deserialize)]
struct SdrRelay {
    ipv4: String,
}

#[derive(Default)]
struct Remote {
    gcp: Option<HashMap<String, GcpRegion>>,
    valve: Option<Vec<(String, String, String)>>, // (pop, desc, ip)
}

impl Remote {
    async fn fetch(games: &[Game], http: &reqwest::Client, warnings: &mut Vec<String>) -> Self {
        let wants = |prefix: &str| {
            games
                .iter()
                .flat_map(|g| &g.targets)
                .any(|t| t.src.starts_with(prefix))
        };
        let mut r = Remote::default();

        let gcp = async {
            if !wants("gcp:") {
                return None;
            }
            match fetch_json::<HashMap<String, GcpRegion>>(http, "https://gcping.com/api/endpoints")
                .await
            {
                Ok(m) => Some(Ok(m)),
                Err(e) => Some(Err(format!("gcping endpoint list unavailable: {e}"))),
            }
        };
        let valve = async {
            if !wants("valve:") {
                return None;
            }
            let url = "https://api.steampowered.com/ISteamApps/GetSDRConfig/v1/?appid=730";
            match fetch_json::<SdrConfig>(http, url).await {
                Ok(cfg) => {
                    let mut pops: Vec<_> = cfg
                        .pops
                        .into_iter()
                        .filter_map(|(k, p)| p.relays.first().map(|r| (k, p.desc, r.ipv4.clone())))
                        .collect();
                    pops.sort();
                    Some(Ok(pops))
                }
                Err(e) => Some(Err(format!("Valve SDR relay list unavailable: {e}"))),
            }
        };
        let (g, v) = tokio::join!(gcp, valve);
        match g {
            Some(Ok(m)) => r.gcp = Some(m),
            Some(Err(w)) => warnings.push(w),
            None => {}
        }
        match v {
            Some(Ok(p)) => r.valve = Some(p),
            Some(Err(w)) => warnings.push(w),
            None => {}
        }
        r
    }
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(http: &reqwest::Client, url: &str) -> Result<T> {
    Ok(http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

pub async fn build(games: &[Game], http: &reqwest::Client) -> Result<Plan> {
    let mut warnings = Vec::new();
    let remote = Remote::fetch(games, http, &mut warnings).await;

    let mut endpoints: Vec<Endpoint> = Vec::new();
    let mut index: HashMap<Probe, usize> = HashMap::new();
    let mut add = |probe: Probe, label: String, direct: bool| -> usize {
        *index.entry(probe.clone()).or_insert_with(|| {
            endpoints.push(Endpoint {
                probe,
                label,
                direct,
                addr: None,
            });
            endpoints.len() - 1
        })
    };

    let mut views = Vec::new();
    for g in games {
        let mut rows = Vec::new();
        for t in &g.targets {
            let (kind, rest) = t.src.split_once(':').unwrap_or((t.src.as_str(), ""));
            match kind {
                "icmp" => {
                    let label = t.region.clone().unwrap_or_else(|| rest.to_string());
                    rows.push(add(Probe::Icmp(rest.into()), label, t.direct));
                }
                "tcp" => {
                    let Some((host, port)) = rest.rsplit_once(':') else {
                        bail!("{}: tcp source needs host:port, got {rest}", g.id);
                    };
                    let label = t.region.clone().unwrap_or_else(|| host.to_string());
                    rows.push(add(Probe::Tcp(host.into(), port.parse()?), label, t.direct));
                }
                "aws" => {
                    let city = AWS_REGIONS
                        .iter()
                        .find(|(r, _)| *r == rest)
                        .map(|(_, c)| *c)
                        .unwrap_or(rest);
                    let label = t.region.clone().unwrap_or_else(|| format!("{city} · AWS"));
                    let host = format!("dynamodb.{rest}.amazonaws.com");
                    rows.push(add(Probe::Tcp(host, 443), label, t.direct));
                }
                "gcp" => {
                    let Some(map) = &remote.gcp else { continue };
                    let Some(reg) = map.get(rest) else {
                        warnings.push(format!("{}: unknown GCP region {rest}", g.id));
                        continue;
                    };
                    let label = t
                        .region
                        .clone()
                        .unwrap_or_else(|| format!("{} · GCP", reg.region_name));
                    let url = format!("{}/api/ping", reg.url.trim_end_matches('/'));
                    rows.push(add(Probe::Http(url), label, t.direct));
                }
                "valve" => {
                    let Some(pops) = &remote.valve else { continue };
                    // `*` skips China-only POPs (Perfect World); `all` includes them.
                    let pick = |p: &str, d: &str| match rest {
                        "all" => true,
                        "*" => !d.contains("(China)"),
                        one => p == one,
                    };
                    for (pop, desc, ip) in pops.iter().filter(|(p, d, _)| pick(p, d)) {
                        // "Mumbai (India)" -> "Mumbai"; keep the POP code for disambiguation.
                        let city = desc.split(" (").next().unwrap_or(desc);
                        let label = format!("{city} [{pop}]");
                        rows.push(add(Probe::Icmp(ip.clone()), label, t.direct));
                    }
                }
                other => bail!("{}: unknown source kind '{other}' in '{}'", g.id, t.src),
            }
        }
        views.push(GameView {
            id: g.id.clone(),
            name: g.name.clone(),
            rows,
        });
    }

    // Resolve hostnames up front so probes measure the network, not DNS.
    let lookups = endpoints.iter().map(|e| async move {
        let host = match &e.probe {
            Probe::Icmp(h) | Probe::Tcp(h, _) => h.clone(),
            Probe::Http(_) => return None, // reqwest handles it
        };
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Some(ip);
        }
        tokio::net::lookup_host((host.as_str(), 0))
            .await
            .ok()?
            .map(|a: SocketAddr| a.ip())
            .find(IpAddr::is_ipv4)
    });
    let addrs = futures::future::join_all(lookups).await;
    for (e, a) in endpoints.iter_mut().zip(addrs) {
        e.addr = a;
    }

    Ok(Plan {
        endpoints,
        games: views,
        warnings,
    })
}

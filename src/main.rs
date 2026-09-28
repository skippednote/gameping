mod config;
mod probe;
mod stats;
mod targets;
mod tui;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};
use tokio::sync::mpsc;

use crate::probe::{Event, Settings};
use crate::stats::Stats;
use crate::targets::Plan;

/// Latency to game servers and the cloud regions games run in.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Game ids to check (default: all). See --list.
    games: Vec<String>,

    /// Only regions whose label contains one of these (comma-separated, case-insensitive).
    #[arg(short, long, value_delimiter = ',')]
    region: Vec<String>,

    /// Samples per region for a one-shot report (disables the TUI).
    #[arg(short = 'n', long)]
    count: Option<usize>,

    /// Print a one-shot report as JSON (implies -n 10 unless set).
    #[arg(long)]
    json: bool,

    /// Milliseconds between probes to the same region.
    #[arg(short, long, default_value_t = 1000)]
    interval: u64,

    /// Probe timeout in milliseconds.
    #[arg(short, long, default_value_t = 2000)]
    timeout: u64,

    /// Extra games file (default: ~/.config/gameping/games.toml).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// List known games and exit.
    #[arg(long)]
    list: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut games = config::load(cli.config.clone())?;

    if cli.list {
        for g in &games {
            println!("{:<10} {}", g.id, g.name);
        }
        return Ok(());
    }

    if !cli.games.is_empty() {
        let wanted: Vec<String> = cli.games.iter().map(|g| g.to_lowercase()).collect();
        for w in &wanted {
            if !games.iter().any(|g| &g.id == w) {
                bail!("unknown game '{w}' (try --list)");
            }
        }
        games.retain(|g| wanted.contains(&g.id));
        games.sort_by_key(|g| wanted.iter().position(|w| *w == g.id));
    }

    let interactive = cli.count.is_none() && !cli.json && std::io::stdout().is_terminal();
    let spinner = Spinner::start(
        !interactive || std::io::stderr().is_terminal(),
        "resolving targets",
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut plan = targets::build(&games, &http).await?;
    spinner.stop();

    if !cli.region.is_empty() {
        let needles: Vec<String> = cli.region.iter().map(|r| r.to_lowercase()).collect();
        for g in &mut plan.games {
            g.rows.retain(|&i| {
                let l = plan.endpoints[i].label.to_lowercase();
                needles.iter().any(|n| l.contains(n))
            });
        }
        plan.games.retain(|g| !g.rows.is_empty());
        if plan.games.is_empty() {
            bail!("no regions match {:?}", cli.region);
        }
    }

    let used: Vec<bool> = (0..plan.endpoints.len())
        .map(|i| plan.games.iter().any(|g| g.rows.contains(&i)))
        .collect();

    let settings = Settings {
        interval: Duration::from_millis(cli.interval),
        timeout: Duration::from_millis(cli.timeout),
        count: if interactive {
            None
        } else {
            Some(cli.count.unwrap_or(10))
        },
    };
    let (tx, rx) = mpsc::unbounded_channel();
    // The TUI probes only the visible game; one-shot reports probe everything.
    let first_game = plan.games[0].rows.clone();
    let active = probe::active_set(plan.endpoints.len(), |i| {
        if interactive {
            first_game.contains(&i)
        } else {
            used[i]
        }
    });
    let _probes = probe::spawn_all(&plan.endpoints, &used, active.clone(), settings, tx)?;

    if interactive {
        tui::run(plan, rx, active, settings.interval).await
    } else {
        report(plan, used, rx, settings.count.unwrap(), cli.json).await
    }
}

async fn report(
    plan: Plan,
    used: Vec<bool>,
    mut rx: mpsc::UnboundedReceiver<Event>,
    count: usize,
    json: bool,
) -> Result<()> {
    let mut stats = vec![Stats::default(); plan.endpoints.len()];
    let total = used.iter().filter(|u| **u).count() * count;
    let mut done = 0usize;
    let spinner = Spinner::start(std::io::stderr().is_terminal(), "probing");
    while done < total {
        match rx.recv().await {
            Some(Event::Sample(i, v)) => {
                stats[i].push(v);
                done += 1;
                spinner.set(format!("probing {done}/{total} samples"));
            }
            Some(Event::Started(_)) => {}
            None => break,
        }
    }
    spinner.stop();

    for w in &plan.warnings {
        eprintln!("warning: {w}");
    }

    if json {
        let out: Vec<_> = plan
            .games
            .iter()
            .map(|g| {
                let regions: Vec<_> = g
                    .rows
                    .iter()
                    .map(|&i| {
                        let e = &plan.endpoints[i];
                        serde_json::json!({
                            "region": e.label,
                            "direct": e.direct,
                            "stats": stats[i].summary(),
                            "loss_pct": stats[i].loss_pct(),
                        })
                    })
                    .collect();
                serde_json::json!({ "id": g.id, "name": g.name, "regions": regions })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    for g in &plan.games {
        let mut rows = g.rows.clone();
        rows.sort_by(|&a, &b| sort_key(&stats[a]).total_cmp(&sort_key(&stats[b])));
        let mut t = Table::new();
        t.load_style(UTF8_FULL_CONDENSED).set_header(vec![
            "Region", "Type", "Avg", "Min", "Max", "P95", "Jitter", "Loss",
        ]);
        for i in rows {
            let e = &plan.endpoints[i];
            let kind = if e.direct { "direct" } else { "estimate" };
            match stats[i].summary() {
                Some(s) => t.add_row(vec![
                    Cell::new(&e.label),
                    Cell::new(kind),
                    Cell::new(format!("{:.0} ms", s.avg)).fg(term_color(s.avg)),
                    Cell::new(format!("{:.0}", s.min)),
                    Cell::new(format!("{:.0}", s.max)),
                    Cell::new(format!("{:.0}", s.p95)),
                    Cell::new(format!("±{:.1}", s.jitter)),
                    Cell::new(format!("{:.0}%", s.loss_pct)).fg(if s.loss_pct > 0.0 {
                        Color::Red
                    } else {
                        Color::Reset
                    }),
                ]),
                None => t.add_row(vec![
                    Cell::new(&e.label),
                    Cell::new(kind),
                    Cell::new("unreachable / blocks ping").fg(Color::DarkGrey),
                ]),
            };
        }
        println!("\n{}", g.name);
        println!("{t}");
    }
    Ok(())
}

/// Replies first (by avg), then unreachable.
pub fn sort_key(s: &Stats) -> f64 {
    s.summary().map_or(f64::INFINITY, |x| x.avg)
}

fn term_color(ms: f64) -> Color {
    match ms {
        m if m < 50.0 => Color::Green,
        m if m < 80.0 => Color::DarkGreen,
        m if m < 120.0 => Color::Yellow,
        m if m < 160.0 => Color::DarkYellow,
        _ => Color::Red,
    }
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Stderr spinner for the non-TUI paths.
struct Spinner {
    msg: Option<std::sync::Arc<std::sync::Mutex<String>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Spinner {
    fn start(enabled: bool, msg: &str) -> Self {
        if !enabled {
            return Spinner {
                msg: None,
                handle: None,
            };
        }
        let text = std::sync::Arc::new(std::sync::Mutex::new(msg.to_string()));
        let t = text.clone();
        let handle = tokio::spawn(async move {
            let mut i = 0;
            loop {
                let m = t.lock().unwrap().clone();
                eprint!("\r\x1b[2K{} {m}…", SPINNER[i % SPINNER.len()]);
                let _ = std::io::stderr().flush();
                i += 1;
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        });
        Spinner {
            msg: Some(text),
            handle: Some(handle),
        }
    }

    fn set(&self, msg: String) {
        if let Some(m) = &self.msg {
            *m.lock().unwrap() = msg;
        }
    }

    fn stop(self) {
        if let Some(h) = self.handle {
            h.abort();
            eprint!("\r\x1b[2K");
        }
    }
}

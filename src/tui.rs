use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Bar, BarChart, Block, BorderType, Cell, Chart, Dataset, GraphType, Paragraph, Row,
        Table, TableState, Tabs,
    },
};
use tokio::sync::mpsc;

use crate::probe::{Active, Event};
use crate::stats::Stats;
use crate::targets::Plan;
use crate::{SPINNER, sort_key};

const TREND_WIDTH: usize = 20;
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Latency,
    Name,
}

struct App {
    plan: Plan,
    stats: Vec<Stats>,
    game: usize,
    table: TableState,
    sort: Sort,
    paused: bool,
    frame: usize,
    started: Instant,
    active: Active,
    interval: Duration,
}

pub async fn run(
    plan: Plan,
    mut rx: mpsc::UnboundedReceiver<Event>,
    active: Active,
    interval: Duration,
) -> Result<()> {
    let mut app = App {
        stats: vec![Stats::default(); plan.endpoints.len()],
        plan,
        game: 0,
        table: TableState::default().with_selected(Some(0)),
        sort: Sort::Latency,
        paused: false,
        frame: 0,
        started: Instant::now(),
        active,
        interval,
    };

    let mut terminal = ratatui::init();
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));

    let result: Result<()> = loop {
        if let Err(e) = terminal.draw(|f| app.draw(f)) {
            break Err(e.into());
        }
        tokio::select! {
            _ = tick.tick() => app.frame = app.frame.wrapping_add(1),
            Some(ev) = rx.recv() => {
                app.apply(ev);
                while let Ok(ev) = rx.try_recv() {
                    app.apply(ev);
                }
            }
            Some(Ok(ev)) = keys.next() => {
                if app.on_key(ev) {
                    break Ok(());
                }
            }
        }
    };

    ratatui::restore();
    result
}

impl App {
    fn apply(&mut self, ev: Event) {
        match ev {
            Event::Started(i) => self.stats[i].in_flight = true,
            Event::Sample(i, v) => {
                if self.paused {
                    self.stats[i].in_flight = false;
                } else {
                    self.stats[i].push(v);
                }
            }
        }
    }

    /// Returns true to quit.
    fn on_key(&mut self, ev: TermEvent) -> bool {
        let TermEvent::Key(k) = ev else { return false };
        if k.kind != KeyEventKind::Press {
            return false;
        }
        let n_games = self.plan.games.len();
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                self.set_game((self.game + 1) % n_games)
            }
            KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => {
                self.set_game((self.game + n_games - 1) % n_games)
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Home | KeyCode::Char('g') => self.table.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => {
                self.table.select(Some(self.rows().len().saturating_sub(1)))
            }
            KeyCode::Char('s') => {
                self.sort = if self.sort == Sort::Latency {
                    Sort::Name
                } else {
                    Sort::Latency
                }
            }
            KeyCode::Char('p') | KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Char('r') => {
                self.stats.iter_mut().for_each(Stats::reset);
                self.started = Instant::now();
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n_games {
                    self.set_game(i);
                }
            }
            _ => {}
        }
        false
    }

    /// Switch tabs and move probing to the new game's endpoints.
    fn set_game(&mut self, g: usize) {
        self.game = g;
        self.table.select(Some(0));
        let rows = &self.plan.games[g].rows;
        for (i, flag) in self.active.iter().enumerate() {
            flag.store(rows.contains(&i), Ordering::Relaxed);
        }
        for (i, s) in self.stats.iter_mut().enumerate() {
            if !rows.contains(&i) {
                s.in_flight = false;
            }
        }
    }

    fn move_sel(&mut self, d: isize) {
        let len = self.rows().len() as isize;
        if len == 0 {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as isize;
        self.table.select(Some((cur + d).rem_euclid(len) as usize));
    }

    /// Endpoint ids for the current game, in display order.
    fn rows(&self) -> Vec<usize> {
        let mut rows = self.plan.games[self.game].rows.clone();
        match self.sort {
            Sort::Latency => rows.sort_by(|&a, &b| {
                // Pending (no samples yet) sorts after unreachable.
                let key = |i: usize| {
                    let s = &self.stats[i];
                    if s.has_data() { sort_key(s) } else { f64::MAX }
                };
                key(a).total_cmp(&key(b))
            }),
            Sort::Name => rows.sort_by(|&a, &b| {
                self.plan.endpoints[a]
                    .label
                    .cmp(&self.plan.endpoints[b].label)
            }),
        }
        rows
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.frame % SPINNER.len()]
    }

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_header(f, header);

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
        let [chart, bars] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(right);

        let rows = self.rows();
        let sel = self
            .table
            .selected()
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.draw_table(f, left, &rows);
        if let Some(&ep) = rows.get(sel) {
            self.draw_chart(f, chart, ep);
        }
        self.draw_bars(f, bars, &rows);
        self.draw_footer(f, footer);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let titles = self
            .plan
            .games
            .iter()
            .enumerate()
            .map(|(i, g)| format!("{} {}", i + 1, g.name));
        let active = self.stats.iter().filter(|s| s.in_flight).count();
        let status = if self.paused {
            Span::styled(" ⏸ paused ", Style::new().fg(Color::Yellow).bold())
        } else if active > 0 {
            Span::styled(
                format!(" {} probing {active} ", self.spinner()),
                Style::new().fg(Color::Cyan),
            )
        } else {
            Span::styled(" ● idle ", Style::new().fg(Color::DarkGray))
        };
        let elapsed = self.started.elapsed().as_secs();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(Line::from(vec![" gameping ".bold().cyan(), status]))
            .title(
                Line::from(format!(" {:02}:{:02} ", elapsed / 60, elapsed % 60))
                    .right_aligned()
                    .dark_gray(),
            );
        let tabs = Tabs::new(titles)
            .select(self.game)
            .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).bold())
            .divider(Span::raw("│").dark_gray())
            .block(block);
        f.render_widget(tabs, area);
    }

    fn draw_table(&mut self, f: &mut Frame, area: Rect, rows: &[usize]) {
        let header = Row::new([
            "", "Region", "", "Last", "Avg", "P95", "Jitter", "Loss", "Trend",
        ])
        .style(Style::new().bold().fg(Color::Gray))
        .bottom_margin(0);

        let body = rows.iter().map(|&i| {
            let e = &self.plan.endpoints[i];
            let s = &self.stats[i];
            let status = if !s.has_data() || s.in_flight {
                Span::styled(self.spinner(), Style::new().fg(Color::Cyan))
            } else if s.summary().is_none() {
                Span::styled("✕", Style::new().fg(Color::Red))
            } else {
                Span::styled("●", Style::new().fg(quality(s.summary().unwrap().avg)))
            };
            let kind = if e.direct {
                Span::styled("◆", Style::new().fg(Color::Magenta))
            } else {
                Span::styled("◇", Style::new().fg(Color::DarkGray))
            };
            let cells: Vec<Cell> = match s.summary() {
                Some(x) => vec![
                    Cell::from(status),
                    Cell::from(e.label.clone()),
                    Cell::from(kind),
                    ms_cell(x.last),
                    ms_cell(Some(x.avg)).add_modifier(Modifier::BOLD),
                    ms_cell(Some(x.p95)),
                    Cell::from(format!("±{:.0}", x.jitter)).fg(if x.jitter > 15.0 {
                        Color::Yellow
                    } else {
                        Color::Gray
                    }),
                    loss_cell(x.loss_pct),
                    Cell::from(trend(s)),
                ],
                None if s.has_data() => vec![
                    Cell::from(status),
                    Cell::from(e.label.clone()),
                    Cell::from(kind),
                    Cell::from("—").dark_gray(),
                    Cell::from("unreachable").red(),
                    Cell::from(""),
                    Cell::from(""),
                    loss_cell(100.0),
                    Cell::from(trend(s)),
                ],
                None => vec![
                    Cell::from(status),
                    Cell::from(e.label.clone()),
                    Cell::from(kind),
                    Cell::from("…").dark_gray(),
                ],
            };
            Row::new(cells)
        });

        let widths = [
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(6),
            Constraint::Length(11),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(TREND_WIDTH as u16),
        ];
        let game = &self.plan.games[self.game];
        let sort = if self.sort == Sort::Latency {
            "latency"
        } else {
            "name"
        };
        let table = Table::new(body, widths)
            .header(header)
            .column_spacing(1)
            .row_highlight_style(
                Style::new()
                    .bg(Color::Rgb(40, 44, 52))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌")
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(format!(" {} · {} regions ", game.name, rows.len()).bold())
                    .title(
                        Line::from(format!(" sort: {sort} "))
                            .right_aligned()
                            .dark_gray(),
                    ),
            );
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_chart(&self, f: &mut Frame, area: Rect, ep: usize) {
        let e = &self.plan.endpoints[ep];
        let s = &self.stats[ep];
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {} ", e.label).bold())
            .title(
                Line::from(if e.direct {
                    " direct "
                } else {
                    " estimate · cloud region "
                })
                .right_aligned()
                .fg(if e.direct {
                    Color::Magenta
                } else {
                    Color::DarkGray
                }),
            );

        let Some(sum) = s.summary() else {
            let msg = if s.has_data() {
                Line::from(" no replies — host unreachable or blocks probes ").red()
            } else {
                Line::from(format!(" {} waiting for first sample… ", self.spinner())).cyan()
            };
            f.render_widget(Paragraph::new(msg).block(block), area);
            return;
        };

        // Only the last N samples that fit the plot width look good in braille.
        let width = (area.width.saturating_sub(10) as usize * 2).max(10);
        let hist: Vec<Option<f64>> = s.history.iter().rev().take(width).rev().copied().collect();
        let n = hist.len().max(2) as f64;
        let line: Vec<(f64, f64)> = hist
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (i as f64, v)))
            .collect();
        let peak = line.iter().map(|p| p.1).fold(sum.avg, f64::max);
        let top = nice_ceiling(peak * 1.15);
        let lost: Vec<(f64, f64)> = hist
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_none())
            .map(|(i, _)| (i as f64, top * 0.98))
            .collect();
        let avg_line = vec![(0.0, sum.avg), (n - 1.0, sum.avg)];

        let datasets = vec![
            Dataset::default()
                .name(format!("avg {:.0}", sum.avg))
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(Color::DarkGray))
                .data(&avg_line),
            Dataset::default()
                .name("rtt")
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(quality(sum.avg)))
                .data(&line),
            Dataset::default()
                .name("lost")
                .marker(Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(Style::new().fg(Color::Red))
                .data(&lost),
        ];
        let chart = Chart::new(datasets)
            .block(block)
            .x_axis(
                Axis::default()
                    .bounds([0.0, n - 1.0])
                    .style(Style::new().dark_gray())
                    .labels([
                        format!("-{:.0}s", hist.len() as f64 * self.interval.as_secs_f64()),
                        "now".into(),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, top])
                    .style(Style::new().dark_gray())
                    .labels([
                        "0".to_string(),
                        format!("{:.0}", top / 2.0),
                        format!("{top:.0}ms"),
                    ]),
            )
            .legend_position(Some(ratatui::widgets::LegendPosition::TopRight));
        f.render_widget(chart, area);
    }

    fn draw_bars(&self, f: &mut Frame, area: Rect, rows: &[usize]) {
        let mut ranked: Vec<(usize, f64)> = rows
            .iter()
            .filter_map(|&i| self.stats[i].summary().map(|s| (i, s.avg)))
            .collect();
        ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
        let fit = area.height.saturating_sub(2) as usize;
        let label_w = 16;
        let bars: Vec<Bar> = ranked
            .iter()
            .take(fit)
            .map(|&(i, avg)| {
                let label: String = self.plan.endpoints[i].label.chars().take(label_w).collect();
                Bar::new(avg.round() as u64)
                    .label(Line::from(format!("{label:<label_w$}")))
                    .text_value(format!("{avg:.0}"))
                    .style(Style::new().fg(quality(avg)))
                    .value_style(Style::new().fg(Color::Black).bg(quality(avg)).bold())
            })
            .collect();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Best regions · avg ms ".bold());
        if bars.is_empty() {
            let msg = Line::from(format!(" {} collecting samples… ", self.spinner())).cyan();
            f.render_widget(Paragraph::new(msg).block(block), area);
            return;
        }
        let chart = BarChart::horizontal(bars)
            .bar_width(1)
            .bar_gap(0)
            .block(block);
        f.render_widget(chart, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Cyan).bold());
        let txt = |t: &'static str| Span::styled(t, Style::new().fg(Color::DarkGray));
        let mut spans = vec![
            key(" ←/→"),
            txt(" game  "),
            key("↑/↓"),
            txt(" region  "),
            key("s"),
            txt(" sort  "),
            key("p"),
            txt(" pause  "),
            key("r"),
            txt(" reset  "),
            key("q"),
            txt(" quit   "),
            Span::styled("◆", Style::new().fg(Color::Magenta)),
            txt(" direct  "),
            Span::styled("◇", Style::new().fg(Color::DarkGray)),
            txt(" estimate"),
        ];
        if let Some(w) = self.plan.warnings.first() {
            spans.push(Span::styled(
                format!("   ⚠ {w}"),
                Style::new().fg(Color::Yellow),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn quality(ms: f64) -> Color {
    match ms {
        m if m < 50.0 => Color::Green,
        m if m < 80.0 => Color::LightGreen,
        m if m < 120.0 => Color::Yellow,
        m if m < 160.0 => Color::LightRed,
        _ => Color::Red,
    }
}

fn ms_cell(v: Option<f64>) -> Cell<'static> {
    match v {
        Some(v) => Cell::from(format!("{v:.0}")).fg(quality(v)),
        None => Cell::from("✕").red(),
    }
}

fn loss_cell(pct: f64) -> Cell<'static> {
    let c = match pct {
        0.0 => Color::DarkGray,
        p if p < 5.0 => Color::Yellow,
        _ => Color::Red,
    };
    Cell::from(format!("{pct:.0}%")).fg(c)
}

/// Inline sparkline of recent samples, coloured per sample; lost packets as a red ✕.
fn trend(s: &Stats) -> Line<'static> {
    let recent: Vec<Option<f64>> = s
        .history
        .iter()
        .rev()
        .take(TREND_WIDTH)
        .rev()
        .copied()
        .collect();
    let ok = recent.iter().flatten();
    let lo = ok.clone().copied().fold(f64::INFINITY, f64::min);
    let hi = ok.copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (hi - lo).max(10.0); // don't amplify 1 ms wobble into full-height bars
    Line::from(
        recent
            .into_iter()
            .map(|v| match v {
                Some(v) => {
                    let idx = (((v - lo) / span) * 7.0).round().clamp(0.0, 7.0) as usize;
                    Span::styled(BLOCKS[idx].to_string(), Style::new().fg(quality(v)))
                }
                None => Span::styled("✕", Style::new().fg(Color::Red)),
            })
            .collect::<Vec<_>>(),
    )
}

fn nice_ceiling(v: f64) -> f64 {
    let step = if v <= 100.0 {
        25.0
    } else if v <= 300.0 {
        50.0
    } else {
        100.0
    };
    ((v / step).ceil() * step).max(step)
}

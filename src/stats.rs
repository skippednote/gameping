use std::collections::VecDeque;

/// How many samples to keep for charts and rolling stats.
pub const HISTORY: usize = 300;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Most recent last. `None` = lost.
    pub history: VecDeque<Option<f64>>,
    pub sent: u64,
    pub lost: u64,
    pub in_flight: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Summary {
    pub last: Option<f64>,
    pub min: f64,
    pub avg: f64,
    pub max: f64,
    pub p95: f64,
    pub jitter: f64,
    pub loss_pct: f64,
    pub samples: u64,
}

impl Stats {
    pub fn push(&mut self, v: Option<f64>) {
        self.in_flight = false;
        self.sent += 1;
        if v.is_none() {
            self.lost += 1;
        }
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(v);
    }

    pub fn reset(&mut self) {
        *self = Stats {
            in_flight: self.in_flight,
            ..Default::default()
        };
    }

    pub fn has_data(&self) -> bool {
        self.sent > 0
    }

    pub fn ok_values(&self) -> impl Iterator<Item = f64> + '_ {
        self.history.iter().flatten().copied()
    }

    /// `None` until at least one reply has come back.
    pub fn summary(&self) -> Option<Summary> {
        let mut v: Vec<f64> = self.ok_values().collect();
        if v.is_empty() {
            return None;
        }
        let n = v.len() as f64;
        let avg = v.iter().sum::<f64>() / n;
        // Jitter as mean absolute difference between consecutive replies (RFC 3550 style).
        let jitter = if v.len() > 1 {
            v.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        v.sort_by(|a, b| a.total_cmp(b));
        let p95 = v[((v.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)];
        Some(Summary {
            last: self.history.back().copied().flatten(),
            min: v[0],
            avg,
            max: *v.last().unwrap(),
            p95,
            jitter,
            loss_pct: self.lost as f64 * 100.0 / self.sent.max(1) as f64,
            samples: self.sent,
        })
    }

    pub fn loss_pct(&self) -> f64 {
        self.lost as f64 * 100.0 / self.sent.max(1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_basics() {
        let mut s = Stats::default();
        for v in [Some(10.0), Some(20.0), None, Some(30.0)] {
            s.push(v);
        }
        let x = s.summary().unwrap();
        assert_eq!(x.min, 10.0);
        assert_eq!(x.max, 30.0);
        assert_eq!(x.avg, 20.0);
        assert_eq!(x.jitter, 10.0);
        assert_eq!(x.loss_pct, 25.0);
        assert_eq!(x.last, Some(30.0));
    }

    #[test]
    fn all_lost_has_no_summary() {
        let mut s = Stats::default();
        s.push(None);
        assert!(s.summary().is_none());
        assert_eq!(s.loss_pct(), 100.0);
    }

    #[test]
    fn history_is_bounded() {
        let mut s = Stats::default();
        for i in 0..(HISTORY + 50) {
            s.push(Some(i as f64));
        }
        assert_eq!(s.history.len(), HISTORY);
        assert_eq!(s.sent as usize, HISTORY + 50);
    }
}

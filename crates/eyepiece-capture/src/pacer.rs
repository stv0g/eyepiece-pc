//! Frame-rate decimation on top of a display's refresh events: e.g. a 120 Hz laptop panel
//! captured at 100 fps takes 5 of every 6 vblanks, evenly spread, long-term exact.

pub struct Pacer {
    period_us: u64,
    slack_us: u64,
    next_due: u64,
}

impl Pacer {
    /// `fps`: target rate; `refresh_hz`: rate of the events `due()` is called at
    /// (None = unknown/timer).
    pub fn new(fps: f64, refresh_hz: Option<f64>) -> Pacer {
        let period_us = (1e6 / fps.max(1.0)) as u64;
        let ev_us = refresh_hz.map(|r| (1e6 / r) as u64).unwrap_or(period_us);
        Pacer { period_us, slack_us: ev_us.min(period_us) / 2, next_due: 0 }
    }
    pub fn period_us(&self) -> u64 {
        self.period_us
    }
    /// Would a frame at `t_us` be on time?
    pub fn due(&self, t_us: u64) -> bool {
        t_us + self.slack_us >= self.next_due
    }
    /// A frame was taken at `t_us`.
    pub fn taken(&mut self, t_us: u64) {
        let n = self.next_due + self.period_us;
        self.next_due = if n + self.period_us < t_us { t_us + self.period_us } else { n };
    }
    /// When the next frame is due (timer mode sleeps until then).
    pub fn next_due(&self) -> u64 {
        self.next_due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimate_120_to_100() {
        let mut p = Pacer::new(100.0, Some(120.0));
        let mut n = 0;
        for i in 0..1200u64 {
            let t = 1_000_000 + i * 1_000_000 / 120;
            if p.due(t) {
                p.taken(t);
                n += 1;
            }
        }
        assert!((995..=1005).contains(&n), "{n}");
    }
    #[test]
    fn every_vblank_when_fps_ge_refresh() {
        let mut p = Pacer::new(100.0, Some(60.0));
        let mut n = 0;
        for i in 0..600u64 {
            let t = 5_000_000 + i * 16_667;
            if p.due(t) {
                p.taken(t);
                n += 1;
            }
        }
        assert_eq!(n, 600);
    }
}

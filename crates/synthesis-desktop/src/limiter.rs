use std::time::{Duration, Instant};

use crate::error::{DesktopError, Result};

/// Paces actions (`per_sec`) and caps them for the session (`max`).
#[derive(Debug)]
pub struct Limiter {
    gap: Duration,
    max: u32,
    used: u32,
    next: Option<Instant>,
}

impl Limiter {
    pub fn new(per_sec: f64, max: u32) -> Self {
        let gap = if per_sec > 0.0 {
            Duration::from_secs_f64(1.0 / per_sec)
        } else {
            Duration::ZERO
        };
        Self {
            gap,
            max,
            used: 0,
            next: None,
        }
    }

    /// Counts one action; returns how long the caller must wait first. The cap is hard.
    pub fn take(&mut self, now: Instant) -> Result<Duration> {
        if self.used >= self.max {
            return Err(DesktopError::RateLimited(format!(
                "this session already used its {} desktop actions; start a new session for more (synt.desktop.max_actions)",
                self.max
            )));
        }
        self.used += 1;
        let at = self.next.map_or(now, |n| n.max(now));
        self.next = Some(at + self.gap);
        Ok(at.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paces_and_caps() {
        let mut l = Limiter::new(2.0, 3);
        let t0 = Instant::now();
        assert_eq!(l.take(t0).unwrap(), Duration::ZERO);
        assert_eq!(l.take(t0).unwrap(), Duration::from_millis(500));
        assert_eq!(l.take(t0).unwrap(), Duration::from_millis(1000));
        assert_eq!(l.take(t0).unwrap_err().kind(), "rate_limited");
        // after a pause the pace resets
        let mut l = Limiter::new(2.0, 10);
        l.take(t0).unwrap();
        assert_eq!(l.take(t0 + Duration::from_secs(5)).unwrap(), Duration::ZERO);
    }
}

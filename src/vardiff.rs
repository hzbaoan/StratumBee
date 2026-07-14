use std::collections::VecDeque;

use chrono::{DateTime, Utc};

const EMA_ALPHA: f64 = 0.25;
const HYSTERESIS: f64 = 1.5;
const OFFLINE_SECS: f64 = 120.0;
const CACHE_SIZE: usize = 30;

#[derive(Debug, Clone)]
pub struct VardiffController {
    target_share_time: f64,
    retarget_time: f64,
    min_diff: f64,
    max_diff: f64,
    last_retarget: DateTime<Utc>,
    session_start: DateTime<Utc>,
    ema_rate: f64,
    ema_seeded: bool,
    samples: VecDeque<(DateTime<Utc>, f64)>,
}

impl VardiffController {
    pub fn new(target_share_time: f64, retarget_time: f64, min_diff: f64, max_diff: f64) -> Self {
        let now = Utc::now();
        let min_diff = min_diff.max(f64::MIN_POSITIVE);
        Self {
            target_share_time: target_share_time.max(1.0),
            retarget_time: retarget_time.max(1.0),
            min_diff,
            max_diff: max_diff.max(min_diff),
            last_retarget: now,
            session_start: now,
            ema_rate: 0.0,
            ema_seeded: false,
            samples: VecDeque::with_capacity(CACHE_SIZE),
        }
    }

    pub fn record_share(&mut self, now: DateTime<Utc>, difficulty: f64) {
        self.samples.push_back((now, difficulty));
        if self.samples.len() > CACHE_SIZE {
            self.samples.pop_front();
        }
    }

    pub fn maybe_retarget(&mut self, current_diff: f64, now: DateTime<Utc>) -> Option<f64> {
        let since_ms = (now - self.last_retarget).num_milliseconds();
        if since_ms < (self.retarget_time * 1000.0) as i64 {
            return None;
        }
        self.last_retarget = now;

        if self.samples.is_empty() {
            let age_secs = (now - self.session_start).num_seconds() as f64;
            if age_secs > OFFLINE_SECS {
                let stepped = self.nearest_p2((current_diff / 6.0).max(self.min_diff));
                if (stepped - current_diff).abs() > f64::EPSILON {
                    return Some(stepped);
                }
            }
            return None;
        }

        let sum_diff = self.samples.iter().map(|(_, d)| *d).sum::<f64>();
        let span_secs = (self.samples.back().unwrap().0 - self.samples.front().unwrap().0)
            .num_milliseconds() as f64
            / 1000.0;
        if span_secs <= 0.0 {
            return None;
        }

        let instant_rate = sum_diff / span_secs;
        if self.ema_seeded {
            self.ema_rate = EMA_ALPHA * instant_rate + (1.0 - EMA_ALPHA) * self.ema_rate;
        } else {
            self.ema_rate = instant_rate;
            self.ema_seeded = true;
        }

        let raw_target = self.ema_rate * self.target_share_time;
        let target_diff = self.nearest_p2(raw_target.clamp(self.min_diff, self.max_diff));

        let should_up = target_diff >= current_diff * HYSTERESIS;
        let should_down = target_diff <= current_diff / HYSTERESIS;

        if should_up || should_down {
            Some(target_diff)
        } else {
            None
        }
    }

    pub fn nearest_p2(&self, value: f64) -> f64 {
        if !value.is_finite() || value <= 0.0 {
            return self.min_diff;
        }
        let clamped = value.clamp(self.min_diff, self.max_diff);
        let exponent = clamped.log2();
        let lower = 2f64.powf(exponent.floor());
        let upper = 2f64.powf(exponent.ceil());
        let chosen = if (clamped - lower) <= (upper - clamped) {
            lower
        } else {
            upper
        };
        chosen.clamp(self.min_diff, self.max_diff)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::VardiffController;

    fn controller() -> VardiffController {
        VardiffController::new(10.0, 30.0, 512.0, 33_554_432.0)
    }

    #[test]
    fn retargets_up_for_fast_miner() {
        let mut vd = controller();
        let mut t = chrono::Utc::now();
        for _ in 0..25 {
            t += Duration::seconds(1);
            vd.record_share(t, 16_384.0);
        }
        let r = vd.maybe_retarget(16_384.0, t + Duration::seconds(31));
        assert!(r.is_some());
        assert!(r.unwrap() > 16_384.0);
    }

    #[test]
    fn rescues_offline_miner() {
        let mut vd = controller();
        let r = vd.maybe_retarget(16_384.0, chrono::Utc::now() + Duration::seconds(200));
        assert_eq!(r, Some(2048.0));
    }

    #[test]
    fn nearest_power_of_two_supports_sub_one_difficulties() {
        let vd = VardiffController::new(10.0, 30.0, 0.125, 8.0);
        assert_eq!(vd.nearest_p2(0.2), 0.25);
        assert_eq!(vd.nearest_p2(0.6), 0.5);
    }
}

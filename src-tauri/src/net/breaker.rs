//! 熔断器。
//!
//! 识别到挑战（验证码 / 429 / 503 / 空壳页）之后**不能重试**。重试会把 IP、
//! 账号、整个代理池一起拖下水，而且成功率不会因为重试次数增加而提高。正确做法
//! 是停下来、冷却、然后降级到别的获取层级。
//!
//! 冷却结束自动半开：放一个探测请求，成功就恢复，失败立刻重新熔断。

use std::sync::{Arc, Mutex};

use crate::net::clock::Clock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// 正常放行。
    Closed,
    /// 熔断中，拒绝一切请求。
    Open,
    /// 冷却已结束，只放一个探测请求。
    HalfOpen,
}

impl BreakerState {
    pub fn label(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half-open",
        }
    }
}

struct Inner {
    state: BreakerState,
    failures: u32,
    opened_at: f64,
    probe_in_flight: bool,
}

/// 熔断器。`threshold` 为连续失败次数阈值，`cooldown_secs` 为冷却时长。
pub struct CircuitBreaker {
    inner: Mutex<Inner>,
    threshold: u32,
    cooldown_secs: f64,
    clock: Arc<dyn Clock>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown_secs: f64, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: BreakerState::Closed,
                failures: 0,
                opened_at: 0.0,
                probe_in_flight: false,
            }),
            threshold: threshold.max(1),
            cooldown_secs: cooldown_secs.max(0.0),
            clock,
        }
    }

    /// 是否允许发起请求。冷却结束后的第一次调用会转成半开并占用探测名额。
    pub fn allow(&self) -> bool {
        let mut inner = self.lock();
        let now = self.clock.now();
        match inner.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                if now >= inner.opened_at + self.cooldown_secs {
                    inner.state = BreakerState::HalfOpen;
                    inner.probe_in_flight = true;
                    true
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => {
                if inner.probe_in_flight {
                    false
                } else {
                    inner.probe_in_flight = true;
                    true
                }
            }
        }
    }

    pub fn record_success(&self) {
        let mut inner = self.lock();
        inner.state = BreakerState::Closed;
        inner.failures = 0;
        inner.probe_in_flight = false;
    }

    /// 归还半开探测名额，但**不改变状态也不计失败**。
    ///
    /// 用于「请求根本没发出去」的情况，典型是合规闸门拒绝 —— 那不是站点故障，
    /// 不该计入熔断，也不该把探测名额永久占住。
    pub fn release_probe(&self) {
        let mut inner = self.lock();
        inner.probe_in_flight = false;
    }

    pub fn record_failure(&self) {
        let mut inner = self.lock();
        inner.probe_in_flight = false;
        match inner.state {
            // 探测失败：立刻重新熔断，并重新开始计时。
            BreakerState::HalfOpen => {
                inner.state = BreakerState::Open;
                inner.opened_at = self.clock.now();
            }
            _ => {
                inner.failures += 1;
                if inner.failures >= self.threshold {
                    inner.state = BreakerState::Open;
                    inner.opened_at = self.clock.now();
                }
            }
        }
    }

    /// 只读观测。冷却已结束时报告 `HalfOpen`，但不占用探测名额。
    pub fn state(&self) -> BreakerState {
        let inner = self.lock();
        match inner.state {
            BreakerState::Open if self.clock.now() >= inner.opened_at + self.cooldown_secs => {
                BreakerState::HalfOpen
            }
            other => other,
        }
    }

    /// 距离下一次放行还需等待的秒数；已可放行时为 0。
    pub fn retry_after_secs(&self) -> f64 {
        let inner = self.lock();
        if inner.state != BreakerState::Open {
            return 0.0;
        }
        (inner.opened_at + self.cooldown_secs - self.clock.now()).max(0.0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::clock::ManualClock;

    fn breaker(threshold: u32, cooldown: f64) -> (CircuitBreaker, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        (
            CircuitBreaker::new(threshold, cooldown, clock.clone()),
            clock,
        )
    }

    #[test]
    fn breaker_opens_after_threshold_and_recovers() {
        let (breaker, clock) = breaker(3, 300.0);
        assert_eq!(breaker.state(), BreakerState::Closed);

        for _ in 0..3 {
            assert!(breaker.allow());
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), BreakerState::Open);
        assert!(!breaker.allow(), "熔断后应拒绝请求");
        assert_eq!(breaker.retry_after_secs(), 300.0);

        clock.advance(299.0);
        assert!(!breaker.allow(), "冷却未结束不应放行");

        clock.advance(2.0);
        assert_eq!(breaker.state(), BreakerState::HalfOpen);
        assert!(breaker.allow(), "冷却结束后应半开并放行一个探测");
        assert!(!breaker.allow(), "半开时只放一个探测，其余仍拒绝");

        breaker.record_success();
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert!(breaker.allow());
    }

    #[test]
    fn half_open_failure_reopens_with_fresh_cooldown() {
        let (breaker, clock) = breaker(1, 100.0);
        assert!(breaker.allow());
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Open);

        clock.advance(101.0);
        assert!(breaker.allow(), "应放行探测");
        breaker.record_failure();

        assert_eq!(breaker.state(), BreakerState::Open);
        assert!(!breaker.allow(), "探测失败应立刻重新熔断");
        clock.advance(101.0);
        assert!(breaker.allow(), "新的冷却结束后才能再探测");
    }

    #[test]
    fn success_resets_failure_count() {
        let (breaker, _clock) = breaker(3, 10.0);
        assert!(breaker.allow());
        breaker.record_failure();
        assert!(breaker.allow());
        breaker.record_failure();
        assert!(breaker.allow());
        breaker.record_success();

        assert!(breaker.allow());
        breaker.record_failure();
        assert!(breaker.allow());
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Closed, "成功应清零失败计数");
    }

    #[test]
    fn below_threshold_stays_closed() {
        let (breaker, _clock) = breaker(5, 10.0);
        for _ in 0..4 {
            assert!(breaker.allow());
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert_eq!(breaker.retry_after_secs(), 0.0);
    }

    /// 合规拒绝不是站点故障：既不计数，也不该把半开探测名额永久占住。
    #[test]
    fn release_probe_returns_the_slot_without_counting_a_failure() {
        let (breaker, clock) = breaker(2, 50.0);
        assert!(breaker.allow());
        breaker.record_failure();
        assert!(breaker.allow());
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Open);

        clock.advance(51.0);
        assert!(breaker.allow(), "应放行探测");
        breaker.release_probe();

        assert_eq!(breaker.state(), BreakerState::HalfOpen);
        assert!(breaker.allow(), "归还名额后应还能再放一个探测");
        breaker.record_success();
        assert_eq!(breaker.state(), BreakerState::Closed);
    }
}

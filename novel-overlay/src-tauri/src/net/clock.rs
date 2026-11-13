//! 可注入的单调时钟。
//!
//! 限速与熔断都依赖时间。如果时间源写死成 `Instant::now()`，测试要么真睡眠
//! （拖慢 CI 且不稳定），要么只能断言「函数被调用过」——那等于没测。
//!
//! 注入之后可以断言真实行为：推进 0.25 秒 → 只补到半个令牌；推进到冷却结束 →
//! 熔断器自动半开。

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 单调递增的秒数。起点任意，只保证不回退。
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> f64;
}

/// 生产环境时钟。
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }
}

/// 测试时钟：只有被显式推进时才前进。
pub struct ManualClock {
    now: Mutex<f64>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            now: Mutex::new(0.0),
        }
    }

    pub fn advance(&self, seconds: f64) {
        *self.lock() += seconds;
    }

    pub fn set(&self, seconds: f64) {
        *self.lock() = seconds;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, f64> {
        // 时钟内部只有 f64，不存在「中毒后状态不一致」的问题，直接取回。
        self.now.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> f64 {
        *self.lock()
    }
}

/// 生产环境的默认时钟。
pub fn system() -> Arc<dyn Clock> {
    Arc::new(SystemClock::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_only_moves_when_advanced() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), 0.0);
        clock.advance(1.5);
        assert_eq!(clock.now(), 1.5);
        clock.set(10.0);
        assert_eq!(clock.now(), 10.0);
    }

    #[test]
    fn system_clock_does_not_go_backwards() {
        let clock = SystemClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
        assert!(first >= 0.0);
    }
}

//! 异步令牌桶。
//!
//! 按固定速率补充令牌，`acquire` 在令牌不足时让出执行权等待。
//!
//! - `rate_per_sec`：每秒补充的令牌数，即长期平均 QPS 上限
//! - `burst`：桶容量，决定瞬时突发请求数
//!
//! 时间源可注入（见 [`crate::net::clock`]），因此测试可以零睡眠断言真实行为：
//! 推进 0.25 秒只补到半个令牌，推进到 `Crawl-delay` 下界才放行。
//!
//! 内部用 `std::sync::Mutex` 而不是 `tokio::sync::Mutex`：临界区里只有纯计算，
//! 从不跨 `await` 持锁，用同步锁更简单也更快。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::net::clock::{self, Clock};

#[derive(Clone)]
pub struct TokenBucket {
    inner: Arc<Mutex<Bucket>>,
    /// `f64` 的位模式。放在锁外是为了让 `apply_crawl_delay` 不必是 async。
    rate_bits: Arc<AtomicU64>,
    clock: Arc<dyn Clock>,
}

struct Bucket {
    capacity: f64,
    tokens: f64,
    last_refill: f64,
}

impl TokenBucket {
    pub fn new(rate_per_sec: f64, burst: u32) -> Self {
        Self::with_clock(rate_per_sec, burst, clock::system())
    }

    pub fn with_clock(rate_per_sec: f64, burst: u32, clock: Arc<dyn Clock>) -> Self {
        let capacity = burst.max(1) as f64;
        let rate = rate_per_sec.max(0.01);
        let now = clock.now();
        Self {
            inner: Arc::new(Mutex::new(Bucket {
                capacity,
                tokens: capacity,
                last_refill: now,
            })),
            rate_bits: Arc::new(AtomicU64::new(rate.to_bits())),
            clock,
        }
    }

    /// 当前速率（每秒令牌数）。
    pub fn rate_per_sec(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Relaxed))
    }

    /// 把站点的 `Crawl-delay` 作为限速下界。
    ///
    /// 实际速率取「配置速率」与「`1 / crawl_delay`」的较小者 —— 也就是**只能变慢，
    /// 不能变快**。站点声明的抓取间隔不允许被调小。
    ///
    /// 返回是否真的生效（更慢）。
    pub fn apply_crawl_delay(&self, crawl_delay_secs: f64) -> bool {
        if !crawl_delay_secs.is_finite() || crawl_delay_secs <= 0.0 {
            return false;
        }
        let ceiling = 1.0 / crawl_delay_secs;
        let current = self.rate_per_sec();
        if ceiling < current {
            self.rate_bits
                .store(ceiling.to_bits(), Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// 立刻尝试取一个令牌，不等待。测试与「探测」场景使用。
    pub fn try_acquire(&self) -> bool {
        self.reserve().is_none()
    }

    /// 下一次可取到令牌需要等待的时长；当前有令牌时为 0。不会消费令牌。
    pub fn wait_for(&self) -> Duration {
        let mut bucket = self.lock();
        self.refill(&mut bucket);
        if bucket.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - bucket.tokens) / self.rate_per_sec())
        }
    }

    /// 取一个令牌，必要时等待。
    pub async fn acquire(&self) {
        loop {
            let Some(wait) = self.reserve() else {
                return;
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// 成功消费一个令牌返回 `None`；令牌不足返回需要等待的时长。
    ///
    /// 锁只在这个同步函数内部持有，绝不会跨越 `await`，因此 `acquire` 的 future
    /// 仍然是 `Send`。
    fn reserve(&self) -> Option<Duration> {
        let mut bucket = self.lock();
        self.refill(&mut bucket);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            Some(Duration::from_secs_f64(
                (1.0 - bucket.tokens) / self.rate_per_sec(),
            ))
        }
    }

    fn refill(&self, bucket: &mut Bucket) {
        let now = self.clock.now();
        let elapsed = (now - bucket.last_refill).max(0.0);
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_sec()).min(bucket.capacity);
        bucket.last_refill = now;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::clock::ManualClock;

    fn bucket(rate: f64, burst: u32) -> (TokenBucket, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        (TokenBucket::with_clock(rate, burst, clock.clone()), clock)
    }

    #[test]
    fn burst_is_consumed_without_waiting() {
        let (bucket, _clock) = bucket(1.0, 3);
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire(), "突发额度用尽后应立即拒绝");
        assert_eq!(bucket.wait_for(), Duration::from_secs_f64(1.0));
    }

    #[test]
    fn tokens_refill_over_time() {
        let (bucket, clock) = bucket(2.0, 1);
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());

        clock.advance(0.25);
        assert!(!bucket.try_acquire(), "0.25s × 2/s 只够半个令牌");

        clock.advance(0.25);
        assert!(bucket.try_acquire(), "累计 0.5s 应补满一个令牌");
    }

    #[test]
    fn tokens_never_exceed_burst_capacity() {
        let (bucket, clock) = bucket(1.0, 2);
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        clock.advance(100.0);
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire(), "长时间空闲也不该超过突发容量");
    }

    #[test]
    fn crawl_delay_can_only_slow_things_down() {
        let (bucket, _clock) = bucket(10.0, 1);
        assert_eq!(bucket.rate_per_sec(), 10.0);

        // Crawl-delay: 2s → 上限 0.5 QPS，比配置的 10 QPS 慢，应当生效。
        assert!(bucket.apply_crawl_delay(2.0));
        assert!((bucket.rate_per_sec() - 0.5).abs() < 1e-9);

        // Crawl-delay: 0.01s → 100 QPS，比当前快，不允许把限速调快。
        assert!(!bucket.apply_crawl_delay(0.01));
        assert!((bucket.rate_per_sec() - 0.5).abs() < 1e-9, "限速只能变慢");

        // 非法值一律忽略。
        assert!(!bucket.apply_crawl_delay(0.0));
        assert!(!bucket.apply_crawl_delay(-1.0));
        assert!(!bucket.apply_crawl_delay(f64::NAN));
    }

    #[tokio::test]
    async fn acquire_returns_immediately_when_tokens_available() {
        let (bucket, _clock) = bucket(1.0, 3);
        let started = std::time::Instant::now();
        for _ in 0..3 {
            bucket.acquire().await;
        }
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}

//! 出口代理池。
//!
//! # 定位：分散负载 + 单出口故障转移，**不是**封禁规避
//!
//! 这一层在 [`crate::net::Ladder`] **之下**，这个位置是刻意的，也是整个设计
//! 成立的前提：
//!
//! - **robots 闸门在它之上** —— 换出口不会让被禁止的路径变得可抓。
//!   `Ladder` 拒绝的 URL 根本走不到这里，一个请求都不会发出去。
//! - **熔断器在它之上** —— 站点已识别到挑战（验证码 / 429 / 503）而熔断时，
//!   换出口也不会继续打。熔断是**按站点**计的，不是按出口计的。
//! - **限速在它之上（且本层内不重置）** —— 换出口不会重置配额。
//!   令牌桶在 [`crate::net::limiter`] 里，与出口无关。
//!
//! 换句话说：**代理池解决的问题是「某个出口挂了」，不是「站点不让我抓」。**
//! 后者正确的反应是降级到下一个获取层级（见 [`crate::net::Ladder`]），
//! 而不是扩大出口池 —— 那只是把封禁从一个 IP 扩散到很多个 IP。
//!
//! # 全部出口冷却中时**不回落直连**
//!
//! 宁可失败也不静默走直连：用户配了代理就是不想暴露真实出口，悄悄回落会把
//! 真实 IP 漏出去，而且这种泄漏不会有任何提示。

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::net::clock::Clock;

/// 出口代理列表，逗号分隔。
///
/// 用逗号而不是冒号：代理 URL 自己就含 `:`（`http://1.2.3.4:8080`），
/// 冒号分隔会把它切碎。
pub const PROXIES_ENV: &str = "NOVEL_OVERLAY_PROXIES";

/// 轮换策略：`round-robin`（默认）/ `sticky` / `failover`。
pub const ROTATION_ENV: &str = "NOVEL_OVERLAY_PROXY_ROTATION";

/// 关闭本机代理自动探测（仍尊重 [`PROXIES_ENV`]）。
pub const DISABLE_LOCAL_PROXY_ENV: &str = "NOVEL_OVERLAY_DISABLE_LOCAL_PROXY";

/// 本机常见代理口（Clash Verge mixed / Clash / Surge 等）。
///
/// 笔趣阁正文接口按出口 IP 限流：直连被封时返回诱饵假文，换出口即可恢复。
/// 未配置 [`PROXIES_ENV`] 时探测这些端口，命中则走本地代理。
pub const LOCAL_PROXY_CANDIDATES: &[&str] = &[
    "http://127.0.0.1:7897",  // Clash Verge mixed-port
    "http://127.0.0.1:7890",  // Clash / ClashX
    "http://127.0.0.1:7899",  // Clash Verge http
    "socks5://127.0.0.1:7898", // Clash Verge socks
    "http://127.0.0.1:1087",
    "socks5://127.0.0.1:1080",
    "http://127.0.0.1:6152", // Surge
];

/// 单个出口连续失败多少次进入冷却。
const DEFAULT_FAILURE_THRESHOLD: u32 = 3;

/// 出口冷却时长（秒）。
const DEFAULT_COOLDOWN_SECS: f64 = 60.0;

/// 一个出口代理，归一化成 `scheme://host:port`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProxySpec {
    normalized: String,
    host: String,
}

impl ProxySpec {
    /// 解析并归一化。只接受明确的代理协议，不接受裸 `host:port` ——
    /// 少写协议前缀在真实场景里几乎总是笔误，静默按 HTTP 处理会掩盖问题。
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let parsed = url::Url::parse(trimmed).ok()?;
        let scheme = parsed.scheme();
        if !matches!(scheme, "http" | "https" | "socks5" | "socks5h" | "socks4") {
            return None;
        }
        let host = parsed.host_str()?.to_string();
        let port = parsed.port().unwrap_or_else(|| default_port(scheme));
        Some(Self {
            normalized: format!("{scheme}://{host}:{port}"),
            host,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.normalized
    }

    /// 出口自身的 host（不是目标站点）。
    pub fn host(&self) -> &str {
        &self.host
    }
}

fn default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        "socks4" | "socks5" | "socks5h" => 1080,
        _ => 80,
    }
}

fn local_proxy_discovery_disabled() -> bool {
    matches!(
        std::env::var(DISABLE_LOCAL_PROXY_ENV)
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// 探测本机已在监听的代理，按 [`LOCAL_PROXY_CANDIDATES`] 顺序去重返回。
pub fn discover_local_proxies() -> Vec<ProxySpec> {
    if local_proxy_discovery_disabled() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in LOCAL_PROXY_CANDIDATES {
        let Some(spec) = ProxySpec::parse(raw) else {
            continue;
        };
        if !seen.insert(spec.clone()) {
            continue;
        }
        if tcp_open(spec.host(), port_of(&spec)) {
            found.push(spec);
        }
    }
    found
}

fn port_of(spec: &ProxySpec) -> u16 {
    url::Url::parse(spec.as_str())
        .ok()
        .and_then(|parsed| parsed.port_or_known_default())
        .unwrap_or(80)
}

fn tcp_open(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&SocketAddr::from(addr), Duration::from_millis(200)).is_ok()
}

/// 轮换策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// 依次轮换，每次请求换下一个可用出口。
    RoundRobin,
    /// 同一目标站点粘住同一个出口：减少同站点的连接抖动，也避免同一次
    /// 会话里出口跳来跳去触发风控。
    StickyPerHost,
    /// 顺序故障转移：永远优先用第一个可用出口，它进冷却了才用下一个。
    Failover,
}

impl Rotation {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "round-robin" | "roundrobin" | "rr" => Some(Rotation::RoundRobin),
            "sticky" | "sticky-per-host" => Some(Rotation::StickyPerHost),
            "failover" | "ordered" => Some(Rotation::Failover),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Rotation::RoundRobin => "依次轮换",
            Rotation::StickyPerHost => "按站点粘住",
            Rotation::Failover => "顺序故障转移",
        }
    }
}

struct Entry {
    spec: ProxySpec,
    /// 连续失败次数。成功时清零；进入冷却后**不清零** —— 反复失效的出口
    /// 再失败一次就该重新冷却，不该重新攒满阈值。
    consecutive_failures: u32,
    /// 冷却到期时刻（时钟口径，秒）。0 表示不在冷却。
    cooldown_until: f64,
    successes: u64,
    failures: u64,
}

/// 出口代理池。
pub struct ProxyPool {
    entries: Mutex<Vec<Entry>>,
    cursor: AtomicUsize,
    /// `StickyPerHost` 用：目标站点 host → 出口下标。
    sticky: Mutex<HashMap<String, usize>>,
    rotation: Rotation,
    failure_threshold: u32,
    cooldown_secs: f64,
    clock: Arc<dyn Clock>,
}

impl ProxyPool {
    pub fn new(specs: Vec<ProxySpec>, rotation: Rotation, clock: Arc<dyn Clock>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let entries = specs
            .into_iter()
            .filter(|spec| seen.insert(spec.clone()))
            .map(|spec| Entry {
                spec,
                consecutive_failures: 0,
                cooldown_until: 0.0,
                successes: 0,
                failures: 0,
            })
            .collect();
        Self {
            entries: Mutex::new(entries),
            cursor: AtomicUsize::new(0),
            sticky: Mutex::new(HashMap::new()),
            rotation,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            clock,
        }
    }

    /// 从环境变量装配。没有任何合法出口时返回 `None`（等于没配代理）。
    pub fn from_env(clock: Arc<dyn Clock>) -> Option<Self> {
        let raw = std::env::var(PROXIES_ENV).ok()?;
        let specs: Vec<ProxySpec> = raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(ProxySpec::parse)
            .collect();
        if specs.is_empty() {
            return None;
        }
        let rotation = std::env::var(ROTATION_ENV)
            .ok()
            .and_then(|value| Rotation::parse(&value))
            .unwrap_or(Rotation::RoundRobin);
        Some(Self::new(specs, rotation, clock))
    }

    /// 环境变量优先；否则探测本机 Clash 等常见端口。
    ///
    /// 本机探测用顺序故障转移（优先 Verge 7897）。显式 [`PROXIES_ENV`] 仍按
    /// [`ROTATION_ENV`]（默认轮换）。
    pub fn from_env_or_local(clock: Arc<dyn Clock>) -> Option<Self> {
        if let Some(pool) = Self::from_env(clock.clone()) {
            return Some(pool);
        }
        if local_proxy_discovery_disabled() {
            return None;
        }
        let specs = discover_local_proxies();
        if specs.is_empty() {
            return None;
        }
        Some(Self::new(specs, Rotation::Failover, clock))
    }

    pub fn policy(mut self, failure_threshold: u32, cooldown_secs: f64) -> Self {
        self.failure_threshold = failure_threshold.max(1);
        self.cooldown_secs = cooldown_secs.max(0.0);
        self
    }

    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    pub fn len(&self) -> usize {
        self.lock_entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 当前不在冷却中的出口数量。
    pub fn healthy_count(&self) -> usize {
        let now = self.clock.now();
        self.lock_entries()
            .iter()
            .filter(|entry| entry.cooldown_until <= now)
            .count()
    }

    /// 为一个目标站点选一个出口。
    ///
    /// 返回 `None` 表示**当前没有可用出口**（池为空，或全部在冷却中）。
    /// 调用方应当失败，而不是回落直连 —— 理由见模块文档。
    pub fn acquire(&self, target_host: &str) -> Option<ProxySpec> {
        let now = self.clock.now();
        let entries = self.lock_entries();
        if entries.is_empty() {
            return None;
        }
        let healthy: Vec<usize> = (0..entries.len())
            .filter(|index| entries[*index].cooldown_until <= now)
            .collect();
        if healthy.is_empty() {
            return None;
        }

        let chosen = match self.rotation {
            Rotation::Failover => healthy[0],
            Rotation::RoundRobin => {
                let start = self.cursor.fetch_add(1, Ordering::Relaxed) % entries.len();
                (0..entries.len())
                    .map(|offset| (start + offset) % entries.len())
                    .find(|index| healthy.contains(index))
                    .expect("healthy 非空，必然能找到")
            }
            Rotation::StickyPerHost => {
                let mut sticky = self.lock_sticky();
                match sticky.get(target_host) {
                    // 粘住的出口还健康就继续用。
                    Some(index) if healthy.contains(index) => *index,
                    // 否则重新挑一个，并改粘到新的上。
                    _ => {
                        let index =
                            healthy[self.cursor.fetch_add(1, Ordering::Relaxed) % healthy.len()];
                        sticky.insert(target_host.to_string(), index);
                        index
                    }
                }
            }
        };

        Some(entries[chosen].spec.clone())
    }

    pub fn record_success(&self, spec: &ProxySpec) {
        let mut entries = self.lock_entries();
        if let Some(entry) = entries.iter_mut().find(|entry| &entry.spec == spec) {
            entry.consecutive_failures = 0;
            entry.cooldown_until = 0.0;
            entry.successes += 1;
        }
    }

    pub fn record_failure(&self, spec: &ProxySpec) {
        let now = self.clock.now();
        let mut entries = self.lock_entries();
        if let Some(entry) = entries.iter_mut().find(|entry| &entry.spec == spec) {
            entry.failures += 1;
            entry.consecutive_failures += 1;
            if entry.consecutive_failures >= self.failure_threshold {
                entry.cooldown_until = now + self.cooldown_secs;
            }
        }
    }

    /// 观测用：某个出口连续失败几次、是否在冷却。
    pub fn stats(&self, spec: &ProxySpec) -> Option<(u64, u64, bool)> {
        let now = self.clock.now();
        self.lock_entries()
            .iter()
            .find(|entry| &entry.spec == spec)
            .map(|entry| (entry.successes, entry.failures, entry.cooldown_until > now))
    }

    pub fn describe(&self) -> String {
        let now = self.clock.now();
        let entries = self.lock_entries();
        let healthy = entries
            .iter()
            .filter(|entry| entry.cooldown_until <= now)
            .count();
        format!(
            "出口代理池 {} 个（{} 个可用），轮换策略：{}",
            entries.len(),
            healthy,
            self.rotation.label()
        )
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_sticky(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.sticky
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::clock::ManualClock;

    fn spec(raw: &str) -> ProxySpec {
        ProxySpec::parse(raw).unwrap_or_else(|| panic!("应能解析: {raw}"))
    }

    fn pool(specs: &[&str], rotation: Rotation) -> (Arc<ProxyPool>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let parsed = specs.iter().map(|raw| spec(raw)).collect();
        let pool = Arc::new(ProxyPool::new(parsed, rotation, clock.clone()).policy(3, 60.0));
        (pool, clock)
    }

    #[test]
    fn parses_supported_schemes_and_normalizes() {
        assert_eq!(spec("http://1.2.3.4:8080").as_str(), "http://1.2.3.4:8080");
        assert_eq!(spec("  http://1.2.3.4:8080  ").as_str(), "http://1.2.3.4:8080");
        assert_eq!(spec("socks5://5.6.7.8:1080").as_str(), "socks5://5.6.7.8:1080");
        // 省略端口时补默认值，便于去重与客户端缓存。
        assert_eq!(spec("http://1.2.3.4").as_str(), "http://1.2.3.4:80");
        assert_eq!(spec("https://proxy.example.com").as_str(), "https://proxy.example.com:443");
        assert_eq!(spec("socks5://5.6.7.8").as_str(), "socks5://5.6.7.8:1080");
    }

    #[test]
    fn rejects_garbage_and_bare_host_port() {
        for bad in ["", "   ", "not a url", "1.2.3.4:8080", "ftp://1.2.3.4:21"] {
            assert!(ProxySpec::parse(bad).is_none(), "不该接受: {bad:?}");
        }
    }

    #[test]
    fn duplicates_are_collapsed() {
        let (pool, _clock) = pool(
            &["http://1.2.3.4:8080", "http://1.2.3.4:8080", "socks5://5.6.7.8:1080"],
            Rotation::RoundRobin,
        );
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn round_robin_cycles_in_order() {
        let (pool, _clock) = pool(
            &["http://a.test:1", "http://b.test:2", "http://c.test:3"],
            Rotation::RoundRobin,
        );
        let picked: Vec<String> = (0..4)
            .map(|_| pool.acquire("site.test").unwrap().as_str().to_string())
            .collect();
        assert_eq!(
            picked,
            vec![
                "http://a.test:1",
                "http://b.test:2",
                "http://c.test:3",
                "http://a.test:1",
            ]
        );
    }

    #[test]
    fn sticky_keeps_one_exit_per_host_and_isolates_hosts() {
        let (pool, _clock) = pool(
            &["http://a.test:1", "http://b.test:2"],
            Rotation::StickyPerHost,
        );
        let first = pool.acquire("one.test").unwrap();
        for _ in 0..5 {
            assert_eq!(
                pool.acquire("one.test").unwrap(),
                first,
                "同一站点应粘住同一个出口"
            );
        }
        // 另一个站点不该被第一个站点的选择绑住（应能分到另一个出口）。
        let other = pool.acquire("two.test").unwrap();
        assert_ne!(other, first, "不同站点应能分到不同出口");
    }

    #[test]
    fn sticky_falls_back_when_the_pinned_exit_cools_down() {
        let (pool, _clock) = pool(
            &["http://a.test:1", "http://b.test:2"],
            Rotation::StickyPerHost,
        );
        let pinned = pool.acquire("one.test").unwrap();
        for _ in 0..3 {
            pool.record_failure(&pinned);
        }
        let replacement = pool.acquire("one.test").unwrap();
        assert_ne!(replacement, pinned, "粘住的出口冷却后应改选别的");
        assert_eq!(pool.healthy_count(), 1);
    }

    #[test]
    fn failover_prefers_the_first_healthy_exit() {
        let (pool, _clock) = pool(
            &["http://a.test:1", "http://b.test:2"],
            Rotation::Failover,
        );
        let first = pool.acquire("site.test").unwrap();
        for _ in 0..10 {
            assert_eq!(pool.acquire("site.test").unwrap(), first, "首选健康时不该换");
        }
        for _ in 0..3 {
            pool.record_failure(&first);
        }
        assert_eq!(
            pool.acquire("site.test").unwrap().as_str(),
            "http://b.test:2",
            "首选冷却后才用下一个"
        );
    }

    #[test]
    fn failure_threshold_puts_an_exit_into_cooldown_and_it_returns_afterwards() {
        let (pool, clock) = pool(&["http://a.test:1"], Rotation::RoundRobin);
        let only = pool.acquire("site.test").unwrap();

        pool.record_failure(&only);
        pool.record_failure(&only);
        assert_eq!(pool.healthy_count(), 1, "未到阈值不该冷却");
        assert!(pool.acquire("site.test").is_some());

        pool.record_failure(&only);
        assert_eq!(pool.healthy_count(), 0, "到阈值应进入冷却");
        assert!(
            pool.acquire("site.test").is_none(),
            "唯一出口冷却中，应报告无可用出口（不回落直连）"
        );

        clock.advance(59.0);
        assert!(pool.acquire("site.test").is_none(), "冷却未结束不该放行");

        clock.advance(2.0);
        assert_eq!(pool.healthy_count(), 1, "冷却结束后应恢复");
        assert_eq!(pool.acquire("site.test").unwrap(), only);
    }

    #[test]
    fn success_clears_the_failure_streak_and_the_cooldown() {
        let (pool, _clock) = pool(&["http://a.test:1"], Rotation::RoundRobin);
        let only = pool.acquire("site.test").unwrap();
        pool.record_failure(&only);
        pool.record_failure(&only);
        pool.record_success(&only);

        pool.record_failure(&only);
        pool.record_failure(&only);
        assert_eq!(pool.healthy_count(), 1, "成功应清零连续失败计数");

        let (successes, failures, cooling) = pool.stats(&only).unwrap();
        assert_eq!((successes, failures), (1, 4));
        assert!(!cooling);
    }

    /// 出口反复失效时，冷却结束后的**第一次**失败就应重新冷却，
    /// 不该重新攒满阈值 —— 否则一个坏出口会周期性放行一批注定失败的请求。
    #[test]
    fn a_flapping_exit_cools_down_again_on_the_first_failure() {
        let (pool, clock) = pool(&["http://a.test:1"], Rotation::RoundRobin);
        let only = pool.acquire("site.test").unwrap();
        for _ in 0..3 {
            pool.record_failure(&only);
        }
        assert_eq!(pool.healthy_count(), 0);

        clock.advance(61.0);
        assert_eq!(pool.acquire("site.test").unwrap(), only, "冷却结束后放行一次");
        pool.record_failure(&only);
        assert_eq!(pool.healthy_count(), 0, "应立刻重新冷却，而不是重新数到 3");
    }

    #[test]
    fn all_exits_cooling_means_no_exit_at_all() {
        let (pool, _clock) = pool(
            &["http://a.test:1", "http://b.test:2"],
            Rotation::RoundRobin,
        );
        for raw in ["http://a.test:1", "http://b.test:2"] {
            let target = spec(raw);
            for _ in 0..3 {
                pool.record_failure(&target);
            }
        }
        assert_eq!(pool.healthy_count(), 0);
        assert!(pool.acquire("site.test").is_none());
    }

    #[test]
    fn empty_pool_reports_no_exit() {
        let (pool, _clock) = pool(&[], Rotation::RoundRobin);
        assert!(pool.is_empty());
        assert!(pool.acquire("site.test").is_none());
    }

    #[test]
    fn rotation_labels_parse() {
        assert_eq!(Rotation::parse("round-robin"), Some(Rotation::RoundRobin));
        assert_eq!(Rotation::parse(" STICKY "), Some(Rotation::StickyPerHost));
        assert_eq!(Rotation::parse("failover"), Some(Rotation::Failover));
        assert_eq!(Rotation::parse("random"), None);
    }

    #[test]
    fn describe_reports_size_and_health() {
        let (pool, _clock) = pool(&["http://a.test:1", "http://b.test:2"], Rotation::RoundRobin);
        assert_eq!(pool.describe(), "出口代理池 2 个（2 个可用），轮换策略：依次轮换");
        let target = spec("http://a.test:1");
        for _ in 0..3 {
            pool.record_failure(&target);
        }
        assert_eq!(pool.describe(), "出口代理池 2 个（1 个可用），轮换策略：依次轮换");
    }
}

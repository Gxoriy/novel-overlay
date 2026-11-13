//! 分级获取阶梯。
//!
//! 按**合规性与可靠性**排序，第一个成功的胜出：
//!
//! | 层级 | 来源 | 默认 |
//! |---|---|---|
//! | L0 | 官方 API | 未配置 → 跳过 |
//! | L1 | 站点/用户授权数据 | 未配置 → 跳过 |
//! | L2 | 公开页面 | **默认关闭** |
//! | L3 | 离线回放 | ✅ 开发测试走这里 |
//!
//! 三条硬规则：
//!
//! 1. **L0/L1 只留位置，不写假实现。** 假的 API 客户端比没有更危险 —— 它会让人
//!    以为数据来源是合规的。
//! 2. **L2 默认关闭，且不报错。** 返回「未配置：默认关闭」让阶梯继续往下走。
//! 3. **合规闸门在网络之前。** 被 robots 拒绝的 URL 不该产生任何请求。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use url::Url;

use crate::net::breaker::CircuitBreaker;
use crate::net::challenge::ChallengeProfile;
use crate::net::client::DEFAULT_USER_AGENT;
use crate::net::clock::Clock;
use crate::net::limiter::TokenBucket;
use crate::net::replay::ReplayTier;
use crate::net::robots::{self, RobotsRules};
use crate::net::transport::{AttemptStep, PageFetch, PageFuture, PageSource, TextFuture, Transport};

/// 获取层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    OfficialApi,
    AuthorizedData,
    PublicWeb,
    Replay,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::OfficialApi => "L0 官方 API",
            Tier::AuthorizedData => "L1 授权数据",
            Tier::PublicWeb => "L2 公开页面",
            Tier::Replay => "L3 离线回放",
        }
    }
}

/// 单层抓取结果。
#[derive(Debug)]
pub enum TierOutcome {
    Fetched {
        html: String,
    },
    /// 未配置或未启用。**不是错误**，阶梯继续往下走。
    Skipped {
        reason: String,
    },
    /// 合规拒绝（robots 禁止）。**不计入熔断** —— 这不是站点故障。
    Refused {
        reason: String,
    },
    /// 尝试过但失败，含识别到挑战。
    Failed {
        reason: String,
    },
}

/// 单层尝试的归类。**结构化而不是字符串** —— 诊断面板要按这个上色，
/// 靠 `contains("命中")` 猜状态迟早在改文案时静默失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptKind {
    /// 命中并返回了页面。
    Hit,
    /// 未配置或未启用，跳过。
    Skipped,
    /// 合规拒绝（robots）。不是站点故障。
    Refused,
    /// 尝试过但失败，含识别到挑战。
    Failed,
}

impl AttemptKind {
    /// 给前端用的状态字面量（`DiagnosticStep.status` 取值域）。
    pub fn status(self) -> &'static str {
        match self {
            AttemptKind::Hit => "ok",
            AttemptKind::Skipped => "info",
            AttemptKind::Refused => "warn",
            AttemptKind::Failed => "fail",
        }
    }

    fn label(self) -> &'static str {
        match self {
            AttemptKind::Hit => "命中",
            AttemptKind::Skipped => "跳过",
            AttemptKind::Refused => "合规拒绝",
            AttemptKind::Failed => "失败",
        }
    }
}

/// 某一层依次发生了什么。
#[derive(Debug, Clone)]
pub struct TierAttempt {
    pub tier: Tier,
    pub kind: AttemptKind,
    pub reason: String,
}

impl TierAttempt {
    fn new(tier: Tier, kind: AttemptKind, reason: impl Into<String>) -> Self {
        Self {
            tier,
            kind,
            reason: reason.into(),
        }
    }

    /// 人类可读的一行，用于日志与错误信息。
    pub fn describe(&self) -> String {
        let label = self.kind.label();
        if self.reason.is_empty() {
            format!("{}：{label}", self.tier.label())
        } else {
            format!("{}：{label}（{}）", self.tier.label(), self.reason)
        }
    }
}

pub type TierFuture<'a> = Pin<Box<dyn Future<Output = TierOutcome> + Send + 'a>>;

/// 阶梯中的一层。
pub trait TierSource: Send + Sync {
    fn tier(&self) -> Tier;

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        challenge_markers: &'a [String],
    ) -> TierFuture<'a>;
}

/// 一次成功的获取。
#[derive(Debug, Clone)]
pub struct Fetched {
    pub html: String,
    /// 最终是哪一层取到的。
    pub tier: Tier,
    /// 各层依次发生了什么。用于日志、错误信息与诊断面板的「为什么走了这一层」。
    pub attempts: Vec<TierAttempt>,
}

/// 阶梯本身。也实现 [`PageSource`]，所以采集内核可以直接依赖它。
pub struct Ladder {
    tiers: Vec<Box<dyn TierSource>>,
}

impl Ladder {
    pub fn new(tiers: Vec<Box<dyn TierSource>>) -> Self {
        Self { tiers }
    }

    /// 只挂回放层。**开发与测试的默认构造方式**，也是唯一不碰网络的构造方式。
    pub fn replay_only(roots: &[PathBuf]) -> Result<Self> {
        Ok(Self::new(vec![Box::new(ReplayTier::from_dirs(roots)?)]))
    }

    /// 按序尝试各层，返回第一个成功的结果。
    pub async fn fetch(
        &self,
        url: &str,
        headers: &[(String, String)],
        challenge_markers: &[String],
    ) -> Result<Fetched> {
        let mut attempts = Vec::new();

        for tier in &self.tiers {
            match tier.fetch(url, headers, challenge_markers).await {
                TierOutcome::Fetched { html } => {
                    attempts.push(TierAttempt::new(tier.tier(), AttemptKind::Hit, ""));
                    log::debug!("{url} 由 {} 取到", tier.tier().label());
                    return Ok(Fetched {
                        html,
                        tier: tier.tier(),
                        attempts,
                    });
                }
                TierOutcome::Skipped { reason } => {
                    attempts.push(TierAttempt::new(tier.tier(), AttemptKind::Skipped, reason));
                }
                TierOutcome::Refused { reason } => {
                    attempts.push(TierAttempt::new(tier.tier(), AttemptKind::Refused, reason));
                }
                TierOutcome::Failed { reason } => {
                    attempts.push(TierAttempt::new(tier.tier(), AttemptKind::Failed, reason));
                }
            }
        }

        Err(anyhow!(
            "所有获取层级都未能取到 {url}：\n  {}",
            attempts
                .iter()
                .map(TierAttempt::describe)
                .collect::<Vec<_>>()
                .join("\n  ")
        ))
    }
}

impl PageSource for Ladder {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        challenge_markers: &'a [String],
    ) -> TextFuture<'a> {
        Box::pin(async move {
            self.get_page(url, headers, challenge_markers)
                .await
                .map(|page| page.html)
        })
    }

    /// 覆写默认实现：阶梯是唯一有「层级轨迹」可报的 [`PageSource`]。
    fn get_page<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        challenge_markers: &'a [String],
    ) -> PageFuture<'a> {
        Box::pin(async move {
            let fetched = self.fetch(url, headers, challenge_markers).await?;
            Ok(PageFetch {
                html: fetched.html,
                source: fetched.tier.label().to_string(),
                attempts: fetched
                    .attempts
                    .into_iter()
                    .map(|attempt| AttemptStep {
                        source: attempt.tier.label().to_string(),
                        status: attempt.kind.status().to_string(),
                        detail: attempt.describe(),
                    })
                    .collect(),
            })
        })
    }

    fn describe(&self) -> String {
        let layers: Vec<&str> = self.tiers.iter().map(|tier| tier.tier().label()).collect();
        format!("分级阶梯 [{}]", layers.join(" → "))
    }
}

/// L0 / L1 的占位实现：**如实报告未配置，不做假实现**。
pub struct UnconfiguredTier {
    tier: Tier,
    reason: &'static str,
}

impl UnconfiguredTier {
    /// L0 官方 API。需要凭据时在这里接真实客户端。
    pub fn official_api() -> Self {
        Self {
            tier: Tier::OfficialApi,
            reason: "未配置：需要官方 API 凭据",
        }
    }

    /// L1 站点/用户授权数据源。
    pub fn authorized_data() -> Self {
        Self {
            tier: Tier::AuthorizedData,
            reason: "未配置：需要站点或用户授权数据源",
        }
    }
}

impl TierSource for UnconfiguredTier {
    fn tier(&self) -> Tier {
        self.tier
    }

    fn fetch<'a>(
        &'a self,
        _url: &'a str,
        _headers: &'a [(String, String)],
        _challenge_markers: &'a [String],
    ) -> TierFuture<'a> {
        let reason = self.reason.to_string();
        Box::pin(async move { TierOutcome::Skipped { reason } })
    }
}

/// L2 公开页面抓取配置。
///
/// [`PublicWebConfig::new`] 是**安全默认**：关闭。必须显式调用
/// [`PublicWebConfig::enabled`] 才会真的发请求 —— 默认行为不该依赖「人记得关掉」。
pub struct PublicWebConfig {
    enabled: bool,
    user_agent: String,
    rate_per_sec: f64,
    burst: u32,
    breaker_threshold: u32,
    breaker_cooldown_secs: f64,
}

impl Default for PublicWebConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicWebConfig {
    pub fn new() -> Self {
        Self {
            enabled: false,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            rate_per_sec: 1.5,
            burst: 3,
            breaker_threshold: 3,
            breaker_cooldown_secs: 300.0,
        }
    }

    /// 显式打开。只有调用方明确知道「这是用户主动发起、且目标是用户自己配置的书源」
    /// 时才应该调用。
    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }

    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    pub fn rate(mut self, rate_per_sec: f64, burst: u32) -> Self {
        self.rate_per_sec = rate_per_sec;
        self.burst = burst;
        self
    }

    pub fn breaker(mut self, threshold: u32, cooldown_secs: f64) -> Self {
        self.breaker_threshold = threshold.max(1);
        self.breaker_cooldown_secs = cooldown_secs;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// L2：公开页面抓取。合规闸门 + 限速 + 挑战检测 + 熔断都在这一层。
pub struct PublicWebTier {
    transport: Arc<dyn Transport>,
    config: PublicWebConfig,
    limiter: TokenBucket,
    breaker: Arc<CircuitBreaker>,
    /// 按 `scheme://host` 缓存的 robots 规则。
    robots: Mutex<HashMap<String, Arc<RobotsRules>>>,
}

impl PublicWebTier {
    pub fn new(transport: Arc<dyn Transport>, clock: Arc<dyn Clock>, config: PublicWebConfig) -> Self {
        let limiter = TokenBucket::with_clock(config.rate_per_sec, config.burst, clock.clone());
        let breaker = Arc::new(CircuitBreaker::new(
            config.breaker_threshold,
            config.breaker_cooldown_secs,
            clock,
        ));
        Self {
            transport,
            config,
            limiter,
            breaker,
            robots: Mutex::new(HashMap::new()),
        }
    }

    pub fn breaker(&self) -> &Arc<CircuitBreaker> {
        &self.breaker
    }

    pub fn limiter(&self) -> &TokenBucket {
        &self.limiter
    }

    /// 取某个 URL 所属站点的 robots 规则，按 `scheme://host` 缓存。
    ///
    /// 取不到 robots.txt 时按 RFC 9309 保守处理：
    /// - 2xx → 解析并应用
    /// - 4xx → 视为「站点没有声明限制」
    /// - 5xx / 网络失败 → 视为**完全禁止**
    async fn rules_for(&self, url: &str) -> Arc<RobotsRules> {
        let Some(origin) = origin_of(url) else {
            // URL 都解析不了，保守禁止。
            return Arc::new(robots::disallow_all());
        };

        if let Some(found) = self
            .robots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&origin)
            .cloned()
        {
            return found;
        }

        let rules = Arc::new(self.load_robots(&origin).await);
        self.robots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(origin, rules.clone());
        rules
    }

    async fn load_robots(&self, origin: &str) -> RobotsRules {
        let url = format!("{origin}/robots.txt");
        match self.transport.get(&url, &[]).await {
            Ok(response) if (200..300).contains(&response.status) => {
                let rules = RobotsRules::parse(&response.body);
                if let Some(delay) = rules.crawl_delay(&self.config.user_agent) {
                    // 简化：站点声明的 Crawl-delay 作为全局限速下界，只能变慢。
                    if self.limiter.apply_crawl_delay(delay) {
                        log::info!(
                            "{origin} 的 Crawl-delay={delay}s 已作为限速下界，当前 {:.2} QPS",
                            self.limiter.rate_per_sec()
                        );
                    }
                }
                rules
            }
            Ok(response) if (400..500).contains(&response.status) => robots::unrestricted(),
            Ok(response) => {
                log::warn!(
                    "{url} 返回 HTTP {}，按保守策略视为完全禁止",
                    response.status
                );
                robots::disallow_all()
            }
            Err(error) => {
                log::warn!("取不到 {url}（{error}），按保守策略视为完全禁止");
                robots::disallow_all()
            }
        }
    }
}

impl TierSource for PublicWebTier {
    fn tier(&self) -> Tier {
        Tier::PublicWeb
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        challenge_markers: &'a [String],
    ) -> TierFuture<'a> {
        Box::pin(async move {
            if !self.config.enabled {
                return TierOutcome::Skipped {
                    reason: "未配置：公开页面抓取默认关闭".to_string(),
                };
            }

            // 熔断优先于一切网络动作：熔断中连 robots.txt 都不该再去拉。
            if !self.breaker.allow() {
                return TierOutcome::Failed {
                    reason: format!(
                        "熔断中，还需等待 {:.0} 秒",
                        self.breaker.retry_after_secs()
                    ),
                };
            }

            // ---- 合规闸门在网络之前 ----
            let rules = self.rules_for(url).await;
            if rules.decide(&self.config.user_agent, url).is_disallowed() {
                // robots 拒绝不是站点故障，把半开探测名额还回去，不计入熔断。
                self.breaker.release_probe();
                return TierOutcome::Refused {
                    reason: format!("robots.txt 禁止抓取 {}", path_of(url)),
                };
            }

            let response = match self.transport.get(url, headers).await {
                Ok(response) => response,
                Err(error) => {
                    self.breaker.record_failure();
                    return TierOutcome::Failed {
                        reason: format!("请求失败: {error}"),
                    };
                }
            };

            // ---- 挑战检测：识别到就停，不重试 ----
            let profile = ChallengeProfile::from_source(if challenge_markers.is_empty() {
                None
            } else {
                Some(challenge_markers)
            });
            if let Some(challenge) = profile.detect(response.status, &response.body) {
                self.breaker.record_failure();
                return TierOutcome::Failed {
                    reason: format!("识别到挑战 {challenge}；已停止重试并计入熔断"),
                };
            }

            if !(200..300).contains(&response.status) {
                self.breaker.record_failure();
                return TierOutcome::Failed {
                    reason: format!("HTTP {}", response.status),
                };
            }

            self.breaker.record_success();
            TierOutcome::Fetched {
                html: response.body,
            }
        })
    }
}

/// `scheme://host[:port]`。
fn origin_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let mut origin = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

/// 只取路径，用于人类可读的错误信息。
fn path_of(url: &str) -> String {
    Url::parse(url)
        .map(|parsed| parsed.path().to_string())
        .unwrap_or_else(|_| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::clock::ManualClock;
    use crate::net::transport::testing::ScriptedTransport;

    const PAGE: &str = "<html><body>页面内容足够长，用来绕过空壳页判定。</body></html>";

    fn long_page(marker: &str) -> String {
        format!(
            "<html><body><div id=\"content\">{}</div></body></html>",
            "正文段落。".repeat(60) + marker
        )
    }

    fn web_tier(
        transport: Arc<ScriptedTransport>,
        enabled: bool,
        threshold: u32,
    ) -> (Ladder, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let config = {
            let config = PublicWebConfig::new()
                .user_agent(DEFAULT_USER_AGENT)
                .rate(1000.0, 10)
                .breaker(threshold, 300.0);
            if enabled {
                config.enabled()
            } else {
                config
            }
        };
        let tier = PublicWebTier::new(transport, clock.clone(), config);
        (Ladder::new(vec![Box::new(tier)]), clock)
    }

    #[tokio::test]
    async fn disabled_public_web_tier_touches_no_network() {
        let transport = Arc::new(ScriptedTransport::new());
        let (ladder, _clock) = web_tier(transport.clone(), false, 3);

        let message = ladder
            .fetch("https://s/book/1", &[], &[])
            .await
            .unwrap_err()
            .to_string();

        assert!(message.contains("默认关闭"), "实际: {message}");
        assert_eq!(transport.call_count(), 0, "关闭状态下不该有任何网络调用");
    }

    /// 验收清单里的核心不变量：**被 robots 拒绝的 URL 不产生任何请求**。
    #[tokio::test]
    async fn robots_refusal_produces_no_request_for_the_page() {
        let transport = Arc::new(ScriptedTransport::new());
        transport.push("https://s/robots.txt", 200, "User-agent: *\nDisallow: /private\n");
        let (ladder, _clock) = web_tier(transport.clone(), true, 3);

        let message = ladder
            .fetch("https://s/private/book/1", &[], &[])
            .await
            .unwrap_err()
            .to_string();

        assert!(message.contains("robots.txt 禁止"), "实际: {message}");
        assert_eq!(
            transport.hits("https://s/private/book/1"),
            0,
            "闸门必须在网络之前"
        );
        assert_eq!(
            transport.calls(),
            vec!["https://s/robots.txt".to_string()],
            "只应该拉过 robots.txt"
        );
    }

    #[tokio::test]
    async fn allowed_path_is_fetched_after_the_gate() {
        let transport = Arc::new(ScriptedTransport::new());
        transport.push("https://s/robots.txt", 200, "User-agent: *\nDisallow: /private\n");
        transport.push("https://s/book/1", 200, &long_page(""));
        let (ladder, _clock) = web_tier(transport.clone(), true, 3);

        let fetched = ladder.fetch("https://s/book/1", &[], &[]).await.unwrap();
        assert_eq!(fetched.tier, Tier::PublicWeb);
        assert!(fetched.html.contains("正文段落"));
        assert_eq!(transport.hits("https://s/book/1"), 1);
    }

    /// robots.txt 返回 4xx → 按 RFC 视为站点没有声明限制。
    #[tokio::test]
    async fn missing_robots_means_unrestricted() {
        let transport = Arc::new(ScriptedTransport::new());
        transport.push("https://s/robots.txt", 404, "not found");
        transport.push("https://s/book/1", 200, &long_page(""));
        let (ladder, _clock) = web_tier(transport.clone(), true, 3);

        assert!(ladder.fetch("https://s/book/1", &[], &[]).await.is_ok());
    }

    /// robots.txt 返回 5xx → 保守处理，视为完全禁止。
    #[tokio::test]
    async fn broken_robots_disables_conservatively() {
        let transport = Arc::new(ScriptedTransport::new());
        transport.push("https://s/robots.txt", 503, "");
        transport.push("https://s/book/1", 200, &long_page(""));
        let (ladder, _clock) = web_tier(transport.clone(), true, 3);

        let message = ladder
            .fetch("https://s/book/1", &[], &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("robots.txt 禁止"), "实际: {message}");
        assert_eq!(transport.hits("https://s/book/1"), 0);
    }

    /// 识别到挑战就停下来熔断 —— 第三次请求应该被熔断挡在本地，不再打站点。
    #[tokio::test]
    async fn captcha_opens_the_breaker_and_stops_retrying() {
        let transport = Arc::new(ScriptedTransport::new());
        transport.push("https://s/robots.txt", 200, "User-agent: *\n");
        let wall = format!("<html><body>{}</body></html>", "x".repeat(600) + "请输入验证码");
        transport.push("https://s/book/1", 200, &wall);

        let (ladder, _clock) = web_tier(transport.clone(), true, 2);

        for round in 1..=2 {
            let message = ladder
                .fetch("https://s/book/1", &[], &[])
                .await
                .unwrap_err()
                .to_string();
            assert!(
                message.contains("识别到挑战"),
                "第 {round} 次应识别为挑战: {message}"
            );
        }

        let message = ladder
            .fetch("https://s/book/1", &[], &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("熔断中"), "第三次应被熔断挡住: {message}");
        assert_eq!(
            transport.hits("https://s/book/1"),
            2,
            "熔断后不该再打站点"
        );
    }

    #[tokio::test]
    async fn crawl_delay_slows_the_limiter_down() {
        let transport = Arc::new(ScriptedTransport::new());
        transport.push(
            "https://s/robots.txt",
            200,
            "User-agent: *\nCrawl-delay: 4\n",
        );
        transport.push("https://s/book/1", 200, &long_page(""));

        let clock = Arc::new(ManualClock::new());
        let config = PublicWebConfig::new()
            .user_agent(DEFAULT_USER_AGENT)
            .rate(50.0, 5)
            .enabled();
        let tier = PublicWebTier::new(transport, clock, config);
        // 令牌桶是 Arc 包着的，克隆一份句柄，装箱后仍然能观察速率。
        let limiter = tier.limiter().clone();
        assert_eq!(limiter.rate_per_sec(), 50.0);

        let ladder = Ladder::new(vec![Box::new(tier)]);
        ladder.fetch("https://s/book/1", &[], &[]).await.unwrap();

        assert!(
            (limiter.rate_per_sec() - 0.25).abs() < 1e-9,
            "Crawl-delay=4 应把限速压到 0.25 QPS，实际 {}",
            limiter.rate_per_sec()
        );
    }

    #[tokio::test]
    async fn unconfigured_tiers_are_skipped_honestly() {
        let ladder = Ladder::new(vec![
            Box::new(UnconfiguredTier::official_api()),
            Box::new(UnconfiguredTier::authorized_data()),
        ]);

        let message = ladder
            .fetch("https://s/x", &[], &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("未配置：需要官方 API 凭据"), "实际: {message}");
        assert!(
            message.contains("未配置：需要站点或用户授权数据源"),
            "实际: {message}"
        );
    }

    /// 阶梯的意义：L2 关掉时自动落到 L3，而不是整体失败。
    #[tokio::test]
    async fn replay_catches_when_public_web_is_disabled() {
        let transport = Arc::new(ScriptedTransport::new());
        let (mut ladder, _clock) = web_tier(transport.clone(), false, 3);

        let mut replay = ReplayTier::empty();
        replay.insert("/book/1", "<html>回放详情</html>");
        ladder.tiers.push(Box::new(replay));

        let fetched = ladder.fetch("https://s/book/1", &[], &[]).await.unwrap();
        assert_eq!(fetched.tier, Tier::Replay);
        assert_eq!(fetched.html, "<html>回放详情</html>");
        assert_eq!(transport.call_count(), 0);

        // 尝试记录里能看到 L2 被跳过的原因。
        assert_eq!(
            fetched.attempts.iter().map(|a| a.kind).collect::<Vec<_>>(),
            vec![AttemptKind::Skipped, AttemptKind::Hit],
            "L2 应先被跳过、L3 再命中"
        );
        let joined = fetched
            .attempts
            .iter()
            .map(TierAttempt::describe)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(joined.contains("默认关闭"), "实际: {joined}");
        assert!(joined.contains("命中"), "实际: {joined}");
    }

    #[test]
    fn origin_drops_path_and_query() {
        assert_eq!(
            origin_of("https://www.example.com/a/b?c=1").as_deref(),
            Some("https://www.example.com")
        );
        assert_eq!(
            origin_of("http://s:8080/a").as_deref(),
            Some("http://s:8080")
        );
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn public_web_config_defaults_to_disabled() {
        assert!(!PublicWebConfig::new().is_enabled());
        assert!(PublicWebConfig::new().enabled().is_enabled());
    }
}

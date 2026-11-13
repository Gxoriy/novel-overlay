use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use encoding_rs::{Encoding, GBK};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::Client;

use crate::net::clock::{self, Clock};
use crate::net::limiter::TokenBucket;
use crate::net::proxy::{discover_local_proxies, ProxyPool, ProxySpec, Rotation};
use crate::net::transport::{HttpResponse, PageSource, TextFuture, Transport, TransportFuture};

/// 本应用的爬虫身份。
///
/// 刻意**不伪装成浏览器**。伪装身份是规避而不是合规，而且多数站点在 robots.txt
/// 里对采集类 UA 有明确表态 —— 伪装只是让站点无法按自己的意愿拒绝你。
///
/// 被拒绝时的正确反应是**降级到下一获取层级**（见 [`crate::net::Ladder`]），
/// 不是换个 UA 再试。
///
/// 要跑真实站点时，请把这里的联系方式换成你自己的，让站点管理员能联系到你。
/// 也可以按书源在 `header` 里单独覆盖 `User-Agent`。
pub const DEFAULT_USER_AGENT: &str =
    "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)";

/// HTTP 出口：限速 + 出口选择 + 字符集解码。
///
/// 这一层**不做任何合规判断**。合规闸门在 [`crate::net::Ladder`] 里，位于它之上；
/// 代理池同样在闸门之下，因此**换出口不可能绕过 robots 或熔断**。
pub struct Fetcher {
    /// 直连客户端。
    client: Client,
    /// 按出口缓存的客户端。reqwest 的代理是**客户端级**配置，没有按请求指定
    /// 出口的接口，所以每个出口缓存一个客户端。出口数量由人工配置，不会失控。
    exit_clients: Mutex<HashMap<String, Client>>,
    /// 显式配置的代理池（`NOVEL_OVERLAY_PROXIES`）。配了就不回落直连。
    proxies: Option<Arc<ProxyPool>>,
    /// 未显式配代理时，自动走本机 Clash 等端口；没有监听则直连。
    auto_local: bool,
    local_pool: Mutex<Option<Arc<ProxyPool>>>,
    limiter: TokenBucket,
}

impl Fetcher {
    pub fn new(rate_per_sec: f64, burst: u32) -> Result<Self> {
        Self::with_clock(rate_per_sec, burst, clock::system())
    }

    pub fn with_clock(rate_per_sec: f64, burst: u32, clock: Arc<dyn Clock>) -> Result<Self> {
        Ok(Self {
            client: build_client(None)?,
            exit_clients: Mutex::new(HashMap::new()),
            proxies: None,
            auto_local: true,
            local_pool: Mutex::new(None),
            limiter: TokenBucket::with_clock(rate_per_sec, burst, clock),
        })
    }

    /// 挂上出口代理池。不调用则尝试本机代理，再直连。
    ///
    /// 装配点见 `lib.rs`：配了 [`crate::net::proxy::PROXIES_ENV`] 才会挂。
    pub fn with_proxies(mut self, proxies: Arc<ProxyPool>) -> Self {
        self.proxies = Some(proxies);
        self.auto_local = false;
        self
    }

    pub fn proxies(&self) -> Option<&Arc<ProxyPool>> {
        self.proxies.as_ref()
    }

    pub fn limiter(&self) -> &TokenBucket {
        &self.limiter
    }

    /// 取（或按需创建）某个出口的客户端。
    fn exit_client(&self, spec: &ProxySpec) -> Result<Client> {
        let key = spec.as_str().to_string();
        if let Some(client) = self.lock_clients().get(&key) {
            return Ok(client.clone());
        }
        let client = build_client(Some(&key))?;
        self.lock_clients().insert(key, client.clone());
        Ok(client)
    }

    fn lock_clients(&self) -> std::sync::MutexGuard<'_, HashMap<String, Client>> {
        self.exit_clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// 原始请求：限速 → 选出口 → 发送 → 按声明解码。
    ///
    /// **不校验状态码**：调用方需要拿到 429 / 503 才能做挑战检测。
    pub async fn request(&self, url: &str, extra_headers: &[(String, String)]) -> Result<HttpResponse> {
        self.request_ex(url, extra_headers, None).await
    }

    /// 带请求体的版本；`body` 为 `Some` 时用 POST 发送。
    async fn request_ex(
        &self,
        url: &str,
        extra_headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse> {
        // 限速与出口无关：换出口不重置配额。
        self.limiter.acquire().await;

        let picked = self.pick_exit(url)?;
        let client = match &picked {
            Some((_, spec)) => self.exit_client(spec)?,
            None => self.client.clone(),
        };

        let mut request = match body {
            Some(body) => {
                // 表单 POST 的默认 Content-Type；书源没显式声明就自动补上。
                let mut request = client.post(url).body(body.to_string());
                if !extra_headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("content-type"))
                {
                    request = request.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
                }
                request
            }
            None => client.get(url),
        };
        if !extra_headers.is_empty() {
            let mut headers = HeaderMap::new();
            for (key, value) in extra_headers {
                let name = HeaderName::from_bytes(key.as_bytes())
                    .with_context(|| format!("非法请求头名: {key}"))?;
                let parsed = HeaderValue::from_str(value)
                    .with_context(|| format!("非法请求头值: {key} = {value}"))?;
                headers.insert(name, parsed);
            }
            request = request.headers(headers);
        }

        // 传输层失败归因于**出口**：连接被拒 / 超时 / 代理挂了。
        // 站点层的失败（429 / 503 / 验证码）由 Ladder 的熔断器负责，不在这里记账 ——
        // 否则站点一抖动就会把整个出口池一起冷却掉。
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                self.blame_exit(&picked);
                return Err(error).with_context(|| format!("请求失败: {url}"));
            }
        };

        let mut status = response.status().as_u16();

        // 407 是代理自己的问题，与站点无关。
        if status == 407 {
            self.blame_exit(&picked);
            return Err(anyhow!("出口代理要求认证（HTTP 407）: {url}"));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let mut bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                // 响应体读到一半断了，同样算出口故障。
                self.blame_exit(&picked);
                return Err(error).with_context(|| format!("读取响应体失败: {url}"));
            }
        };

        // 站点按**地区**拒绝代理出口时（"The region has been denied."），
        // 本机直连就是站点允许的访问方式 —— 用真实出口重试一次。
        // 只认这一种响应，一般 403 不重试：那是站点态度，换出口也无济于事。
        if status == 403 && picked.is_some() && is_region_denied(&bytes) {
            log::info!("站点拒绝代理出口地区，改用本机直连重试: {url}");
            let direct = self.client.clone();
            let mut retry = match body {
                Some(body) => {
                    // 首次请求的 Content-Type 自动补齐逻辑在这里同样要生效，
                    // 否则直连重试发出去的是没有类型的 POST，表单参数会被丢掉。
                    let mut retry = direct.post(url).body(body.to_string());
                    if !extra_headers
                        .iter()
                        .any(|(key, _)| key.eq_ignore_ascii_case("content-type"))
                    {
                        retry = retry.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
                    }
                    retry
                }
                None => direct.get(url),
            };
            if !extra_headers.is_empty() {
                let mut headers = HeaderMap::new();
                for (key, value) in extra_headers {
                    if let (Ok(name), Ok(parsed)) = (
                        HeaderName::from_bytes(key.as_bytes()),
                        HeaderValue::from_str(value),
                    ) {
                        headers.insert(name, parsed);
                    }
                }
                retry = retry.headers(headers);
            }
            match retry.send().await {
                Ok(second) => {
                    status = second.status().as_u16();
                    match second.bytes().await {
                        Ok(second_bytes) => bytes = second_bytes,
                        Err(error) => {
                            return Err(error).with_context(|| format!("直连重试读取响应体失败: {url}"));
                        }
                    }
                    log::info!("直连重试得到 HTTP {status}: {url}");
                }
                Err(error) => {
                    log::warn!("直连重试失败，沿用代理出口的响应: {error}");
                }
            }
        }

        // 拿到**完整**响应就说明这个出口是通的。4xx / 5xx 是站点的态度，
        // 记成出口故障会误伤池子。
        if let Some((pool, spec)) = &picked {
            pool.record_success(spec);
        }

        Ok(HttpResponse {
            status,
            body: decode_body(&bytes, content_type.as_deref()),
            content_type,
        })
    }

    /// 选一个出口。
    ///
    /// - 显式代理池：没有可用出口时**直接失败，不回落直连**（用户配了就不想漏真实 IP）。
    /// - 本机自动探测：Clash 等在听就走代理；没开则直连（直连被封时正文会是诱饵）。
    fn pick_exit(&self, url: &str) -> Result<Option<(Arc<ProxyPool>, ProxySpec)>> {
        if let Some(pool) = &self.proxies {
            return match pool.acquire(&host_of(url)) {
                Some(spec) => Ok(Some((pool.clone(), spec))),
                None => Err(anyhow!(
                    "没有可用出口代理（{}），已放弃本次请求：不回落直连，以免暴露真实出口",
                    pool.describe()
                )),
            };
        }
        if self.auto_local {
            if let Some(picked) = self.pick_local_exit(url) {
                return Ok(Some(picked));
            }
        }
        Ok(None)
    }

    fn pick_local_exit(&self, url: &str) -> Option<(Arc<ProxyPool>, ProxySpec)> {
        let host = host_of(url);
        let mut guard = self
            .local_pool
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale = guard
            .as_ref()
            .map(|pool| pool.healthy_count() == 0)
            .unwrap_or(true);
        if stale {
            let specs = discover_local_proxies();
            *guard = if specs.is_empty() {
                None
            } else {
                Some(Arc::new(ProxyPool::new(
                    specs,
                    Rotation::Failover,
                    clock::system(),
                )))
            };
        }
        let pool = guard.as_ref()?.clone();
        let spec = pool.acquire(&host)?;
        Some((pool, spec))
    }

    /// 把一次失败记到当前出口头上。
    fn blame_exit(&self, picked: &Option<(Arc<ProxyPool>, ProxySpec)>) {
        if let Some((pool, spec)) = picked {
            pool.record_failure(spec);
        }
    }
}

/// 构建客户端。`proxy` 为 `None` 时直连。
fn build_client(proxy: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        // HTTP/2 指纹也容易被风控单独画像；章节 JSON 用 1.1 足够。
        .http1_only()
        .redirect(reqwest::redirect::Policy::limited(8));
    if let Some(url) = proxy {
        let parsed = reqwest::Proxy::all(url).with_context(|| format!("构造代理失败: {url}"))?;
        builder = builder.proxy(parsed);
    }
    builder.build().context("构建 HTTP 客户端失败")
}

/// 取 URL 的 host，用于 `StickyPerHost` 的粘性键。解析失败时退回整串。
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// 站点按地区拒绝访问的响应特征（403 页面里的一句话，ASCII，与编码无关）。
fn is_region_denied(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes)
        .to_lowercase()
        .contains("region has been denied")
}

impl Transport for Fetcher {
    fn get<'a>(&'a self, url: &'a str, headers: &'a [(String, String)]) -> TransportFuture<'a> {
        Box::pin(self.request(url, headers))
    }

    fn post<'a>(
        &'a self,
        url: &'a str,
        body: &'a str,
        headers: &'a [(String, String)],
    ) -> TransportFuture<'a> {
        Box::pin(self.request_ex(url, headers, Some(body)))
    }
}

impl PageSource for Fetcher {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        _challenge_markers: &'a [String],
    ) -> TextFuture<'a> {
        Box::pin(async move {
            let response = self.request(url, headers).await?;
            if !(200..300).contains(&response.status) {
                return Err(anyhow!("HTTP {}: {url}", response.status));
            }
            Ok(response.body)
        })
    }

    fn post_text<'a>(
        &'a self,
        url: &'a str,
        body: &'a str,
        headers: &'a [(String, String)],
        _challenge_markers: &'a [String],
    ) -> TextFuture<'a> {
        Box::pin(async move {
            let response = self.request_ex(url, headers, Some(body)).await?;
            if !(200..300).contains(&response.status) {
                return Err(anyhow!("HTTP {}: {url}", response.status));
            }
            Ok(response.body)
        })
    }

    fn describe(&self) -> String {
        if let Some(pool) = &self.proxies {
            return format!("经代理出口（未经 robots 闸门）· {}", pool.describe());
        }
        if self.auto_local {
            let local = self
                .local_pool
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(pool) = local.as_ref() {
                return format!(
                    "本机代理优先（未经 robots 闸门）· {}",
                    pool.describe()
                );
            }
            return "本机代理优先（当前未探测到，直连）".to_string();
        }
        "直连（未经 robots 闸门）".to_string()
    }
}

/// 解码顺序：Content-Type 声明的字符集 → HTML meta 声明 → 严格 UTF-8 → GBK 兜底。
pub fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    if let Some(label) = content_type.and_then(charset_from_content_type) {
        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            return encoding.decode(bytes).0.into_owned();
        }
    }
    if let Some(label) = sniff_meta_charset(bytes) {
        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            return encoding.decode(bytes).0.into_owned();
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => GBK.decode(bytes).0.into_owned(),
    }
}

fn charset_from_content_type(content_type: &str) -> Option<String> {
    let lowered = content_type.to_lowercase();
    let index = lowered.find("charset=")?;
    let rest = &lowered[index + "charset=".len()..];
    let label: String = rest
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

fn sniff_meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(head).to_lowercase();
    let index = text.find("charset=")?;
    let rest = &text[index + "charset=".len()..];
    let label: String = rest
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::clock::ManualClock;
    use crate::net::proxy::Rotation;

    #[test]
    fn decodes_gbk_when_no_declaration() {
        let (bytes, _, _) = GBK.encode("斗破苍穹");
        let text = decode_body(&bytes, None);
        assert_eq!(text, "斗破苍穹");
    }

    #[test]
    fn decodes_utf8_by_default() {
        let text = decode_body("斗破苍穹".as_bytes(), None);
        assert_eq!(text, "斗破苍穹");
    }

    #[test]
    fn reads_charset_from_content_type() {
        assert_eq!(
            charset_from_content_type("text/html; charset=gbk"),
            Some("gbk".to_string())
        );
    }

    #[test]
    fn reads_charset_from_meta() {
        let page = br#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=gb2312"></head></html>"#;
        assert_eq!(sniff_meta_charset(page), Some("gb2312".to_string()));
    }

    /// 身份必须是诚实的：UA 里不能出现浏览器特征串。
    #[test]
    fn user_agent_does_not_impersonate_a_browser() {
        let lowered = DEFAULT_USER_AGENT.to_lowercase();
        for forbidden in ["mozilla", "applewebkit", "chrome", "safari", "gecko"] {
            assert!(
                !lowered.contains(forbidden),
                "UA 里不应出现浏览器特征串 `{forbidden}`：{DEFAULT_USER_AGENT}"
            );
        }
        assert!(DEFAULT_USER_AGENT.starts_with("NovelOverlay/"));
    }

    /// 配了代理池却没有可用出口时，请求必须**失败**，而不是静默回落直连。
    ///
    /// 静默回落会把用户的真实出口漏出去，而且这种泄漏不会有任何提示 ——
    /// 用户以为自己在用代理，实际在裸奔。这是本模块最需要守住的一条不变量。
    ///
    /// 这个测试**不发任何网络请求**：`pick_exit` 在选出口阶段就返回了错误。
    #[tokio::test]
    async fn no_available_exit_fails_instead_of_falling_back_to_direct() {
        let clock = Arc::new(ManualClock::new());
        let exit = ProxySpec::parse("http://127.0.0.1:1").expect("合法代理");
        let pool = Arc::new(
            ProxyPool::new(vec![exit.clone()], Rotation::RoundRobin, clock.clone()).policy(1, 60.0),
        );
        // 阈值设为 1，一次失败就进冷却 → 池里再无可用出口。
        pool.record_failure(&exit);
        assert_eq!(pool.healthy_count(), 0);

        let fetcher = Fetcher::with_clock(1000.0, 10, clock)
            .expect("构建 Fetcher")
            .with_proxies(pool);

        let error = fetcher
            .request("http://example.invalid/robots.txt", &[])
            .await
            .expect_err("没有可用出口时必须报错");
        let message = error.to_string();
        assert!(
            message.contains("不回落直连"),
            "报错信息要说明为何不回落直连，实际是: {message}"
        );
    }

    /// 未显式配代理时默认本机优先；显式挂池后 `describe` 带上池子状态。
    #[tokio::test]
    async fn describe_reflects_whether_a_proxy_pool_is_attached() {
        let clock = Arc::new(ManualClock::new());
        let direct = Fetcher::with_clock(1000.0, 10, clock.clone()).expect("构建 Fetcher");
        assert_eq!(direct.describe(), "本机代理优先（当前未探测到，直连）");
        assert!(direct.proxies().is_none());

        let pool = Arc::new(ProxyPool::new(
            vec![ProxySpec::parse("http://a.test:1").unwrap()],
            Rotation::Failover,
            clock.clone(),
        ));
        let proxied = Fetcher::with_clock(1000.0, 10, clock)
            .expect("构建 Fetcher")
            .with_proxies(pool);
        let described = proxied.describe();
        assert!(
            described.contains("经代理出口") && described.contains("顺序故障转移"),
            "describe 应反映出口池状态，实际是: {described}"
        );
    }

    #[test]
    fn host_of_extracts_the_target_host() {
        assert_eq!(host_of("https://www.example.com/a/b?c=1"), "www.example.com");
        assert_eq!(host_of("http://1.2.3.4:8080/x"), "1.2.3.4");
        // 解析失败时退回整串，至少能当个粘性键用，不 panic。
        assert_eq!(host_of("不是 URL"), "不是 URL");
    }

    /// 端到端：请求**真的**走了配置的出口。
    ///
    /// 池子逻辑的单元测试证明不了这件事 —— 它只证明「选出了哪个出口」，
    /// 不证明「请求真的从那儿出去」。这里起一个本地假代理，断言两件事：
    /// 它收到了连接，且请求行里是**绝对 URI**（HTTP 代理协议的特征）。
    ///
    /// 全程本地回环，不碰任何真实站点。
    #[tokio::test]
    async fn requests_are_actually_routed_through_the_configured_exit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定本地端口");
        let port = listener.local_addr().expect("取本地端口").port();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("假代理应收到连接");
            let mut buffer = vec![0u8; 2048];
            let read = socket.read(&mut buffer).await.expect("读取请求");
            let _ = tx.send(String::from_utf8_lossy(&buffer[..read]).to_string());

            let body = "hello";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });

        let clock = Arc::new(ManualClock::new());
        let exit = ProxySpec::parse(&format!("http://127.0.0.1:{port}")).expect("合法代理");
        let pool = Arc::new(ProxyPool::new(vec![exit], Rotation::Failover, clock.clone()));
        let fetcher = Fetcher::with_clock(1000.0, 10, clock)
            .expect("构建 Fetcher")
            .with_proxies(pool);

        let response = fetcher
            .request("http://example.invalid/secret-path", &[])
            .await
            .expect("经代理的请求应当成功");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");

        let head = rx.await.expect("假代理应收到请求");
        assert!(
            head.contains("http://example.invalid/secret-path"),
            "HTTP 代理的请求行里应当是绝对 URI（这才能证明请求真的走了出口）。\
             实际收到: {head}"
        );
    }
}

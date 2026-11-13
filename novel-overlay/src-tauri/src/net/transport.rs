//! 传输抽象。
//!
//! 抽这一层有两个目的：
//!
//! 1. **让合规闸门可以被证明在网络之前**。测试注入一个「记录调用次数」的
//!    transport，指向被 robots 禁止的 URL，断言调用次数为 0。
//! 2. **让采集内核可离线端到端测试**。`SourceEngine` 依赖 [`PageSource`] 而不是
//!    具体的 `Fetcher`，于是 search → toc → content 整条链路可以用回放层跑通。
//!
//! 两个 trait 的返回值都是 boxed future 而不是 `async fn`：`async fn` 在 trait 里
//! 不满足 dyn-safe，而这里必须能用 `dyn`（`Arc<dyn Transport>` / `&dyn PageSource`）。

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

/// 原始 HTTP 响应。保留状态码是因为挑战检测需要区分 429 / 503 / 200。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: Some("text/html; charset=utf-8".to_string()),
            body: body.into(),
        }
    }
}

pub type TransportFuture<'a> = Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>>;

/// 最底层的网络出口。**没有任何合规判断**，闸门在它之上（见 `Ladder`）。
pub trait Transport: Send + Sync {
    fn get<'a>(&'a self, url: &'a str, headers: &'a [(String, String)]) -> TransportFuture<'a>;
}

pub type TextFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// 一次页面获取里「某一层发生了什么」。
///
/// 只描述事实，不做归因 —— 归因（该不该计入熔断、要不要换出口）留在各层自己那里。
#[derive(Debug, Clone)]
pub struct AttemptStep {
    /// 来源标签，例如 `L2 公开页面`。
    pub source: String,
    /// `ok` / `info` / `warn` / `fail`。
    pub status: String,
    /// 人类可读的一行。
    pub detail: String,
}

/// 一次页面获取的结果，附带「怎么拿到的」轨迹。
#[derive(Debug, Clone)]
pub struct PageFetch {
    pub html: String,
    /// 最终是谁给的页面。
    pub source: String,
    /// 依次发生了什么。空表示该实现没有分级概念。
    pub attempts: Vec<AttemptStep>,
}

impl PageFetch {
    /// 没有轨迹可报的实现用它构造。
    pub fn plain(html: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            html: html.into(),
            source: source.into(),
            attempts: Vec::new(),
        }
    }
}

pub type PageFuture<'a> = Pin<Box<dyn Future<Output = Result<PageFetch>> + Send + 'a>>;

/// 采集内核看到的「取页面」接口。
///
/// 由 [`crate::net::Fetcher`]（直连）、[`crate::net::Ladder`]（分级 + 闸门）与
/// [`crate::net::ReplayTier`]（离线回放）分别实现。
pub trait PageSource: Send + Sync {
    /// 取一个页面的文本。实现负责：2xx 校验、字符集解码、合规闸门、限速、挑战检测。
    ///
    /// `challenge_markers` 是书源自带的挑战标记，用于覆盖默认标记集；
    /// 传空切片表示只用默认标记。
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        challenge_markers: &'a [String],
    ) -> TextFuture<'a>;

    /// 取页面，并带上「怎么拿到的」轨迹。诊断面板靠它回答「为什么走了这一层」。
    ///
    /// 默认实现只委托 [`Self::get_text`] 并留空轨迹 —— 只有分级阶梯有轨迹可报。
    /// 覆写时**不要**反过来调 `get_text`，否则会与默认实现互相递归。
    fn get_page<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        challenge_markers: &'a [String],
    ) -> PageFuture<'a> {
        let source = self.describe();
        Box::pin(async move {
            let html = self.get_text(url, headers, challenge_markers).await?;
            Ok(PageFetch::plain(html, source))
        })
    }

    /// 人类可读的来源描述，用于日志与错误信息。
    fn describe(&self) -> String;
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 脚本化 transport：不做任何网络调用，按 URL 返回预置响应，并记录被请求过什么。
    ///
    /// 用来锁死「合规闸门在网络之前」这条不变量 —— 断言某个 URL 的命中次数为 0。
    /// 未登记的 URL 返回 599，这样「意外请求」会立刻暴露而不是静默通过。
    pub struct ScriptedTransport {
        routes: Mutex<HashMap<String, HttpResponse>>,
        calls: Mutex<Vec<String>>,
        counter: AtomicUsize,
    }

    impl Default for ScriptedTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ScriptedTransport {
        pub fn new() -> Self {
            Self {
                routes: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
                counter: AtomicUsize::new(0),
            }
        }

        pub fn push(&self, url: &str, status: u16, body: &str) {
            self.routes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(url.to_string(), HttpResponse::new(status, body));
        }

        pub fn call_count(&self) -> usize {
            self.counter.load(Ordering::SeqCst)
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// 某个 URL 被请求过几次。
        pub fn hits(&self, url: &str) -> usize {
            self.calls().iter().filter(|call| call.as_str() == url).count()
        }
    }

    impl Transport for ScriptedTransport {
        fn get<'a>(&'a self, url: &'a str, _headers: &'a [(String, String)]) -> TransportFuture<'a> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(url.to_string());

            let found = self
                .routes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(url)
                .cloned();

            Box::pin(async move {
                found.ok_or_else(|| {
                    anyhow::anyhow!("脚本化 transport 没有登记 {url}（说明发生了意外请求）")
                })
            })
        }
    }
}

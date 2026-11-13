//! 离线回放层。
//!
//! 从本地目录读已保存的页面。**这是开发与测试的默认路径，也是唯一不碰网络的路径。**
//!
//! # 查找规则
//!
//! 只看 URL 的**路径部分**，忽略 host 与查询串 —— 这样带随机签名参数的搜索地址
//! 也能命中同一份夹具。
//!
//! # 目录约定
//!
//! 优先读目录下的 `pages.json` 清单，显式且可读：
//!
//! ```json
//! { "/search.php": "search.html", "/book/1": "book.html" }
//! ```
//!
//! 没有清单时退化为「所有 `*.html`，键为 `/` + 文件名主干」。
//!
//! 支持多目录：自己的页面、竞品页面、历史存档的采集时机与保留策略都不一样，
//! 混在一个目录会失控。**先加载的目录优先命中。**

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use url::Url;

use crate::net::ladder::{Tier, TierFuture, TierOutcome, TierSource};
use crate::net::transport::{PageSource, TextFuture};

/// 回放层。
pub struct ReplayTier {
    pages: BTreeMap<String, String>,
    roots: Vec<PathBuf>,
}

impl ReplayTier {
    /// 空的回放层。主要用于先构造再 [`ReplayTier::insert`]。
    pub fn empty() -> Self {
        Self {
            pages: BTreeMap::new(),
            roots: Vec::new(),
        }
    }

    /// 从若干目录加载页面。不存在的目录直接跳过（不算错误）。
    pub fn from_dirs(roots: &[PathBuf]) -> Result<Self> {
        let mut tier = Self::empty();
        tier.roots = roots.to_vec();

        for root in roots {
            if !root.exists() {
                continue;
            }
            let manifest = root.join("pages.json");
            if manifest.exists() {
                tier.load_manifest(root, &manifest)?;
            } else {
                tier.load_loose_html(root)?;
            }
        }
        Ok(tier)
    }

    fn load_manifest(&mut self, root: &Path, manifest: &Path) -> Result<()> {
        let raw = std::fs::read_to_string(manifest)
            .with_context(|| format!("读取回放清单失败: {}", manifest.display()))?;
        let mapping: BTreeMap<String, String> = serde_json::from_str(&raw)
            .with_context(|| format!("回放清单不是合法 JSON 对象: {}", manifest.display()))?;

        for (key, file) in mapping {
            let path = root.join(&file);
            let html = std::fs::read_to_string(&path)
                .with_context(|| format!("回放清单指向的文件读不到: {}", path.display()))?;
            self.pages.entry(page_key(&key)).or_insert(html);
        }
        Ok(())
    }

    fn load_loose_html(&mut self, root: &Path) -> Result<()> {
        let entries = std::fs::read_dir(root)
            .with_context(|| format!("读取回放目录失败: {}", root.display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("html") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let html = std::fs::read_to_string(&path)
                .with_context(|| format!("读取回放页面失败: {}", path.display()))?;
            self.pages.entry(page_key(stem)).or_insert(html);
        }
        Ok(())
    }

    /// 手工塞一份页面（测试用）。
    pub fn insert(&mut self, url_or_path: &str, html: impl Into<String>) {
        self.pages.insert(page_key(url_or_path), html.into());
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// 已登记的路径列表，便于排查「为什么没命中」。
    pub fn paths(&self) -> Vec<&str> {
        self.pages.keys().map(String::as_str).collect()
    }

    pub fn lookup(&self, url: &str) -> Option<&str> {
        self.pages.get(&page_key(url)).map(String::as_str)
    }
}

impl TierSource for ReplayTier {
    fn tier(&self) -> Tier {
        Tier::Replay
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        _headers: &'a [(String, String)],
        _challenge_markers: &'a [String],
    ) -> TierFuture<'a> {
        // 回放层不做挑战检测：夹具本身就是期望的输入，本地文件也不存在被封禁的问题。
        let outcome = match self.lookup(url) {
            Some(html) => TierOutcome::Fetched {
                html: html.to_string(),
            },
            None => TierOutcome::Failed {
                reason: format!(
                    "回放层没有该页面：{}（已登记 {}）",
                    page_key(url),
                    self.paths().join(", ")
                ),
            },
        };
        Box::pin(async move { outcome })
    }
}

impl PageSource for ReplayTier {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        _headers: &'a [(String, String)],
        _challenge_markers: &'a [String],
    ) -> TextFuture<'a> {
        // 回放层不做挑战检测：夹具本身就是期望的输入，本地文件也不存在被封禁的问题。
        let outcome = match self.lookup(url) {
            Some(html) => Ok(html.to_string()),
            None => Err(anyhow!(
                "回放层没有该页面：{}（已登记 {}）",
                page_key(url),
                self.paths().join(", ")
            )),
        };
        Box::pin(async move { outcome })
    }

    fn describe(&self) -> String {
        format!(
            "离线回放（{} 个页面，来自 {} 个目录）",
            self.pages.len(),
            self.roots.len()
        )
    }
}

/// 把一个 URL 或路径归一化成回放键：丢掉 host / 查询串 / 锚点，补上开头的 `/`。
fn page_key(url_or_path: &str) -> String {
    let path = match Url::parse(url_or_path) {
        Ok(parsed) => parsed.path().to_string(),
        Err(_) => url_or_path.to_string(),
    };
    normalize_path(&path)
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    let without_suffix = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed);
    if without_suffix.is_empty() {
        "/".to_string()
    } else if without_suffix.starts_with('/') {
        without_suffix.to_string()
    } else {
        format!("/{without_suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::transport::PageSource;

    #[test]
    fn lookup_ignores_host_and_query() {
        let mut tier = ReplayTier::empty();
        tier.insert("/search.php", "<html>搜索页</html>");
        assert_eq!(
            tier.lookup("https://a.example.com/search.php?q=%E6%96%97&sign=abc"),
            Some("<html>搜索页</html>")
        );
        assert_eq!(
            tier.lookup("https://another.example.com/search.php"),
            Some("<html>搜索页</html>"),
            "回放不该关心 host"
        );
    }

    #[test]
    fn keys_are_normalized() {
        assert_eq!(page_key("/a/b"), "/a/b");
        assert_eq!(page_key("a/b"), "/a/b");
        assert_eq!(page_key("/a/b?x=1#f"), "/a/b");
        assert_eq!(page_key("https://s/a/b?x=1"), "/a/b");
        assert_eq!(page_key(""), "/");
    }

    #[tokio::test]
    async fn missing_page_error_lists_registered_paths() {
        let mut tier = ReplayTier::empty();
        tier.insert("/book/1", "<html>详情</html>");

        assert_eq!(
            tier.get_text("https://s/book/1", &[], &[]).await.unwrap(),
            "<html>详情</html>"
        );

        let message = tier
            .get_text("https://s/book/2", &[], &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("/book/2"), "错误应指出缺失的路径: {message}");
        assert!(message.contains("/book/1"), "错误应列出已登记路径: {message}");
    }

    #[test]
    fn nonexistent_directories_are_skipped() {
        let tier = ReplayTier::from_dirs(&[PathBuf::from("/definitely/not/here")]).unwrap();
        assert!(tier.is_empty());
    }
}

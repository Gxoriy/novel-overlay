use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// 书源规则。字段命名对齐"阅读"App 的书源 JSON 格式，便于直接导入现成规则库。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSource {
    pub book_source_name: String,
    pub book_source_url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 搜索地址模板，支持 `{{key}}` / `{key}` / `searchKey` 占位符
    pub search_url: String,
    pub rule_search: RuleSearch,
    #[serde(default)]
    pub rule_book_info: Option<RuleBookInfo>,
    pub rule_toc: RuleToc,
    pub rule_content: RuleContent,
    /// 附加请求头，JSON 对象字符串，例如 `{"Referer": "https://example.com"}`
    #[serde(default)]
    pub header: Option<String>,
    /// 站点专属的挑战标记。命中即判定为被识别，用于补充默认标记集（不覆盖）。
    #[serde(default)]
    pub challenge_markers: Option<Vec<String>>,
}

impl BookSource {
    /// 解析 `header` 字段为请求头列表。
    pub fn headers(&self) -> Result<Vec<(String, String)>> {
        let raw = match self.header.as_deref() {
            None => return Ok(Vec::new()),
            Some(value) if value.trim().is_empty() => return Ok(Vec::new()),
            Some(value) => value,
        };
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(raw).with_context(|| format!("书源 header 不是合法 JSON 对象: {raw}"))?;
        Ok(map
            .into_iter()
            .filter_map(|(key, value)| value.as_str().map(|text| (key, text.to_string())))
            .collect())
    }

    /// 站点自带的挑战标记；没有配置时返回空切片。
    pub fn challenge_markers(&self) -> &[String] {
        self.challenge_markers.as_deref().unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSearch {
    /// 结果列表选择器，例如 `css:div.result-list@css:div.item`
    pub book_list: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub intro: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub book_url: Option<String>,
    #[serde(default)]
    pub last_chapter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleBookInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub intro: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
    /// 目录页地址；留空表示目录就在详情页
    #[serde(default)]
    pub toc_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleToc {
    pub chapter_list: String,
    pub chapter_name: String,
    pub chapter_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleContent {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBook {
    pub source_id: i64,
    pub source_name: String,
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub intro: String,
    #[serde(default)]
    pub cover_url: String,
    pub book_url: String,
    #[serde(default)]
    pub last_chapter: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chapter {
    pub index: i64,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterContent {
    pub index: i64,
    pub name: String,
    pub content: String,
    pub from_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSourceBrief {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelfItem {
    pub id: i64,
    pub source_id: i64,
    pub book_url: String,
    pub name: String,
    pub author: String,
    pub intro: String,
    pub last_read_index: i64,
    pub last_read_offset: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayChapter {
    pub book_name: String,
    pub index: i64,
    pub total: i64,
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayState {
    pub visible: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub source_id: i64,
    pub book_url: String,
    pub book_name: String,
    pub done: i64,
    pub total: i64,
    pub chapter_name: String,
    pub from_cache: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadResult {
    pub source_id: i64,
    pub book_url: String,
    pub total: i64,
    pub downloaded: i64,
    pub from_cache: i64,
    pub failed: i64,
}

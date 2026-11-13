//! 笔趣阁 616 原生适配器。
//!
//! 站点是 hash SPA，业务接口在 `apibi.cc` 一族。搜索明文；书籍 / 目录 / 正文
//! 用 AES-CBC 把紧凑 JSON 封成 `token`。HTML CSS 规则解析不了这份 JSON，所以
//! 在 [`crate::engine::SourceEngine`] 里按 `bookSourceUrl` 分流到这里。
//!
//! HTTP 仍然走 [`PageSource`]：robots、限速、熔断、离线回放对这里透明。

use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde_json::Value;

use crate::engine::crypto;
use crate::model::{BookSource, Chapter, SearchBook};
use crate::net::PageSource;

const SITE: &str = "https://www.bqg616.cc";
const TOKEN_PREIMAGE: &str = "book@token.html";
const API_HOSTS: &[&str] = &["apibi.cc", "apiqu.cc", "apige.cc"];

/// 仓库内置书源。首次启动写入数据库（已有同 URL 则不覆盖启用状态）。
pub fn builtin_source() -> BookSource {
    serde_json::from_str(include_str!("../../../sources/bqg616.json"))
        .expect("内置 bqg616 书源必须符合 schema")
}

pub fn is_bqg_source(source: &BookSource) -> bool {
    source
        .book_source_url
        .to_ascii_lowercase()
        .contains("bqg616.cc")
}

pub async fn search(
    pages: &dyn PageSource,
    source: &BookSource,
    keyword: &str,
) -> Result<Vec<SearchBook>> {
    let headers = request_headers(source)?;
    let markers = source.challenge_markers();
    let path = format!("/api/search?q={}", encode_component(keyword));
    let raw = get_api(pages, &headers, markers, &path).await?;
    let value = parse_json(&raw)?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("搜索响应缺少 data 数组"))?;

    let mut results = Vec::new();
    for item in items {
        let id = json_i64(item, &["id"]).ok_or_else(|| anyhow!("搜索结果缺少 id"))?;
        let name = json_str(item, &["title", "name"]);
        if name.is_empty() {
            continue;
        }
        let cover = json_str(item, &["img", "cover", "image", "coverUrl"]);
        results.push(SearchBook {
            source_id: 0,
            source_name: String::new(),
            name,
            author: json_str(item, &["author"]),
            kind: json_str(item, &["sortname", "kind", "sort"]),
            intro: json_str(item, &["remark", "intro", "desc"]),
            cover_url: resolve_asset(&cover),
            book_url: book_url(id),
            last_chapter: json_str(item, &["lastchapter", "lastChapter"]),
        });
    }
    Ok(results)
}

pub async fn toc(pages: &dyn PageSource, source: &BookSource, book_href: &str) -> Result<Vec<Chapter>> {
    let book_id = parse_book_id(book_href)?;
    let headers = request_headers(source)?;
    let markers = source.challenge_markers();

    let meta = match api_json(pages, &headers, markers, "book", &token_id(book_id)).await {
        Ok(value) => value,
        Err(error) => {
            log::warn!("bqg book 元数据失败，回退只用目录: {error}");
            Value::Null
        }
    };
    let dir_id = json_i64(&meta, &["dirid", "dirId"]).unwrap_or(book_id);
    let last_id = json_i64(&meta, &["lastchapterid", "lastChapterId"]).unwrap_or(0);

    let names = match api_json(pages, &headers, markers, "booklist", &token_id(dir_id)).await {
        Ok(value) => booklist_names(&value)?,
        Err(error) if last_id > 0 => {
            log::warn!("bqg booklist 失败，按 lastchapterid={last_id} 生成目录: {error}");
            Vec::new()
        }
        Err(error) => return Err(error),
    };

    let total = expected_total(names.len(), last_id);
    if total == 0 {
        return Err(anyhow!("目录为空"));
    }

    let mut chapters = Vec::with_capacity(total);
    for index in 1..=total {
        let listed = names
            .get(index - 1)
            .cloned()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("第{index}章"));
        chapters.push(Chapter {
            index: (index - 1) as i64,
            name: listed,
            url: chapter_url(book_id, index as i64),
        });
    }
    Ok(chapters)
}

pub async fn content(
    pages: &dyn PageSource,
    source: &BookSource,
    chapter_href: &str,
) -> Result<String> {
    let (book_id, chapter_id) = parse_chapter_ref(chapter_href)?;
    let headers = request_headers(source)?;
    let markers = source.challenge_markers();
    let value = api_json(
        pages,
        &headers,
        markers,
        "chapter",
        &token_chapter(book_id, chapter_id),
    )
    .await?;
    let text = normalize_chapter_text(&json_str(&value, &["txt", "content"]));
    if text.is_empty() {
        return Err(anyhow!("章节 {chapter_id} 正文为空"));
    }
    if looks_like_bait(&text) {
        return Err(anyhow!(BAIT_ERROR));
    }
    Ok(text)
}

/// 站点在风控时返回的固定诱饵正文特征。
///
/// 章节标题仍正确，但 `txt` 换成同一段都市文 + 滚动水印域名。
/// 命中后不得写入本地缓存，否则书架会长期显示假书。
pub const BAIT_ERROR: &str =
    "正文接口返回风控假文（出口 IP 被限流）。请打开 Clash Verge 后再采，或设置 NOVEL_OVERLAY_PROXIES";

/// 识别笔趣阁一类接口在封禁后下发的诱饵章。
pub fn looks_like_bait(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "阿拉伯语专业的学生",
        "东北大汉的身材",
        "０６１８黄金身材",
        "我们学院的院草",
    ];
    MARKERS.iter().filter(|marker| text.contains(*marker)).count() >= 2
}

pub fn encrypt_token(plain_json: &str) -> Result<String> {
    let digest = crypto::md5_hex(TOKEN_PREIMAGE);
    let iv = &digest[..16];
    let key = &digest[16..];
    crypto::aes_encrypt("CBC", key, iv, plain_json)
}

fn token_id(id: i64) -> String {
    format!(r#"{{"id":{id}}}"#)
}

fn token_chapter(id: i64, chapter_id: i64) -> String {
    format!(r#"{{"id":{id},"chapterid":{chapter_id}}}"#)
}

fn book_url(id: i64) -> String {
    format!("{SITE}/#/book/{id}/")
}

fn chapter_url(book_id: i64, chapter_id: i64) -> String {
    format!("{SITE}/#/book/{book_id}/{chapter_id}.html")
}

fn parse_book_id(url: &str) -> Result<i64> {
    book_id_re()
        .captures(url)
        .and_then(|caps| caps.get(1))
        .and_then(|mat| mat.as_str().parse().ok())
        .ok_or_else(|| anyhow!("无法从地址解析书籍 id: {url}"))
}

fn parse_chapter_ref(url: &str) -> Result<(i64, i64)> {
    if let Some(caps) = chapter_re().captures(url) {
        let book = caps.get(1).and_then(|mat| mat.as_str().parse().ok());
        let chapter = caps.get(2).and_then(|mat| mat.as_str().parse().ok());
        if let (Some(book), Some(chapter)) = (book, chapter) {
            return Ok((book, chapter));
        }
    }
    Err(anyhow!("无法从地址解析章节: {url}"))
}

fn book_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/book/(\d+)").expect("book id 正则"))
}

fn chapter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/book/(\d+)/(\d+)").expect("chapter 正则"))
}

fn expected_total(listed: usize, last_chapter_id: i64) -> usize {
    listed.max(last_chapter_id.max(0) as usize)
}

fn booklist_names(value: &Value) -> Result<Vec<String>> {
    let items = value
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("目录响应缺少 list 数组"))?;
    Ok(items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            other => json_str(other, &["chaptername", "name", "title", "n"]),
        })
        .collect())
}

fn normalize_chapter_text(raw: &str) -> String {
    raw.replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("<br>", "\n")
        .replace("&nbsp;", " ")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn request_headers(source: &BookSource) -> Result<Vec<(String, String)>> {
    let mut headers = source.headers()?;
    if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("Referer")) {
        headers.push(("Referer".into(), format!("{SITE}/")));
    }
    if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("Accept")) {
        headers.push((
            "Accept".into(),
            "application/json,text/javascript,*/*;q=0.1".into(),
        ));
    }
    Ok(headers)
}

async fn api_json(
    pages: &dyn PageSource,
    headers: &[(String, String)],
    markers: &[String],
    name: &str,
    plain: &str,
) -> Result<Value> {
    let token = encrypt_token(plain)?;
    let path = format!("/api/{name}?token={}", encode_component(&token));
    let raw = get_api(pages, headers, markers, &path).await?;
    parse_json(&raw)
}

async fn get_api(
    pages: &dyn PageSource,
    headers: &[(String, String)],
    markers: &[String],
    path_and_query: &str,
) -> Result<String> {
    let mut last = None;
    for host in API_HOSTS {
        let url = format!("https://{host}{path_and_query}");
        match pages.get_text(&url, headers, markers).await {
            Ok(body) => return Ok(body),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("API 主机列表为空")))
        .with_context(|| format!("请求 {path_and_query} 失败"))
}

fn parse_json(raw: &str) -> Result<Value> {
    serde_json::from_str(raw.trim()).with_context(|| {
        let preview: String = raw.chars().take(80).collect();
        format!("接口返回的不是 JSON: {preview}")
    })
}

fn json_str(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        match value.get(*key) {
            Some(Value::String(text)) if !text.is_empty() => return text.clone(),
            Some(Value::Number(number)) => return number.to_string(),
            _ => {}
        }
    }
    String::new()
}

fn json_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        match value.get(*key) {
            Some(Value::Number(number)) => {
                if let Some(int) = number.as_i64() {
                    return Some(int);
                }
            }
            Some(Value::String(text)) => {
                if let Ok(int) = text.trim().parse::<i64>() {
                    return Some(int);
                }
            }
            _ => {}
        }
    }
    None
}

fn resolve_asset(raw: &str) -> String {
    let href = raw.trim();
    if href.is_empty() {
        return String::new();
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    if href.starts_with('/') {
        format!("{SITE}{href}")
    } else {
        format!("{SITE}/{href}")
    }
}

/// RFC 3986 非保留字符以外全部百分号编码，对齐 Python `quote(safe="")`。
fn encode_component(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::PassThroughJs;
    use crate::engine::SourceEngine;
    use crate::net::ReplayTier;

    const FIXED_VECTOR_PLAIN: &str = r#"{"id":106164,"chapterid":1}"#;
    const FIXED_VECTOR_TOKEN: &str = "J4RNzsyCMs99KGUkm9Ir6UE4n/y1YYqn5v6BscyNY18=";

    fn sample_source() -> BookSource {
        builtin_source()
    }

    #[test]
    fn token_matches_site_vector() {
        assert_eq!(encrypt_token(FIXED_VECTOR_PLAIN).unwrap(), FIXED_VECTOR_TOKEN);
        assert_eq!(
            encode_component(FIXED_VECTOR_TOKEN),
            "J4RNzsyCMs99KGUkm9Ir6UE4n%2Fy1YYqn5v6BscyNY18%3D"
        );
    }

    #[test]
    fn compact_json_keeps_js_key_order() {
        assert_eq!(token_chapter(106164, 1), FIXED_VECTOR_PLAIN);
        assert_eq!(token_id(135539), r#"{"id":135539}"#);
    }

    #[test]
    fn detects_bqg_source_url() {
        let mut source = sample_source();
        assert!(is_bqg_source(&source));
        source.book_source_url = "https://www.example.com".into();
        assert!(!is_bqg_source(&source));
    }

    #[test]
    fn parses_hash_book_and_chapter_urls() {
        assert_eq!(
            parse_book_id("https://www.bqg616.cc/#/book/106164/").unwrap(),
            106164
        );
        assert_eq!(
            parse_chapter_ref("https://www.bqg616.cc/#/book/106164/3.html").unwrap(),
            (106164, 3)
        );
        assert!(parse_chapter_ref("https://www.bqg616.cc/#/book/106164/").is_err());
    }

    #[test]
    fn toc_uses_last_chapter_id_when_list_is_short() {
        assert_eq!(expected_total(2, 5), 5);
        assert_eq!(expected_total(10, 3), 10);
        assert_eq!(expected_total(0, 0), 0);
    }

    #[test]
    fn builtin_source_deserializes() {
        let source = builtin_source();
        assert_eq!(source.book_source_name, "笔趣阁 616");
        assert!(is_bqg_source(&source));
    }

    #[test]
    fn detects_site_bait_chapter() {
        let bait = "我今年２２岁，是一个大学阿拉伯语专业的学生，全班一共３０人。\
而我是典型东北大汉的身材，身高１８２，典型的０６１８黄金身材，自然也就成了我们学院的院草。";
        assert!(looks_like_bait(bait));
        assert!(!looks_like_bait("霍雨浩沉默片刻，王冬儿的第五魂技终于发动。"));
        assert!(!looks_like_bait("宴桃园豪杰三结义，斩黄巾英雄首立功。"));
    }

    fn replay_pages() -> ReplayTier {
        let mut pages = ReplayTier::empty();
        pages.insert(
            "/api/search",
            r#"{"data":[{"id":1,"title":"公版样例","author":"佚名","sortname":"古典","remark":"离线夹具","img":"/cover/1.jpg","lastchapter":"第二章","lastchapterid":3}]}"#,
        );
        pages.insert(
            "/api/book",
            r#"{"id":1,"title":"公版样例","author":"佚名","dirid":1,"lastchapterid":3}"#,
        );
        pages.insert("/api/booklist", r#"{"list":["第一章","第二章"]}"#);
        pages.insert(
            "/api/chapter",
            r#"{"chaptername":"第一章","txt":"第一段\n第二段","cs":3}"#,
        );
        pages
    }

    #[tokio::test]
    async fn adapter_runs_on_offline_replay() {
        let pages = replay_pages();
        let source = sample_source();

        let books = search(&pages, &source, "样例").await.unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].name, "公版样例");
        assert_eq!(books[0].author, "佚名");
        assert_eq!(books[0].kind, "古典");
        assert_eq!(books[0].book_url, "https://www.bqg616.cc/#/book/1/");
        assert_eq!(books[0].cover_url, "https://www.bqg616.cc/cover/1.jpg");
        assert_eq!(books[0].last_chapter, "第二章");

        let chapters = toc(&pages, &source, &books[0].book_url).await.unwrap();
        assert_eq!(chapters.len(), 3, "list=2 且 lastchapterid=3，应取较大值");
        assert_eq!(chapters[0].name, "第一章");
        assert_eq!(chapters[2].name, "第3章");
        assert_eq!(
            chapters[1].url,
            "https://www.bqg616.cc/#/book/1/2.html"
        );

        let text = content(&pages, &source, &chapters[0].url).await.unwrap();
        assert_eq!(text, "第一段\n第二段");
    }

    #[tokio::test]
    async fn source_engine_dispatches_bqg() {
        let js = PassThroughJs;
        let pages = replay_pages();
        let engine = SourceEngine::new(&pages, &js);
        let source = sample_source();

        let books = engine.search(&source, "样例").await.unwrap();
        assert_eq!(books[0].name, "公版样例");
        let chapters = engine.toc(&source, &books[0].book_url).await.unwrap();
        assert_eq!(chapters.len(), 3);
        let text = engine.content(&source, &chapters[0].url).await.unwrap();
        assert!(text.contains("第一段"));
    }
}

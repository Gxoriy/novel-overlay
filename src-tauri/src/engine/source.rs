use anyhow::{anyhow, Context, Result};
use url::Url;

use crate::engine::js::{JsEvaluator, JsVars};
use crate::model::{BookSource, Chapter, SearchBook};
use crate::net::PageSource;
use crate::parse::rule::{self, EvalEnv, RuleChain};

/// 书源执行器：把「规则 + 取页面 + JS」组装成 search / toc / content 三个动作。
///
/// 依赖 [`PageSource`] 而不是具体的 HTTP 客户端，所以整条链路可以在离线回放上
/// 端到端跑通，合规闸门/限速/熔断/回放对这里完全透明。
pub struct SourceEngine<'a> {
    pages: &'a dyn PageSource,
    js: &'a dyn JsEvaluator,
}

impl<'a> SourceEngine<'a> {
    pub fn new(pages: &'a dyn PageSource, js: &'a dyn JsEvaluator) -> Self {
        Self { pages, js }
    }

    pub async fn search(&self, source: &BookSource, keyword: &str) -> Result<Vec<SearchBook>> {
        if crate::engine::bqg::is_bqg_source(source) {
            return crate::engine::bqg::search(self.pages, source, keyword).await;
        }
        let headers = source.headers()?;
        let markers = source.challenge_markers();
        let base = EvalEnv {
            js: self.js,
            base_url: &source.book_source_url,
        };
        let request = build_search_request(source, keyword, &base).await?;
        let html = match request.body.as_deref() {
            Some(body) => {
                self.pages
                    .post_text(&request.url, body, &headers, markers)
                    .await?
            }
            None => self.pages.get_text(&request.url, &headers, markers).await?,
        };

        // 字段规则相对搜索结果页求值，baseUrl 指向该页。
        let page = EvalEnv {
            js: self.js,
            base_url: &request.url,
        };

        let list_chain = RuleChain::parse(&source.rule_search.book_list)?;
        let name_chain = optional_chain(source.rule_search.name.as_deref())?;
        let author_chain = optional_chain(source.rule_search.author.as_deref())?;
        let kind_chain = optional_chain(source.rule_search.kind.as_deref())?;
        let intro_chain = optional_chain(source.rule_search.intro.as_deref())?;
        let cover_chain = optional_chain(source.rule_search.cover_url.as_deref())?;
        let book_url_chain = optional_chain(source.rule_search.book_url.as_deref())?;
        let last_chapter_chain = optional_chain(source.rule_search.last_chapter.as_deref())?;

        let items = rule::eval(&html, &list_chain, &page).await?;
        let mut results = Vec::new();
        for item in items {
            let name = pick_element(&item, name_chain.as_ref(), &page).await?;
            if name.is_empty() {
                continue;
            }
            let raw_book_url = pick_element(&item, book_url_chain.as_ref(), &page).await?;
            let book_url = if raw_book_url.is_empty() {
                request.url.clone()
            } else {
                resolve(&request.url, &raw_book_url).unwrap_or(raw_book_url)
            };
            let raw_cover = pick_element(&item, cover_chain.as_ref(), &page).await?;
            let cover_url = if raw_cover.is_empty() {
                String::new()
            } else {
                resolve(&request.url, &raw_cover).unwrap_or(raw_cover)
            };
            results.push(SearchBook {
                source_id: 0,
                source_name: String::new(),
                name,
                author: pick_element(&item, author_chain.as_ref(), &page).await?,
                kind: pick_element(&item, kind_chain.as_ref(), &page).await?,
                intro: pick_element(&item, intro_chain.as_ref(), &page).await?,
                cover_url,
                book_url,
                last_chapter: pick_element(&item, last_chapter_chain.as_ref(), &page).await?,
            });
        }
        Ok(results)
    }

    pub async fn toc(&self, source: &BookSource, book_url: &str) -> Result<Vec<Chapter>> {
        if crate::engine::bqg::is_bqg_source(source) {
            return crate::engine::bqg::toc(self.pages, source, book_url).await;
        }
        let headers = source.headers()?;
        let markers = source.challenge_markers();
        let detail_html = self.pages.get_text(book_url, &headers, markers).await?;

        let detail_env = EvalEnv {
            js: self.js,
            base_url: book_url,
        };
        let toc_url_chain = match &source.rule_book_info {
            Some(info) => optional_chain(info.toc_url.as_deref())?,
            None => None,
        };
        let raw_toc_url = pick_page(&detail_html, toc_url_chain.as_ref(), &detail_env).await?;
        let toc_page = if raw_toc_url.is_empty() {
            book_url.to_string()
        } else {
            resolve(book_url, &raw_toc_url)?
        };

        let toc_html = if toc_page == book_url {
            detail_html
        } else {
            self.pages.get_text(&toc_page, &headers, markers).await?
        };
        let toc_env = EvalEnv {
            js: self.js,
            base_url: &toc_page,
        };

        let list_chain = RuleChain::parse(&source.rule_toc.chapter_list)?;
        let name_chain = RuleChain::parse(&source.rule_toc.chapter_name)?;
        let url_chain = RuleChain::parse(&source.rule_toc.chapter_url)?;

        let items = rule::eval(&toc_html, &list_chain, &toc_env).await?;
        let mut chapters: Vec<Chapter> = Vec::new();
        for item in items {
            let name = first_non_empty(rule::eval_element(&item, &name_chain, &toc_env).await?);
            let raw_url = first_non_empty(rule::eval_element(&item, &url_chain, &toc_env).await?);
            if name.is_empty() || raw_url.is_empty() {
                continue;
            }
            let url = match resolve(&toc_page, &raw_url) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if chapters.iter().any(|existing| existing.url == url) {
                continue;
            }
            let index = chapters.len() as i64;
            chapters.push(Chapter { index, name, url });
        }
        if chapters.is_empty() {
            return Err(anyhow!(
                "目录规则没有匹配到章节，检查 ruleToc.chapterList / chapterName / chapterUrl"
            ));
        }
        Ok(chapters)
    }

    pub async fn content(&self, source: &BookSource, chapter_url: &str) -> Result<String> {
        if crate::engine::bqg::is_bqg_source(source) {
            return crate::engine::bqg::content(self.pages, source, chapter_url).await;
        }
        let headers = source.headers()?;
        let markers = source.challenge_markers();
        let html = self.pages.get_text(chapter_url, &headers, markers).await?;
        let env = EvalEnv {
            js: self.js,
            base_url: chapter_url,
        };
        let chain = RuleChain::parse(&source.rule_content.content)?;
        let parts = rule::eval(&html, &chain, &env).await?;
        let joined = parts
            .into_iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        // 规则若以节点选择收尾，这里会把残留标签洗掉。
        let text = rule::html_to_lines(&joined);
        if text.is_empty() {
            return Err(anyhow!("正文规则没有匹配到内容，检查 ruleContent.content"));
        }
        Ok(text)
    }
}

fn optional_chain(raw: Option<&str>) -> Result<Option<RuleChain>> {
    match raw {
        Some(value) if !value.trim().is_empty() => Ok(Some(RuleChain::parse(value)?)),
        _ => Ok(None),
    }
}

/// 对**列表项片段**求值一条字段规则，取第一条非空结果。
async fn pick_element(html: &str, chain: Option<&RuleChain>, env: &EvalEnv<'_>) -> Result<String> {
    match chain {
        None => Ok(String::new()),
        Some(chain) => Ok(first_non_empty(rule::eval_element(html, chain, env).await?)),
    }
}

/// 对**整页**求值一条规则，取第一条非空结果。
async fn pick_page(html: &str, chain: Option<&RuleChain>, env: &EvalEnv<'_>) -> Result<String> {
    match chain {
        None => Ok(String::new()),
        Some(chain) => Ok(first_non_empty(rule::eval(html, chain, env).await?)),
    }
}

fn first_non_empty(values: Vec<String>) -> String {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

/// 拼好的搜索请求：地址 + 可选的 POST 请求体。
#[derive(Debug)]
struct SearchRequest {
    url: String,
    /// `Some` 表示用 POST 发送（书源配置了 `searchBody`）。
    body: Option<String>,
}

async fn build_search_request(
    source: &BookSource,
    keyword: &str,
    env: &EvalEnv<'_>,
) -> Result<SearchRequest> {
    let base = Url::parse(&source.book_source_url)
        .with_context(|| format!("书源地址非法: {}", source.book_source_url))?;
    // `{{gbk:key}}`：老站（笔趣阁系）按 GBK 匹配关键词，UTF-8 编码过去是「查无此书」。
    // `{{urlencode:key}}`：POST 表单体里中文必须百分号编码，原样 UTF-8 字节会被
    // 服务端当「没有输入」（表现为永远返回「请输入搜索词」的空结果页）。
    // 先替换带花括号的完整写法，再替换单花括号写法，避免残留 `}`。
    let substitute = |template: &str| -> String {
        if template.contains("urlencode:key") {
            let encoded = form_urlencode_utf8(keyword);
            return template
                .replace("{{urlencode:key}}", &encoded)
                .replace("{urlencode:key}", &encoded);
        }
        if template.contains("gbk:key") {
            let encoded = encode_query_component_gbk(keyword);
            template
                .replace("{{gbk:key}}", &encoded)
                .replace("{gbk:key}", &encoded)
        } else if template.contains("{{key}}") {
            template.replace("{{key}}", keyword)
        } else if template.contains("{key}") {
            template.replace("{key}", keyword)
        } else {
            template.replace("searchKey", keyword)
        }
    };
    let url_template = substitute(&source.search_url);
    let url_template = render_js_blocks(&url_template, env, keyword).await?;
    let url = base
        .join(&url_template)
        .or_else(|_| Url::parse(&url_template))
        .with_context(|| format!("无法拼接搜索地址: {}", source.search_url))?;

    // 请求体也走同一套占位符替换与 js 求值。
    let body = match source.search_body.as_deref() {
        None => None,
        Some(template) => {
            let substituted = substitute(template);
            Some(render_js_blocks(&substituted, env, keyword).await?)
        }
    };
    Ok(SearchRequest {
        url: url.to_string(),
        body,
    })
}

/// 把关键词按 GBK 编码再做百分号编码，供 `{{gbk:key}}` 使用。
///
/// GBK 表示不了的字符会被替换成 `?` —— 这类关键词在老站本来就搜不到，
/// 不必为此报错打断整次搜索。
fn encode_query_component_gbk(keyword: &str) -> String {
    let (bytes, _, _) = encoding_rs::GBK.encode(keyword);
    percent_encode(&bytes)
}

/// UTF-8 字节的表单百分号编码，供 `{{urlencode:key}}` 使用。
fn form_urlencode_utf8(keyword: &str) -> String {
    percent_encode(keyword.as_bytes())
}

/// 非 URL 保留字符逐字节转 `%XX`。
fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 把字符串里的 `<js>...</js>` 块求值后替换掉。searchUrl 常用它拼签名参数。
async fn render_js_blocks(text: &str, env: &EvalEnv<'_>, keyword: &str) -> Result<String> {
    if !text.contains("<js>") {
        return Ok(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<js>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        let end = after
            .find("</js>")
            .ok_or_else(|| anyhow!("书源里的 `<js>` 没有闭合"))?;
        let script = &after[..end];
        let vars = JsVars::new()
            .set("key", keyword)
            .set("result", keyword)
            .set("baseUrl", env.base_url);
        out.push_str(&env.js.eval(script, vars).await?);
        rest = &after[end + 5..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 把规则里取到的相对地址补成绝对地址。
pub fn resolve(base: &str, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("地址为空"));
    }
    let base_url = Url::parse(base).with_context(|| format!("基准地址非法: {base}"))?;
    Ok(base_url
        .join(trimmed)
        .with_context(|| format!("无法解析地址: {trimmed}"))?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::crypto;
    use crate::engine::quickjs::QuickJsEvaluator;
    use crate::model::{RuleContent, RuleSearch, RuleToc};
    use crate::net::{Ladder, PageSource};

    const ROOT: &str = "https://www.example.com";

    fn source(search_url: &str) -> BookSource {
        BookSource {
            book_source_name: "测试源".to_string(),
            book_source_url: ROOT.to_string(),
            enabled: true,
            search_url: search_url.to_string(),
            search_body: None,
            rule_search: RuleSearch {
                book_list: "css:div.item".to_string(),
                name: None,
                author: None,
                kind: None,
                intro: None,
                cover_url: None,
                book_url: None,
                last_chapter: None,
            },
            rule_book_info: None,
            rule_toc: RuleToc {
                chapter_list: "css:a".to_string(),
                chapter_name: "text".to_string(),
                chapter_url: "attr:href".to_string(),
            },
            rule_content: RuleContent {
                content: "css:div#content@br".to_string(),
            },
            header: None,
            challenge_markers: None,
        }
    }

    fn env<'a>(js: &'a QuickJsEvaluator) -> EvalEnv<'a> {
        EvalEnv { js, base_url: ROOT }
    }

    #[tokio::test]
    async fn search_url_substitutes_keyword_placeholder() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let request = build_search_request(&source("/search?q={{key}}"), "斗破苍穹", &env(&js))
            .await
            .unwrap();
        assert!(request.body.is_none());
        // `Url::join` 会对中文做百分号编码，这里只断言结构与编码结果。
        assert!(
            request.url.starts_with("https://www.example.com/search?q=%E6%96%97"),
            "实际地址: {}",
            request.url
        );
    }

    /// 老站按 GBK 匹配关键词，UTF-8 编码过去只会返回「查无此书」。
    #[tokio::test]
    async fn search_url_encodes_keyword_as_gbk() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let request = build_search_request(
            &source("/search.php?searchkey={{gbk:key}}"),
            "斗破苍穹",
            &env(&js),
        )
        .await
        .unwrap();
        assert!(
            request
                .url
                .ends_with("searchkey=%B6%B7%C6%C6%B2%D4%F1%B7"),
            "实际地址: {}",
            request.url
        );

        let single = build_search_request(&source("/s?q={gbk:key}"), "斗破苍穹", &env(&js))
            .await
            .unwrap();
        assert!(
            single.url.ends_with("q=%B6%B7%C6%C6%B2%D4%F1%B7"),
            "实际地址: {}",
            single.url
        );
    }

    /// 配置了 `searchBody` 即用 POST，且请求体里的占位符同样会被替换。
    #[tokio::test]
    async fn search_body_is_substituted_and_marks_post() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let mut src = source("/search.html");
        src.search_body = Some("s={{key}}&token=<js>'tk'</js>".to_string());

        let request = build_search_request(&src, "斗破苍穹", &env(&js))
            .await
            .unwrap();
        assert_eq!(request.url, "https://www.example.com/search.html");
        assert_eq!(request.body.as_deref(), Some("s=斗破苍穹&token=tk"));
    }

    /// POST 表单体里的中文必须百分号编码：原样 UTF-8 字节会被服务端当成
    /// 「没有输入」，永远返回空结果页。
    #[tokio::test]
    async fn search_body_supports_urlencode_placeholder() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let mut src = source("/search.html");
        src.search_body = Some("s={{urlencode:key}}".to_string());

        let request = build_search_request(&src, "斗破苍穹", &env(&js))
            .await
            .unwrap();
        assert_eq!(request.body.as_deref(), Some("s=%E6%96%97%E7%A0%B4%E8%8B%8D%E7%A9%B9"));
    }

    #[tokio::test]
    async fn search_url_accepts_legacy_placeholder_forms() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let env = env(&js);
        for template in ["/s?k={key}", "/s?k=searchKey"] {
            let request = build_search_request(&source(template), "abc", &env)
                .await
                .unwrap();
            assert!(
                request.url.contains("k=abc"),
                "模板 {template} 产出 {}",
                request.url
            );
        }
    }

    /// 本轮的核心能力：`searchUrl` 里内嵌 `<js>` 现算签名参数。
    #[tokio::test]
    async fn search_url_runs_embedded_js_signature() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let template = "/search?q={{key}}&sign=<js>md5(key + '|' + baseUrl)</js>";
        let request = build_search_request(&source(template), "斗破苍穹", &env(&js))
            .await
            .unwrap();

        let expected = crypto::md5_hex(&format!("斗破苍穹|{ROOT}"));
        assert!(
            request.url.contains(&format!("sign={expected}")),
            "实际地址: {}",
            request.url
        );
    }

    /// 签名脚本能看到 `baseUrl`，因此站点换域名时不用改规则。
    #[tokio::test]
    async fn embedded_js_can_read_base_url() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let template = "/s?host=<js>document.location.host</js>";
        let request = build_search_request(&source(template), "x", &env(&js))
            .await
            .unwrap();
        assert!(
            request.url.contains("host=www.example.com"),
            "实际地址: {}",
            request.url
        );
    }

    #[tokio::test]
    async fn unclosed_js_block_is_reported() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let error = build_search_request(&source("/s?q=<js>md5(key)"), "x", &env(&js))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("没有闭合"), "实际错误: {error}");
    }

    /// 演示模板里的写法：第一个 `<js>` 算出时间戳存进 `java`，第二个 `<js>` 取出来算签名。
    /// 这依赖 QuickJS Context 在多次求值之间长期存活（同一线程复用同一个 Context）。
    #[tokio::test]
    async fn embedded_js_blocks_share_java_kv_state() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let template = "/api/search?q={{key}}&t=<js>var t = Math.floor(Date.now() / 1000); \
                        java.put('t', t); t</js>&sign=<js>md5(key + '|' + java.get('t') + '|SALT')</js>";
        let request = build_search_request(&source(template), "斗破苍穹", &env(&js))
            .await
            .unwrap();
        let url = request.url;

        let stamp = url
            .split("&t=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("地址里应带 t 参数");
        let sign = url.split("&sign=").nth(1).expect("地址里应带 sign 参数");

        assert_eq!(sign, crypto::md5_hex(&format!("斗破苍穹|{stamp}|SALT")));
    }

    /// 仓库里带出去的示例书源必须能真的被 schema 解析，否则等于发了个假模板。
    #[test]
    fn shipped_demo_sources_deserialize() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources");
        for name in [
            "demo-source.json",
            "demo-source-js-signature.json",
            "bqg616.json",
        ] {
            let path = dir.join(name);
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("读取 {name} 失败: {error}"));
            let parsed: BookSource = serde_json::from_str(&raw)
                .unwrap_or_else(|error| panic!("{name} 不符合书源 schema: {error}"));
            assert!(!parsed.book_source_name.is_empty(), "{name} 缺少书源名");
            // 规则链必须能编译，否则导入后要到运行时才炸。
            for rule in [
                &parsed.rule_search.book_list,
                &parsed.rule_toc.chapter_list,
                &parsed.rule_toc.chapter_name,
                &parsed.rule_toc.chapter_url,
                &parsed.rule_content.content,
            ] {
                RuleChain::parse(rule).unwrap_or_else(|error| panic!("{name} 规则 `{rule}` 非法: {error}"));
            }
        }
    }

    #[test]
    fn resolve_makes_relative_urls_absolute() {
        assert_eq!(
            resolve("https://www.example.com/book/1", "/read/2").unwrap(),
            "https://www.example.com/read/2"
        );
        assert_eq!(
            resolve("https://www.example.com/book/1", "2.html").unwrap(),
            "https://www.example.com/book/2.html"
        );
        assert!(resolve("https://www.example.com", "  ").is_err());
    }

    #[test]
    fn header_json_becomes_request_headers() {
        let mut src = source("/s?q={key}");
        src.header = Some(r#"{"Referer":"https://www.example.com","X-Token":"t1"}"#.to_string());
        let headers = src.headers().unwrap();
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().any(|(k, v)| k == "Referer" && v == ROOT));

        src.header = Some("not-json".to_string());
        assert!(src.headers().is_err());
    }

    // ---------- 端到端：书源 JSON → 搜索 → 目录 → 正文 ----------

    fn fixture_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay")
    }

    /// 夹具里的目录页规则必须能选中两个章节 —— 端到端失败时先看这个。
    #[tokio::test]
    async fn fixture_toc_rules_select_both_chapters() {
        let html = std::fs::read_to_string(fixture_dir().join("toc.html")).unwrap();
        let js = crate::engine::js::PassThroughJs;
        let env = EvalEnv {
            js: &js,
            base_url: "https://fixture.test/book/1/toc",
        };

        let list = RuleChain::parse("css:div#chapter-list@css:a").unwrap();
        let items = rule::eval(&html, &list, &env).await.unwrap();
        assert_eq!(items.len(), 2, "章节列表规则结果: {items:?}");
        assert!(items[0].contains("/read/1.html"), "实际: {}", items[0]);

        let name = RuleChain::parse("text").unwrap();
        assert_eq!(
            rule::eval_element(&items[0], &name, &env).await.unwrap()[0],
            "第一章 陨落的天才"
        );

        // `attr:href` 必须走元素级入口：页面级入口的初始节点是解析器补出来的
        // `<html>`，读它的 `href` 只会静默返回空串。
        let url = RuleChain::parse("attr:href").unwrap();
        assert_eq!(
            rule::eval_element(&items[0], &url, &env).await.unwrap()[0],
            "/read/1.html"
        );
        assert_eq!(
            rule::eval(&items[0], &url, &env).await.unwrap()[0],
            "",
            "页面级入口在片段上取属性会返回空串，这正是引入 Entry::Element 的原因"
        );
    }

    /// 详情页里的 tocUrl 规则。
    #[tokio::test]
    async fn fixture_book_info_toc_url_is_extracted() {
        let html = std::fs::read_to_string(fixture_dir().join("book.html")).unwrap();
        let js = crate::engine::js::PassThroughJs;
        let env = EvalEnv {
            js: &js,
            base_url: "https://fixture.test/book/1",
        };
        let chain = RuleChain::parse("css:a.toc-link@attr:href").unwrap();
        let out = rule::eval(&html, &chain, &env).await.unwrap();
        assert_eq!(out, vec!["/book/1/toc".to_string()]);
    }

    /// 整条采集链路跑在离线回放上，一个网络请求都不发。
    ///
    /// 这个测试是「采集内核可测」这条主张的证据：书源 JSON、规则引擎、内嵌 JS、
    /// 分级阶梯、回放层全部串起来跑一遍，且完全确定。
    #[tokio::test]
    async fn full_chain_runs_on_offline_replay() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let dir = fixture_dir();

        let raw = std::fs::read_to_string(dir.join("source.json")).expect("夹具书源应存在");
        let source: BookSource = serde_json::from_str(&raw).expect("夹具书源应符合 schema");
        assert_eq!(source.challenge_markers(), ["fixture-wall".to_string()]);

        let ladder = Ladder::replay_only(&[dir]).expect("回放层应能加载");
        assert_eq!(ladder.describe(), "分级阶梯 [L3 离线回放]");
        let engine = SourceEngine::new(&ladder, &js);

        // --- 搜索 ---
        let books = engine.search(&source, "斗破苍穹").await.unwrap();
        assert_eq!(books.len(), 2, "搜索结果应有两本");
        assert_eq!(books[0].name, "斗破苍穹");
        assert_eq!(books[0].author, "天蚕土豆");
        assert_eq!(books[0].kind, "玄幻");
        assert_eq!(books[0].last_chapter, "第二章 斗之气");
        assert_eq!(books[0].book_url, "https://fixture.test/book/1");
        assert_eq!(books[0].cover_url, "https://fixture.test/img/1.jpg");
        assert_eq!(books[1].name, "凡人修仙传");

        // --- 目录：先取详情页里的 tocUrl，再抓目录页 ---
        let chapters = engine.toc(&source, &books[0].book_url).await.unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].name, "第一章 陨落的天才");
        assert_eq!(chapters[0].url, "https://fixture.test/read/1.html");
        assert_eq!(chapters[1].name, "第二章 斗之气");

        // --- 正文 ---
        let text = engine.content(&source, &chapters[0].url).await.unwrap();
        assert!(text.contains("斗之力，三段！"), "实际正文: {text}");
        assert!(text.contains("紧握的手掌"), "实际正文: {text}");
        assert!(
            text.lines().count() >= 3,
            "`br` 应把 <br/> 折成换行，实际 {} 行: {text}",
            text.lines().count()
        );
        assert!(
            !text.contains("仅供离线回放测试使用"),
            "规则只取 div#content，不该把页脚串进来: {text}"
        );
    }

    /// 回放层缺页面时，错误信息要能直接指出「哪个路径没登记」。
    #[tokio::test]
    async fn replay_miss_reports_the_missing_path() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay");
        let raw = std::fs::read_to_string(dir.join("source.json")).unwrap();
        let mut source: BookSource = serde_json::from_str(&raw).unwrap();

        let ladder = Ladder::replay_only(&[dir]).unwrap();
        let engine = SourceEngine::new(&ladder, &js);

        // 把搜索地址指到一个夹具里没有的路径。
        source.search_url = "/no-such-page.php?q={{key}}".to_string();
        let message = engine
            .search(&source, "任意")
            .await
            .unwrap_err()
            .to_string();

        assert!(message.contains("/no-such-page.php"), "实际: {message}");
        assert!(message.contains("离线回放"), "实际: {message}");
    }
}

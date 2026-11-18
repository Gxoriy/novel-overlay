//! 规则管道语法。
//!
//! 一条规则由若干步骤用 `@` 串联，从左到右依次求值：
//!
//! | 步骤 | 作用 |
//! |---|---|
//! | `css:<selector>` | 在当前节点集合里选出所有匹配的子孙节点（无前缀时按 css 处理） |
//! | `index:<n>` | 取第 n 个（节点或字符串） |
//! | `skip:<n>` | 丢掉前 n 个（节点或字符串），用于跳过「最新章节」之类的重复前缀 |
//! | `text` | 取节点纯文本，折叠连续空白 |
//! | `html` | 取节点 inner HTML（保留标签） |
//! | `attr:<name>` | 取节点属性值 |
//! | `br` | HTML 转带换行的纯文本：`<br>`/`</p>`/`</div>` → 换行，去标签，解实体，压空行 |
//! | `regex:<pattern>` | 正则提取，优先取捕获组 1，否则取整个匹配 |
//! | `trim` | 去掉首尾空白 |
//! | `js:<code>` 或 `<js>code</js>` | 交给内嵌 QuickJS 执行，当前值以 `result` 注入 |
//!
//! 求值约束：节点类步骤（`css:` / `index:` / `text` / `html` / `attr:`）必须排在
//! 字符串类步骤之前；一旦进入字符串阶段，就不能再用 `css:` 回到节点。
//!
//! `<js>` 块内部的 `@` 不会切断规则链——站点签名代码里出现 `@` 很常见。

use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use html_escape::decode_html_entities;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

use crate::engine::js::{JsEvaluator, JsVars};

static BR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<\s*br\s*/?\s*>").unwrap());
static BLOCK_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</\s*(p|div|h[1-6]|li|tr|section|article|dd|dt)\s*>").unwrap());
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static BODY_SELECTOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("body").expect("`body` 必定是合法选择器"));

/// 求值环境：JS 执行器 + 当前页面地址（脚本里以 `baseUrl` 可见）。
pub struct EvalEnv<'a> {
    pub js: &'a dyn JsEvaluator,
    pub base_url: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Css(String),
    Text,
    Html,
    Attr(String),
    Regex(String),
    Index(usize),
    /// 丢掉前 n 个：目录页常把「最新章节」先列一遍，正文从后面才开始。
    Skip(usize),
    Br,
    Trim,
    Js(String),
}

#[derive(Debug, Clone)]
pub struct RuleChain {
    steps: Vec<Step>,
    source: String,
}

impl RuleChain {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut steps = Vec::new();
        for part in split_steps(raw) {
            let step = if let Some(rest) = part.strip_prefix("css:") {
                Step::Css(rest.trim().to_string())
            } else if let Some(rest) = part.strip_prefix("regex:") {
                Step::Regex(rest.to_string())
            } else if let Some(rest) = part.strip_prefix("attr:") {
                Step::Attr(rest.trim().to_string())
            } else if let Some(rest) = part.strip_prefix("index:") {
                let value = rest
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| anyhow!("`index:` 参数不是非负整数: {rest}"))?;
                Step::Index(value)
            } else if let Some(rest) = part.strip_prefix("skip:") {
                let value = rest
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| anyhow!("`skip:` 参数不是非负整数: {rest}"))?;
                Step::Skip(value)
            } else if let Some(rest) = part.strip_prefix("js:") {
                Step::Js(rest.to_string())
            } else if let Some(inner) = js_block(&part) {
                Step::Js(inner.to_string())
            } else {
                match part.as_str() {
                    "text" => Step::Text,
                    "html" => Step::Html,
                    "br" => Step::Br,
                    "trim" => Step::Trim,
                    _ => Step::Css(part),
                }
            };
            steps.push(step);
        }
        if steps.is_empty() {
            return Err(anyhow!("规则为空"));
        }
        Ok(Self {
            steps,
            source: raw.to_string(),
        })
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

/// 按 `@` 切分步骤，但跳过 `<js>...</js>` 内部（签名代码里常有 `@`）。
fn split_steps(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut js_depth = 0usize;
    let mut rest = raw;

    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("<js>") {
            js_depth += 1;
            current.push_str("<js>");
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix("</js>") {
            js_depth = js_depth.saturating_sub(1);
            current.push_str("</js>");
            rest = tail;
            continue;
        }
        let ch = rest.chars().next().expect("rest 非空时必有字符");
        if ch == '@' && js_depth == 0 {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
        rest = &rest[ch.len_utf8()..];
    }
    parts.push(current);

    parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn js_block(part: &str) -> Option<&str> {
    part.trim().strip_prefix("<js>")?.strip_suffix("</js>")
}

/// 求值入口，决定初始节点集合。
///
/// 这个区分是必须的：`Html::parse_document` 会把片段包进
/// `<html><head></head><body>…</body></html>`，所以 `root_element()` 拿到的是
/// `<html>` 而不是片段元素本身。`css:` 开头的规则能靠导航绕过去，但 `attr:href`
/// 这类直接取值的规则会去读 `<html>` 的属性，**静默返回空串**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// 输入是整页 HTML，初始节点是文档根元素。
    Page,
    /// 输入是单个元素的序列化结果（列表项），初始节点是该元素本身。
    Element,
}

/// 页面级求值。
pub async fn eval(html: &str, chain: &RuleChain, env: &EvalEnv<'_>) -> Result<Vec<String>> {
    eval_with(html, chain, env, Entry::Page).await
}

/// 片段以表格结构标签开头时补一层 `<table><tbody>` 外壳。
///
/// `Html::parse_document` 走的是 `in body` 插入模式，遇到 `tr` / `td` / `th` /
/// `tbody` 这类只在表格里合法的标签会按 parse error **直接丢弃**。表格型搜索结果的
/// 列表项正好是一整行 `<tr>`，不补外壳的话片段解析出来 body 是空的，
/// 书源表现为「搜索 0 条」——不报错，最难查。
fn table_fragment_wrapper(html: &str) -> Option<String> {
    const TABLE_TAGS: [&str; 9] = [
        "tr", "td", "th", "tbody", "thead", "tfoot", "caption", "colgroup", "col",
    ];
    let rest = html.trim_start().strip_prefix('<')?;
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if TABLE_TAGS.contains(&name.as_str()) {
        Some(format!("<table><tbody>{html}</tbody></table>"))
    } else {
        None
    }
}

/// 元素级求值：把规则作用在一个列表项片段上。
pub async fn eval_element(
    html: &str,
    chain: &RuleChain,
    env: &EvalEnv<'_>,
) -> Result<Vec<String>> {
    eval_with(html, chain, env, Entry::Element).await
}

/// 求值，返回所有匹配结果（调用方自行取第一条）。
///
/// # 为什么要把求值拆成两段
///
/// `scraper::Html` 内部是 html5ever 的 `tendril::Tendril`，而 `Tendril` 是 `!Send`。
/// 只要 `Html` 或 `ElementRef` 活过任何一个 `.await`，整个 future 就不再满足 `Send`，
/// Tauri 的多线程 runtime 会直接拒绝编译。
///
/// 所以这里按"节点阶段 / 字符串阶段"切两段：前一段同步跑完 DOM 操作并把结果落成
/// `Vec<String>`，作用域出口只留下 `Send` 的字符串；后一段才允许 `await`（`js:` 规则）。
///
/// 分界点取"第一个既不是 `css:` 也不是 `index:` 的步骤"——在那之前所有步骤都只碰节点，
/// 那一步负责消费节点集合、产出字符串。
async fn eval_with(
    html: &str,
    chain: &RuleChain,
    env: &EvalEnv<'_>,
    entry: Entry,
) -> Result<Vec<String>> {
    let steps = chain.steps();
    let split = steps
        .iter()
        .position(|step| !matches!(step, Step::Css(_) | Step::Index(_) | Step::Skip(_)))
        .unwrap_or(steps.len());

    // 表格行片段（表格型搜索结果的每一行）要先补外壳，否则会被 html5ever 丢掉。
    let wrapped = match entry {
        Entry::Element => table_fragment_wrapper(html),
        Entry::Page => None,
    };
    let html = wrapped.as_deref().unwrap_or(html);

    // ---- 阶段一：节点阶段（同步，可安全持有 ElementRef）----
    let mut strings: Vec<String> = {
        let doc = Html::parse_document(html);
        let root = doc.root_element();

        let mut nodes: Vec<ElementRef<'_>> = match entry {
            Entry::Page => vec![root],
            Entry::Element => {
                // 片段模式下 `<body>` 的直接子元素就是"当前节点"。
                // `children()` 给的是 NodeRef，需要 wrap 成 ElementRef 并滤掉文本节点。
                let children: Vec<ElementRef<'_>> = root
                    .select(&BODY_SELECTOR)
                    .next()
                    .map(|body| {
                        body.children()
                            .filter_map(ElementRef::wrap)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if children.is_empty() {
                    vec![root]
                } else {
                    children
                }
            }
        };
        for step in &steps[..split] {
            match step {
                Step::Css(selector) => {
                    let compiled = Selector::parse(selector)
                        .map_err(|err| anyhow!("选择器解析失败 `{selector}`: {err:?}"))?;
                    let mut next = Vec::new();
                    for node in &nodes {
                        for matched in node.select(&compiled) {
                            next.push(matched);
                        }
                    }
                    nodes = next;
                }
                Step::Index(index) => {
                    nodes = nodes.get(*index).cloned().into_iter().collect();
                }
                Step::Skip(count) => {
                    nodes = nodes.into_iter().skip(*count).collect();
                }
                _ => unreachable!("节点阶段只会出现 `css:` / `index:` / `skip:`"),
            }
        }

        if split == steps.len() {
            // 规则以节点选择收尾：返回节点自身的 HTML，供列表项逐条再求值。
            return Ok(nodes.iter().map(|node| node.html()).collect());
        }

        // 分界步骤：把节点集合消费成字符串集合。
        match &steps[split] {
            Step::Text => nodes
                .iter()
                .map(|node| clean_text(&node.text().collect::<String>()))
                .collect(),
            Step::Html => nodes.iter().map(|node| node.inner_html()).collect(),
            Step::Attr(name) => nodes
                .iter()
                .map(|node| node.value().attr(name.as_str()).unwrap_or_default().to_string())
                .collect(),
            Step::Br => nodes
                .iter()
                .map(|node| html_to_lines(&node.inner_html()))
                .collect(),
            // `regex:` / `trim` / `js:` 直接跟在节点后面时，先取节点纯文本作为起点，
            // 但这一步本身还要继续执行——所以它不算"消费节点"的分界步骤。
            Step::Regex(_) | Step::Trim | Step::Js(_) => nodes
                .iter()
                .map(|node| clean_text(&node.text().collect::<String>()))
                .collect(),
            Step::Css(_) | Step::Index(_) | Step::Skip(_) => {
                unreachable!("分界步骤不会是节点步骤")
            }
        }
        // `doc` / `nodes` / `root` 在这里析构，`Tendril` 不再跨越后续的 await。
    };

    // `text` / `html` / `attr:` / `br` 已经在上面把节点消费掉了；
    // 其余分界步骤（`regex:` / `trim` / `js:`）本身还需要在字符串阶段执行一次。
    let first_consumed_nodes = matches!(
        &steps[split],
        Step::Text | Step::Html | Step::Attr(_) | Step::Br
    );
    let rest = if first_consumed_nodes {
        &steps[split + 1..]
    } else {
        &steps[split..]
    };

    // ---- 阶段二：字符串阶段（允许 await）----
    for step in rest {
        match step {
            Step::Css(selector) => {
                return Err(anyhow!(
                    "规则顺序错误：`css:{selector}` 不能出现在字符串步骤之后（规则：{}）",
                    chain.source()
                ));
            }
            Step::Html => {
                return Err(anyhow!(
                    "规则顺序错误：`html` 不能出现在字符串步骤之后（规则：{}）",
                    chain.source()
                ));
            }
            Step::Attr(name) => {
                return Err(anyhow!(
                    "规则顺序错误：`attr:{name}` 不能出现在字符串步骤之后（规则：{}）",
                    chain.source()
                ));
            }
            Step::Text => {
                strings = strings.iter().map(|item| clean_text(item)).collect();
            }
            Step::Index(index) => {
                strings = vec![strings.get(*index).cloned().unwrap_or_default()];
            }
            Step::Skip(count) => {
                strings = strings.into_iter().skip(*count).collect();
            }
            Step::Br => {
                strings = strings.iter().map(|item| html_to_lines(item)).collect();
            }
            Step::Trim => {
                strings = strings.iter().map(|item| item.trim().to_string()).collect();
            }
            Step::Regex(pattern) => {
                let compiled =
                    Regex::new(pattern).map_err(|err| anyhow!("正则解析失败 `{pattern}`: {err}"))?;
                strings = strings
                    .iter()
                    .map(|item| {
                        compiled
                            .captures(item)
                            .and_then(|caps| caps.get(1).or_else(|| caps.get(0)))
                            .map(|found| found.as_str().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
            }
            Step::Js(code) => {
                let mut out = Vec::with_capacity(strings.len());
                for item in &strings {
                    let vars = JsVars::new().set("result", item).set("baseUrl", env.base_url);
                    out.push(env.js.eval(code, vars).await?);
                }
                strings = out;
            }
        }
    }

    Ok(strings)
}

/// 折叠连续空白并去首尾空格。`ElementRef::text()` 已经解码过实体，这里不再重复解码。
pub fn clean_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// 把 HTML 片段转成以换行分段的纯文本。
pub fn html_to_lines(input: &str) -> String {
    let stage = BR_RE.replace_all(input, "\n");
    let stage = BLOCK_END_RE.replace_all(&stage, "\n");
    let stage = TAG_RE.replace_all(&stage, "");
    let decoded = decode_html_entities(&stage);
    let mut lines = Vec::new();
    for line in decoded.split('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::PassThroughJs;
    use crate::engine::quickjs::QuickJsEvaluator;

    const PAGE: &str = r#"
      <html><body>
        <div class="result-list">
          <div class="item">
            <h3 class="title"><a href="/book/1">斗破苍穹</a></h3>
            <p class="author">天蚕土豆</p>
          </div>
          <div class="item">
            <h3 class="title"><a href="/book/2">凡人修仙传</a></h3>
            <p class="author">忘语</p>
          </div>
        </div>
      </body></html>"#;

    fn passthrough() -> EvalEnv<'static> {
        EvalEnv {
            js: &PassThroughJs,
            base_url: "",
        }
    }

    #[tokio::test]
    async fn list_rule_returns_item_fragments() {
        let chain = RuleChain::parse("css:div.result-list@css:div.item").unwrap();
        let items = eval(PAGE, &chain, &passthrough()).await.unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].contains("斗破苍穹"));
        // 列表项必须是**外层** HTML：字段规则要靠这个外层元素做 `css:` 导航的起点。
        // 若退化成 inner HTML，`css:h3.title@…` 这类规则会因为找不到自身而全部返回空。
        assert!(
            items[0].starts_with("<div"),
            "列表项应是外层 HTML，实际: {}",
            items[0]
        );
    }

    #[tokio::test]
    async fn field_rules_run_inside_item_fragment() {
        let list = RuleChain::parse("css:div.item").unwrap();
        let name = RuleChain::parse("css:h3.title@css:a@text").unwrap();
        let url = RuleChain::parse("css:h3.title@css:a@attr:href").unwrap();
        let author = RuleChain::parse("css:p.author@text").unwrap();
        let env = passthrough();

        let items = eval(PAGE, &list, &env).await.unwrap();
        let first = &items[0];
        assert_eq!(eval_element(first, &name, &env).await.unwrap()[0], "斗破苍穹");
        assert_eq!(eval_element(first, &url, &env).await.unwrap()[0], "/book/1");
        assert_eq!(eval_element(first, &author, &env).await.unwrap()[0], "天蚕土豆");
    }

    /// 回归测试：表格型搜索结果的列表项（一整行 `<tr>`）不能被解析丢弃。
    ///
    /// html5ever 在 `in body` 模式里遇到 `tr` / `td` 会按 parse error 忽略，片段解析完
    /// body 是空的；不补 `<table><tbody>` 外壳的话，整条书源表现为「搜索 0 条」。
    #[tokio::test]
    async fn table_row_fragment_keeps_its_cells() {
        let row = r#"<tr><td class="odd"><a href="/0_844/">斗破苍穹</a></td>
            <td class="even"><a href="/0_844/1.html">第一章</a></td>
            <td class="odd">天蚕土豆</td></tr>"#;
        let env = passthrough();

        let name = RuleChain::parse("css:td.odd@css:a@text").unwrap();
        let url = RuleChain::parse("css:td.odd@css:a@attr:href").unwrap();
        let author = RuleChain::parse("css:td@index:2@text").unwrap();

        assert_eq!(eval_element(row, &name, &env).await.unwrap()[0], "斗破苍穹");
        assert_eq!(eval_element(row, &url, &env).await.unwrap()[0], "/0_844/");
        assert_eq!(eval_element(row, &author, &env).await.unwrap()[0], "天蚕土豆");
    }

    /// `skip:` 用于跳过目录页开头重复列出的「最新章节」块。
    #[tokio::test]
    async fn skip_step_drops_the_repeated_prefix() {
        let page = r#"<html><body><div id="list"><dl>
            <dt>最新章节</dt>
            <dd><a href="/b/9.html">大结局</a></dd>
            <dd><a href="/b/8.html">第八章</a></dd>
            <dt>正文</dt>
            <dd><a href="/b/1.html">第一章</a></dd>
            <dd><a href="/b/2.html">第二章</a></dd>
        </dl></div></body></html>"#;
        let chain = RuleChain::parse("css:div#list@css:dl@css:dd@skip:2@css:a@text").unwrap();
        let items = eval(page, &chain, &passthrough()).await.unwrap();
        assert_eq!(items, vec!["第一章".to_string(), "第二章".to_string()]);
    }

    /// 回归测试：片段入口必须以片段元素为初始节点。
    ///
    /// `Html::parse_document` 会把片段包进 `<html><head></head><body>…</body></html>`，
    /// 所以页面级入口的初始节点是 `<html>`。`css:` 开头的规则能靠导航绕过去，
    /// 但 `attr:` / `text` 这类直接取值的规则会读错节点 —— `attr:href` 会静默返回
    /// 空串，然后整条书源表现为「字段全空」而没有任何报错。
    #[tokio::test]
    async fn element_entry_uses_the_fragment_element_as_initial_node() {
        let env = passthrough();
        let fragment = r#"<a class="link" href="/read/1.html" data-id="7">第一章</a>"#;

        let attr = RuleChain::parse("attr:href").unwrap();
        assert_eq!(eval_element(fragment, &attr, &env).await.unwrap()[0], "/read/1.html");

        let attr = RuleChain::parse("attr:data-id").unwrap();
        assert_eq!(eval_element(fragment, &attr, &env).await.unwrap()[0], "7");

        let text = RuleChain::parse("text").unwrap();
        assert_eq!(eval_element(fragment, &text, &env).await.unwrap()[0], "第一章");

        // `html` 步骤按设计取的是 inner HTML（保留子标签、去掉自身标签）。
        let nested = r#"<div class="box"><em>甲</em></div>"#;
        let inner = RuleChain::parse("html").unwrap();
        assert_eq!(
            eval_element(nested, &inner, &env).await.unwrap()[0],
            "<em>甲</em>"
        );

        // 页面级入口保持原语义：初始节点仍是文档根，读它的 `href` 只会得到空串。
        assert_eq!(eval(fragment, &attr_text("href"), &env).await.unwrap()[0], "");
    }

    fn attr_text(name: &str) -> RuleChain {
        RuleChain::parse(&format!("attr:{name}")).unwrap()
    }

    /// 多顶层元素的片段：初始节点是全部顶层元素，`text` 逐个产出。
    #[tokio::test]
    async fn element_entry_handles_multiple_top_level_nodes() {
        let env = passthrough();
        let fragment = "<span>甲</span><span>乙</span>";
        let text = RuleChain::parse("text").unwrap();
        assert_eq!(
            eval_element(fragment, &text, &env).await.unwrap(),
            vec!["甲".to_string(), "乙".to_string()]
        );
    }

    /// 空片段回退到文档根，不 panic、不丢内容。
    #[tokio::test]
    async fn element_entry_falls_back_when_fragment_is_empty() {
        let env = passthrough();
        let text = RuleChain::parse("text").unwrap();
        assert_eq!(eval_element("", &text, &env).await.unwrap(), vec!["".to_string()]);
    }

    #[tokio::test]
    async fn br_step_splits_lines_and_strips_tags() {
        let html = r#"<div id="content">第一段<br/>第二段<br/><br/>&nbsp;第三段 &amp; 结尾</div>"#;
        let chain = RuleChain::parse("css:div#content@br").unwrap();
        let out = eval(html, &chain, &passthrough()).await.unwrap();
        assert_eq!(out[0], "第一段\n第二段\n第三段 & 结尾");
    }

    #[tokio::test]
    async fn regex_prefers_capture_group() {
        let chain = RuleChain::parse(r"regex:第(\d+)章").unwrap();
        let out = eval("正文 第128章 开始", &chain, &passthrough())
            .await
            .unwrap();
        assert_eq!(out[0], "128");
    }

    #[tokio::test]
    async fn css_after_string_step_is_rejected() {
        let chain = RuleChain::parse("text@css:a").unwrap();
        assert!(eval(PAGE, &chain, &passthrough()).await.is_err());
    }

    #[test]
    fn clean_text_collapses_whitespace() {
        assert_eq!(clean_text("  斗破   苍穹 \n "), "斗破 苍穹");
    }

    #[tokio::test]
    async fn js_block_runs_through_quickjs() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let env = EvalEnv {
            js: &js,
            base_url: "",
        };
        let chain = RuleChain::parse("css:div#sign@text@<js>md5(result)</js>").unwrap();
        let out = eval(r#"<div id="sign">abc</div>"#, &chain, &env)
            .await
            .unwrap();
        assert_eq!(out[0], "900150983cd24fb0d6963f7d28e17f72");
    }

    #[tokio::test]
    async fn at_sign_inside_js_block_does_not_split_chain() {
        let js = QuickJsEvaluator::spawn().unwrap();
        let env = EvalEnv {
            js: &js,
            base_url: "",
        };
        let chain = RuleChain::parse("<js>result.split('@')[1]</js>").unwrap();
        assert_eq!(chain.steps().len(), 1);
        let out = eval("a@b", &chain, &env).await.unwrap();
        assert_eq!(out[0], "b");
    }

    #[test]
    fn js_prefix_form_also_works() {
        let chain = RuleChain::parse("text@js:result.trim()").unwrap();
        assert_eq!(chain.steps().len(), 2);
    }
}

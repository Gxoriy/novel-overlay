//! 前后端契约测试。
//!
//! 这组测试不启动 Tauri、不碰网络、不读数据库 —— 它只读源码，目的很窄：
//! **让 IPC 契约的两侧无法悄悄漂移**。
//!
//! 为什么需要它：`cargo test --lib` 全绿证明不了前端能用。命令名少一个字母、
//! 参数从 `sourceId` 写成 `source_id`、事件名两端不一致、HTML 里少了 `#toc-list`
//! —— 编译全过、测试全过，用户看到的却是「点了没反应」。这类缺陷在单元测试的
//! 视野之外，只有把两侧摆在一起逐字比对才抓得到。
//!
//! 用 `include_str!` 而不是运行时读文件：源码变更会触发重新编译，测试不可能
//! 对着上一次构建的旧副本通过。
//!
//! 唯一的事实来源是源码本身 —— `lib.rs` 里真实的 `generate_handler![...]`、
//! `commands.rs` 里真实的函数签名、TS 里真实的 `invoke` / `listen` 调用。
//! 测试不维护第二份命令清单，因此不存在「测试和实现一起写错」的可能。

use std::collections::BTreeSet;

const LIB_RS: &str = include_str!("../src/lib.rs");
const COMMANDS_RS: &str = include_str!("../src/commands.rs");
const API_TS: &str = include_str!("../../src/api.ts");
const MAIN_TS: &str = include_str!("../../src/main.ts");
const OVERLAY_TS: &str = include_str!("../../src/overlay.ts");
const INDEX_HTML: &str = include_str!("../../index.html");
const OVERLAY_HTML: &str = include_str!("../../overlay.html");

const COMMAND_MARKER: &str = "#[tauri::command]";
const HANDLER_MARKER: &str = "generate_handler![";

// ---------- 通用解析辅助 ----------

/// 从 `open_at`（必须指向开括号）开始，返回配对括号**内部**的切片。
fn balanced(src: &str, open_at: usize, open: char, close: char) -> &str {
    assert_eq!(
        src.as_bytes()[open_at] as char,
        open,
        "起点不是 `{open}`（实际是 `{}`）",
        &src[open_at..open_at + 1]
    );
    let mut depth = 0usize;
    for (offset, ch) in src[open_at..].char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return &src[open_at + open.len_utf8()..open_at + offset];
            }
        }
    }
    panic!("括号不配对：{open} ... {close}");
}

/// 逐个取出 `needle(...)` 调用括号内部的内容，跳过可选的 `<...>` 泛型参数。
///
/// `needle` 后面必须紧跟（允许空白）左括号才算命中 —— `import { invoke } from "…"`
/// 里的 `invoke` 就是靠这条守门排掉的，否则会解析出重复的假条目。
fn call_bodies<'a>(src: &'a str, needle: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = src[cursor..].find(needle) {
        let mut at = cursor + rel + needle.len();

        // 可选泛型：invoke<SearchBook[]>(...) / listen<boolean>(...)
        if src[at..].starts_with('<') {
            at += balanced(src, at, '<', '>').len() + 2;
        }

        let tail = src[at..].trim_start();
        if !tail.starts_with('(') {
            cursor = at;
            continue;
        }
        let open = src.len() - tail.len();
        let inner = balanced(src, open, '(', ')');
        cursor = open + inner.len() + 2;
        out.push(inner);
    }
    out
}

fn first_string_literal(src: &str) -> Option<String> {
    let start = src.find('"')?;
    let end = start + 1 + src[start + 1..].find('"')?;
    Some(src[start + 1..end].to_string())
}

/// 取 `attribute="value"` 里的 value，按出现顺序。
fn attribute_values(src: &str, attribute: &str) -> Vec<String> {
    let needle = format!("{attribute}=\"");
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = src[cursor..].find(&needle) {
        let start = cursor + rel + needle.len();
        let Some(end_rel) = src[start..].find('"') else {
            break;
        };
        let end = start + end_rel;
        out.push(src[start..end].to_string());
        cursor = end + 1;
    }
    out
}

/// 源码里所有 `q<...>("#id")` 形式的 id 选择器。
fn id_selectors(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = src[cursor..].find("\"#") {
        let start = cursor + rel + 2;
        let Some(end_rel) = src[start..].find('"') else {
            break;
        };
        let end = start + end_rel;
        out.push(src[start..end].to_string());
        cursor = end + 1;
    }
    out
}

/// `sourceId` -> `source_id`。Tauri 在 JS 侧按 camelCase 收参、Rust 侧按
/// snake_case 解构，两侧的名字天然不同形，比对前必须归一。
fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        if ch.is_uppercase() {
            out.push('_');
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// 取出 `fn <name>` 的函数体（配对花括号内部）。
fn function_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let at = src.find(&format!("fn {name}"))?;
    let brace = at + src[at..].find('{')?;
    Some(balanced(src, brace, '{', '}'))
}

// ---------- 两侧各自的解析结果 ----------

/// `lib.rs` 里 `generate_handler![...]` 实际注册的命令名。
fn registered_commands() -> Vec<String> {
    let at = LIB_RS
        .find(HANDLER_MARKER)
        .expect("lib.rs 里找不到 generate_handler![");
    let open_at = at + HANDLER_MARKER.len() - 1;
    balanced(LIB_RS, open_at, '[', ']')
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.rsplit("::")
                .next()
                .expect("路径至少有一段")
                .trim()
                .to_string()
        })
        .collect()
}

/// `commands.rs` 里每个 `#[tauri::command]` 函数的 (名字, 来自前端的参数名)。
///
/// `State<'_, AppState>` 与 `AppHandle` 由 Tauri 注入，不属于 payload，剔除。
fn rust_commands() -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = COMMANDS_RS;
    while let Some(rel) = rest.find(COMMAND_MARKER) {
        rest = &rest[rel + COMMAND_MARKER.len()..];
        let Some(fn_rel) = rest.find("fn ") else {
            break;
        };
        let name: String = rest[fn_rel + 3..]
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        let open = fn_rel + rest[fn_rel..].find('(').expect("函数必须有参数表");
        let params = balanced(rest, open, '(', ')')
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .filter_map(|item| {
                let (param, ty) = item.split_once(':')?;
                let ty = ty.trim();
                if ty.starts_with("State<") || ty.contains("AppHandle") {
                    return None;
                }
                Some(param.trim().to_string())
            })
            .collect();
        out.push((name, params));
    }
    out
}

struct Invocation {
    command: String,
    args: Vec<String>,
}

/// `api.ts` 里所有 `invoke<T>("command", { args })` 调用。
fn frontend_invocations() -> Vec<Invocation> {
    call_bodies(API_TS, "invoke")
        .into_iter()
        .filter_map(|inner| {
            let command = first_string_literal(inner)?;
            let args = match inner.find('{') {
                Some(brace) => balanced(inner, brace, '{', '}')
                    .split(',')
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(|key| key.split(':').next().unwrap_or(key).trim().to_string())
                    .collect(),
                None => Vec::new(),
            };
            Some(Invocation { command, args })
        })
        .collect()
}

// ---------- 契约 ----------

#[test]
fn every_frontend_command_is_registered_and_vice_versa() {
    let registered = registered_commands();
    let marker_count = COMMANDS_RS.matches(COMMAND_MARKER).count();
    assert_eq!(
        registered.len(),
        marker_count,
        "generate_handler! 注册了 {} 项，但 commands.rs 里有 {marker_count} 个 #[tauri::command] \
         —— 要么有命令忘了注册（前端调用会报 command not found），要么解析器坏了",
        registered.len()
    );

    let invocations = frontend_invocations();
    let frontend: Vec<String> = invocations.iter().map(|call| call.command.clone()).collect();

    for name in &frontend {
        assert!(
            registered.contains(name),
            "api.ts 调用了 `{name}`，但 lib.rs 的 generate_handler! 里没有注册它 \
             —— 运行时会报 command not found"
        );
    }
    for name in &registered {
        assert!(
            frontend.contains(name),
            "lib.rs 注册了 `{name}`，但 api.ts 里没有任何调用 —— 要么前端漏接，要么注册是多余的"
        );
    }
    assert_eq!(
        frontend.len(),
        registered.len(),
        "命令数量对不上，api.ts 里大概有重复调用"
    );
}

#[test]
fn command_parameter_names_match() {
    let rust = rust_commands();
    let invocations = frontend_invocations();

    let mut checked = 0usize;
    for call in &invocations {
        let Some((_, params)) = rust.iter().find(|(name, _)| *name == call.command) else {
            // 命令名对不上由上一个测试负责报错，这里跳过避免重复噪声。
            continue;
        };
        let expected: BTreeSet<String> = params.iter().map(|p| to_snake_case(p)).collect();
        let actual: BTreeSet<String> = call.args.iter().map(|a| to_snake_case(a)).collect();
        assert_eq!(
            actual, expected,
            "命令 `{}` 的参数名对不上：api.ts 传 {:?}，Rust 侧解构 {:?}。\
             Tauri 按名字取参，名字不符就是 None / 报错，且不会有编译期提示",
            call.command, actual, expected
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        rust.len(),
        "只校验了 {checked} / {} 个命令的参数名，说明有调用没匹配上 Rust 函数",
        rust.len()
    );
}

/// 事件是浮窗唯一的「重新加载」触发方式：`window.show()` 不会让 webview 重新执行
/// 脚本。所以事件名两端写错一个字符，功能就是静默失效 —— 不报错、不崩溃、只是没反应。
#[test]
fn frontend_listens_to_exactly_the_events_the_backend_emits() {
    let mut emitted: Vec<String> = call_bodies(COMMANDS_RS, "emit")
        .into_iter()
        .chain(call_bodies(LIB_RS, "emit"))
        .filter_map(first_string_literal)
        .collect();
    let mut listened: Vec<String> = call_bodies(OVERLAY_TS, "listen")
        .into_iter()
        .chain(call_bodies(MAIN_TS, "listen"))
        .filter_map(first_string_literal)
        .collect();

    assert!(!emitted.is_empty(), "一个 emit 都没解析到，解析器大概坏了");
    assert!(!listened.is_empty(), "一个 listen 都没解析到，解析器大概坏了");

    emitted.sort();
    emitted.dedup();
    listened.sort();
    listened.dedup();
    assert_eq!(
        emitted, listened,
        "后端广播的事件与前端监听的事件对不上：多出来的 emit 是死事件，多出来的 listen 永远不触发"
    );
}

#[test]
fn the_overlay_is_only_shown_through_one_function() {
    let body = function_body(COMMANDS_RS, "show_overlay")
        .expect("commands.rs 里找不到 show_overlay —— 解析器或实现变了");
    assert!(
        body.contains(".show()"),
        "show_overlay 里没有 window.show()，解析器或实现变了"
    );

    let call_sites = COMMANDS_RS.matches(".show()").count();
    assert_eq!(
        call_sites, 1,
        "commands.rs 里有 {call_sites} 处裸 .show()，应当只有 show_overlay 里那一处"
    );

    assert!(
        !LIB_RS.contains(".show()"),
        "lib.rs 里出现了裸 .show()。`show()` 只是把窗口显示出来，不会让 webview 重新\
         执行脚本 —— 浮窗会停在旧内容上（用户表现为「点了没反应」）。\
         必须改调 commands::show_overlay()，它才会广播 overlay://session"
    );
}

#[test]
fn every_dom_selector_exists_in_the_markup() {
    for (label, source, html) in [
        ("main.ts ↔ index.html", MAIN_TS, INDEX_HTML),
        ("overlay.ts ↔ overlay.html", OVERLAY_TS, OVERLAY_HTML),
    ] {
        let ids = id_selectors(source);
        assert!(
            !ids.is_empty(),
            "{label}：一个 id 选择器都没解析到，解析器大概坏了"
        );
        for id in &ids {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "{label}：源码里取了 `#{id}`，但 HTML 里没有 `id=\"{id}\"` \
                 —— 模块加载时会直接抛 missing element，整个页面白屏"
            );
        }
    }
}

#[test]
fn tab_buttons_have_matching_panes() {
    let tabs = attribute_values(INDEX_HTML, "data-tab");
    assert!(!tabs.is_empty(), "一个 data-tab 都没解析到");

    let panes: Vec<String> = attribute_values(INDEX_HTML, "id")
        .into_iter()
        .filter(|id| id.starts_with("pane-"))
        .collect();

    for tab in &tabs {
        let expected = format!("pane-{tab}");
        assert!(
            panes.contains(&expected),
            "标签页 data-tab=\"{tab}\" 找不到对应的 `id=\"{expected}\"` \
             —— 点这个标签页不会有任何反应（不报错，只是没反应）"
        );
    }
    assert_eq!(panes.len(), tabs.len(), "面板比标签页多，有孤儿面板");
}

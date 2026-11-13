//! robots.txt 解析与判定（RFC 9309）。
//!
//! 合规闸门要放在网络调用之前，所以这一层必须是纯函数：输入 robots.txt 文本 +
//! URL，输出允许/禁止，不碰网络、不碰文件系统。
//!
//! # 最容易写错的地方：分组边界
//!
//! 规则是「连续的 `User-agent:` 行属于同一组；只有在规则行之后再出现
//! `User-agent:` 才开新组」。如果按「是否进入 UA 块」来判断，所有 UA 组会塌成
//! 一组，`*` 组的 `Disallow: /` 会传染给每一个 UA —— 结果是**所有 URL 都被判为
//! 禁止**，而且不报任何错。
//!
//! 一个「全都禁止」的检查器看起来很像一个「很严格」的检查器，所以这里用
//! [`RobotsRules::group_count`] 把分组数量暴露出来，测试直接断言它。

use regex::Regex;
use url::Url;

static WILDCARD: &str = "*";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Disallowed,
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Decision::Allowed)
    }

    pub fn is_disallowed(self) -> bool {
        matches!(self, Decision::Disallowed)
    }
}

#[derive(Debug, Clone)]
struct Rule {
    /// 原始模式，用于「最长优先」比较
    pattern: String,
    allow: bool,
    matcher: Regex,
}

#[derive(Debug, Clone)]
struct Group {
    /// 已小写化的 UA 令牌
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

/// 一个 robots.txt 的解析结果。
#[derive(Debug, Clone, Default)]
pub struct RobotsRules {
    groups: Vec<Group>,
}

impl RobotsRules {
    /// 解析 robots.txt 文本。永不失败：无法理解的行走忽略，整体为空表示不限制。
    pub fn parse(text: &str) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut agents: Vec<String> = Vec::new();
        let mut rules: Vec<Rule> = Vec::new();
        let mut crawl_delay: Option<f64> = None;
        // 只有这个状态位能正确表达 RFC 的分组语义。
        let mut last_line_was_ua = false;

        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field.trim().to_ascii_lowercase(), value.trim().to_string()),
                None => continue,
            };

            if field == "user-agent" {
                if !last_line_was_ua {
                    flush(&mut agents, &mut rules, &mut crawl_delay, &mut groups);
                }
                if !value.is_empty() {
                    agents.push(value.to_ascii_lowercase());
                }
                last_line_was_ua = true;
                continue;
            }

            // 关键：任何非 `User-agent` 行都会关闭「连续 UA 行」状态。
            last_line_was_ua = false;

            // 规则出现在任何 UA 之前时无归属，按规范忽略。
            if agents.is_empty() {
                continue;
            }

            match field.as_str() {
                // 空值的 `Disallow:` 表示显式不限制，等价于没有这条规则。
                "disallow" if !value.is_empty() => rules.push(build_rule(&value, false)),
                "allow" if !value.is_empty() => rules.push(build_rule(&value, true)),
                "crawl-delay" => {
                    if let Ok(seconds) = value.parse::<f64>() {
                        if seconds.is_finite() && seconds >= 0.0 {
                            crawl_delay = Some(seconds);
                        }
                    }
                }
                _ => {}
            }
        }
        flush(&mut agents, &mut rules, &mut crawl_delay, &mut groups);

        Self { groups }
    }

    /// 组数量。暴露出来是为了让「分组塌陷」这个静默 bug 可以被测试锁死。
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// 规则总条数，用于自检与调试输出。
    pub fn rule_count(&self) -> usize {
        self.groups.iter().map(|group| group.rules.len()).sum()
    }

    /// 判定某个 UA 是否可以抓取某个 URL。
    pub fn decide(&self, user_agent: &str, url: &str) -> Decision {
        let Some(target) = match_target(url) else {
            // URL 解析不了，宁可放行也不静默禁止（调用方另有 URL 校验）。
            return Decision::Allowed;
        };
        let Some(group) = self.pick_group(user_agent) else {
            return Decision::Allowed;
        };

        // 最长模式优先；长度相同时 `Allow` 胜出。用元组比较一次表达这两条。
        let mut best: Option<&Rule> = None;
        for rule in &group.rules {
            if !rule.matcher.is_match(&target) {
                continue;
            }
            let candidate = (rule.pattern.len(), rule.allow);
            let current = best.map(|rule| (rule.pattern.len(), rule.allow));
            if current.is_none_or(|current| candidate > current) {
                best = Some(rule);
            }
        }

        match best {
            Some(rule) if rule.allow => Decision::Allowed,
            Some(_) => Decision::Disallowed,
            // 没有任何规则命中 → 放行。
            None => Decision::Allowed,
        }
    }

    /// 该 UA 对应的 `Crawl-delay`（秒）。作为限速下界使用，不可被调小。
    pub fn crawl_delay(&self, user_agent: &str) -> Option<f64> {
        self.pick_group(user_agent).and_then(|group| group.crawl_delay)
    }

    /// 选组：最长匹配的 UA 令牌优先，都不匹配才回落到 `*`。
    fn pick_group(&self, user_agent: &str) -> Option<&Group> {
        let ua = user_agent.to_ascii_lowercase();
        let mut best: Option<(&Group, usize)> = None;
        let mut wildcard: Option<&Group> = None;

        for group in &self.groups {
            for agent in &group.agents {
                if agent == WILDCARD {
                    if wildcard.is_none() {
                        wildcard = Some(group);
                    }
                    continue;
                }
                if ua.contains(agent.as_str()) {
                    let length = agent.len();
                    if best.is_none_or(|(_, best_len)| length > best_len) {
                        best = Some((group, length));
                    }
                }
            }
        }

        best.map(|(group, _)| group).or(wildcard)
    }
}

fn flush(
    agents: &mut Vec<String>,
    rules: &mut Vec<Rule>,
    crawl_delay: &mut Option<f64>,
    groups: &mut Vec<Group>,
) {
    if agents.is_empty() {
        rules.clear();
        *crawl_delay = None;
        return;
    }
    groups.push(Group {
        agents: std::mem::take(agents),
        rules: std::mem::take(rules),
        crawl_delay: crawl_delay.take(),
    });
}

fn build_rule(pattern: &str, allow: bool) -> Rule {
    Rule {
        pattern: pattern.to_string(),
        allow,
        matcher: compile(pattern),
    }
}

/// 把 robots 模式编译成正则：`*` 通配任意字符，仅结尾的 `$` 是尾锚。
fn compile(pattern: &str) -> Regex {
    let end_anchored = pattern.ends_with('$');
    let body = if end_anchored {
        &pattern[..pattern.len() - 1]
    } else {
        pattern
    };
    let escaped = body
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    let source = if end_anchored {
        format!("^{escaped}$")
    } else {
        format!("^{escaped}")
    };
    // `regex::escape` 之后不可能构造出非法正则。
    Regex::new(&source).expect("转义后的模式必定是合法正则")
}

/// 匹配对象是「路径 + 查询串」。
fn match_target(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let mut target = parsed.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    Some(target)
}

/// 表示「该站点没有任何限制」。robots.txt 返回 4xx 时按 RFC 视为这种情况。
pub fn unrestricted() -> RobotsRules {
    RobotsRules::default()
}

/// 表示「完全禁止」。
///
/// robots.txt 返回 5xx 或取不到时用这个 —— 按 RFC 9309，无法获取 robots.txt 时
/// 应保守处理。宁可少抓，也不要默认放行。
pub fn disallow_all() -> RobotsRules {
    RobotsRules::parse("User-agent: *\nDisallow: /\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_user_agent_lines_form_one_group() {
        let rules = RobotsRules::parse("User-agent: a\nUser-agent: b\nDisallow: /x\n");
        assert_eq!(rules.group_count(), 1);
        assert!(rules.decide("a", "https://s/x").is_disallowed());
        assert!(rules.decide("b", "https://s/x").is_disallowed());
    }

    #[test]
    fn a_user_agent_line_after_rules_starts_a_new_group() {
        let rules = RobotsRules::parse("User-agent: a\nDisallow: /x\nUser-agent: b\nDisallow: /y\n");
        assert_eq!(rules.group_count(), 2);
        assert!(rules.decide("a", "https://s/x").is_disallowed());
        assert!(rules.decide("a", "https://s/y").is_allowed());
        assert!(rules.decide("b", "https://s/y").is_disallowed());
        assert!(rules.decide("b", "https://s/x").is_allowed());
    }

    /// 锁死「所有 UA 组塌成一组」这个静默 bug。
    ///
    /// 一旦塌陷，`*` 组的 `Disallow: /` 会传染给所有 UA，表现为「所有 URL 都被
    /// 判为禁止」——不报错，只是悄悄把所有采集都拦掉。
    #[test]
    fn many_groups_do_not_collapse() {
        let mut text = String::new();
        for index in 0..60 {
            text.push_str(&format!(
                "User-agent: bot{index}\nDisallow: /private{index}\n\n"
            ));
        }
        text.push_str("User-agent: *\nDisallow: /\n");

        let rules = RobotsRules::parse(&text);
        assert_eq!(rules.group_count(), 61, "分组塌陷了，UA 组被合并成一组");
        assert_eq!(rules.rule_count(), 61);

        // 命中具体组的 UA 不受 `*` 组影响。
        assert!(rules.decide("bot7", "https://s/public").is_allowed());
        assert!(rules.decide("bot7", "https://s/private7").is_disallowed());
        // 只有没命中任何具体组的 UA 才落到 `*`。
        assert!(rules.decide("nobody", "https://s/public").is_disallowed());
    }

    #[test]
    fn longest_user_agent_token_wins() {
        let rules = RobotsRules::parse(
            "User-agent: *\nDisallow: /\n\n\
             User-agent: mybot\nDisallow: /a\n\n\
             User-agent: mybot-news\nDisallow: /b\n",
        );
        assert_eq!(rules.group_count(), 3);
        // `mybot-news` 更具体，用它自己的组：只禁 /b，因此 /a 放行。
        assert!(rules.decide("mybot-news", "https://s/a").is_allowed());
        assert!(rules.decide("mybot-news", "https://s/b").is_disallowed());
        // `mybot` 命中较短的令牌。
        assert!(rules.decide("mybot", "https://s/a").is_disallowed());
    }

    #[test]
    fn wildcard_and_end_anchor_are_supported() {
        let rules =
            RobotsRules::parse("User-agent: *\nDisallow: /*.pdf$\nAllow: /docs/\nDisallow: /tmp\n");
        assert!(rules.decide("x", "https://s/a/b.pdf").is_disallowed());
        // `$` 是尾锚，带查询串就不匹配了。
        assert!(rules.decide("x", "https://s/a/b.pdf?x=1").is_allowed());
        assert!(rules.decide("x", "https://s/docs/secret").is_allowed());
        // 没有 `$` 时是前缀匹配。
        assert!(rules.decide("x", "https://s/tmpfile").is_disallowed());
    }

    #[test]
    fn longest_pattern_wins_and_allow_wins_on_tie() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /a\nAllow: /a/b\n");
        assert!(rules.decide("x", "https://s/a/b/c").is_allowed());
        assert!(rules.decide("x", "https://s/a/z").is_disallowed());

        let rules = RobotsRules::parse("User-agent: *\nDisallow: /a\nAllow: /a\n");
        assert!(rules.decide("x", "https://s/a").is_allowed());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let rules = RobotsRules::parse("# 顶部注释\nUser-agent: *\n\nDisallow: /x  # 行尾注释\n");
        assert_eq!(rules.group_count(), 1);
        assert!(rules.decide("x", "https://s/x").is_disallowed());
    }

    #[test]
    fn empty_disallow_means_no_restriction() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow:\n");
        assert_eq!(rules.rule_count(), 0);
        assert!(rules.decide("x", "https://s/anything").is_allowed());
    }

    #[test]
    fn rules_before_any_user_agent_are_ignored() {
        let rules = RobotsRules::parse("Disallow: /\nUser-agent: *\nDisallow: /x\n");
        assert_eq!(rules.group_count(), 1);
        assert_eq!(rules.rule_count(), 1);
        assert!(rules.decide("x", "https://s/other").is_allowed());
    }

    #[test]
    fn crawl_delay_is_read_from_matching_group() {
        let rules = RobotsRules::parse("User-agent: *\nCrawl-delay: 2.5\nDisallow: /x\n");
        assert_eq!(rules.crawl_delay("anything"), Some(2.5));

        let rules = RobotsRules::parse("User-agent: *\nDisallow: /x\n");
        assert_eq!(rules.crawl_delay("anything"), None);
    }

    #[test]
    fn empty_document_restricts_nothing() {
        let rules = RobotsRules::parse("");
        assert_eq!(rules.group_count(), 0);
        assert!(rules.decide("x", "https://s/anything").is_allowed());
    }

    #[test]
    fn disallow_all_blocks_everything() {
        let rules = disallow_all();
        assert!(rules.decide("anything", "https://s/").is_disallowed());
        assert!(rules.decide("anything", "https://s/a/b?c=1").is_disallowed());
    }

    #[test]
    fn query_string_participates_in_matching() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /search?\n");
        assert!(rules.decide("x", "https://s/search?q=1").is_disallowed());
        assert!(rules.decide("x", "https://s/search").is_allowed());
    }
}

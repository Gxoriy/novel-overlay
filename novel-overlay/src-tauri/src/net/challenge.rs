//! 挑战检测。
//!
//! 采集链路遇到下面任意一种情况，说明已经被站点识别出来了：
//!
//! | 类型 | 特征 |
//! |---|---|
//! | `Captcha` | 响应体里出现验证码 / 人机校验标记 |
//! | `RateLimited` | HTTP 429 |
//! | `Unavailable` | HTTP 503 |
//! | `EmptyShell` | HTTP 200，但内容为空或只是个空壳 |
//!
//! **识别到挑战就停下来，冷却，然后降级。重试是最坏的选择** —— 它会把 IP、
//! 账号、整个代理池一起拖下水，而且成功率不会因为重试次数增加而提高。

/// 挑战类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    Captcha,
    RateLimited,
    Unavailable,
    EmptyShell,
}

impl ChallengeKind {
    pub fn label(self) -> &'static str {
        match self {
            ChallengeKind::Captcha => "captcha",
            ChallengeKind::RateLimited => "rate-limited",
            ChallengeKind::Unavailable => "unavailable",
            ChallengeKind::EmptyShell => "empty-shell",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub kind: ChallengeKind,
    pub detail: String,
}

impl std::fmt::Display for Challenge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}（{}）", self.kind.label(), self.detail)
    }
}

/// 判定「空壳页」的字节下限。真实的小说站页面都在 1KB 以上，
/// 200 字节以下基本只可能是空壳或占位页。
pub const MIN_BODY_BYTES: usize = 200;

/// 默认挑战标记（小写匹配）。
///
/// 站点可以自带一套更准的标记覆盖它 —— 见 `BookSource::challengeMarkers`。
pub const DEFAULT_MARKERS: &[&str] = &[
    "validatecaptcha",
    "captcha",
    "cf-challenge",
    "challenge-platform",
    "just a moment...",
    "attention required",
    "are you a robot",
    "verify you are human",
    "人机验证",
    "请输入验证码",
    "验证码",
    "访问过于频繁",
    "请求过于频繁",
    "访问受限",
];

/// 一个站点生效的挑战标记集。
#[derive(Debug, Clone)]
pub struct ChallengeProfile {
    markers: Vec<String>,
}

impl Default for ChallengeProfile {
    fn default() -> Self {
        Self::from_source(None)
    }
}

impl ChallengeProfile {
    /// 站点自带标记优先，再补上默认标记。站点标记不覆盖默认，而是叠加 ——
    /// 漏判的代价远大于误判。
    pub fn from_source(extra: Option<&[String]>) -> Self {
        let mut markers: Vec<String> = Vec::new();
        if let Some(extra) = extra {
            for marker in extra {
                let lowered = marker.trim().to_lowercase();
                if !lowered.is_empty() && !markers.contains(&lowered) {
                    markers.push(lowered);
                }
            }
        }
        for marker in DEFAULT_MARKERS {
            let lowered = marker.to_lowercase();
            if !markers.contains(&lowered) {
                markers.push(lowered);
            }
        }
        Self { markers }
    }

    pub fn markers(&self) -> &[String] {
        &self.markers
    }

    /// 判定一次响应是否为挑战。返回 `None` 表示看起来正常。
    pub fn detect(&self, status: u16, body: &str) -> Option<Challenge> {
        match status {
            429 => {
                return Some(Challenge {
                    kind: ChallengeKind::RateLimited,
                    detail: "HTTP 429".to_string(),
                });
            }
            503 => {
                return Some(Challenge {
                    kind: ChallengeKind::Unavailable,
                    detail: "HTTP 503".to_string(),
                });
            }
            _ => {}
        }

        let lowered = body.to_lowercase();
        for marker in &self.markers {
            if lowered.contains(marker.as_str()) {
                return Some(Challenge {
                    kind: ChallengeKind::Captcha,
                    detail: format!("命中挑战标记 `{marker}`"),
                });
            }
        }

        let trimmed = body.trim();
        if status == 200 && trimmed.len() < MIN_BODY_BYTES {
            return Some(Challenge {
                kind: ChallengeKind::EmptyShell,
                detail: format!("响应体仅 {} 字节", trimmed.len()),
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(size: usize) -> String {
        "<html><body>".to_string() + &"x".repeat(size) + "</body></html>"
    }

    #[test]
    fn status_codes_are_detected() {
        let profile = ChallengeProfile::default();
        let long = body(2000);
        assert_eq!(
            profile.detect(429, &long).map(|c| c.kind),
            Some(ChallengeKind::RateLimited)
        );
        assert_eq!(
            profile.detect(503, &long).map(|c| c.kind),
            Some(ChallengeKind::Unavailable)
        );
        assert_eq!(profile.detect(200, &long), None);
    }

    #[test]
    fn captcha_markers_are_detected() {
        let profile = ChallengeProfile::default();
        let page = format!(
            "<html><body>{}</body></html>",
            "a".repeat(500) + "Enter the characters you see below validateCaptcha"
        );
        let challenge = profile.detect(200, &page).expect("应识别为验证码");
        assert_eq!(challenge.kind, ChallengeKind::Captcha);
    }

    #[test]
    fn empty_shell_is_detected() {
        let profile = ChallengeProfile::default();
        let challenge = profile.detect(200, "<html></html>").expect("应识别为空壳页");
        assert_eq!(challenge.kind, ChallengeKind::EmptyShell);
    }

    #[test]
    fn source_markers_are_merged_with_defaults() {
        let extra = vec!["站点专属标记".to_string(), "custom-wall".to_string()];
        let profile = ChallengeProfile::from_source(Some(&extra));
        assert!(profile.markers().contains(&"站点专属标记".to_string()));
        // 默认标记仍然在，不会被站点标记顶掉。
        assert!(profile.markers().contains(&"captcha".to_string()));

        let page = format!("<html>{}</html>", "b".repeat(500) + "custom-wall");
        assert_eq!(
            profile.detect(200, &page).map(|c| c.kind),
            Some(ChallengeKind::Captcha)
        );
    }

    #[test]
    fn markers_are_matched_case_insensitively() {
        let profile = ChallengeProfile::default();
        let page = format!("<html>{}</html>", "c".repeat(500) + "JUST A MOMENT...");
        assert!(profile.detect(200, &page).is_some());
    }

    #[test]
    fn blank_source_markers_are_ignored() {
        let extra = vec!["".to_string(), "   ".to_string()];
        let profile = ChallengeProfile::from_source(Some(&extra));
        assert!(!profile.markers().iter().any(|marker| marker.trim().is_empty()));
    }
}

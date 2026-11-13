use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

/// 注入给 JS 片段的变量，例如 `result` / `key` / `baseUrl`。
#[derive(Debug, Clone, Default)]
pub struct JsVars {
    entries: Vec<(String, String)>,
}

impl JsVars {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, name: &str, value: &str) -> Self {
        self.entries.push((name.to_string(), value.to_string()));
        self
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }
}

pub type JsFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// 规则里 `js:` / `<js>` 片段的执行入口。
///
/// 返回 boxed future 而不是用 `async fn`，是为了让 trait 保持 dyn-safe
/// （`AppState` 里存的是 `Box<dyn JsEvaluator>`）。
pub trait JsEvaluator: Send + Sync {
    fn eval<'a>(&'a self, script: &'a str, vars: JsVars) -> JsFuture<'a>;
}

/// 占位实现：不执行脚本，原样返回 `result`。
///
/// 用于两处：单元测试，以及 QuickJS 初始化失败时的降级路径。
pub struct PassThroughJs;

impl JsEvaluator for PassThroughJs {
    fn eval<'a>(&'a self, _script: &'a str, vars: JsVars) -> JsFuture<'a> {
        Box::pin(async move {
            Ok(vars
                .entries()
                .iter()
                .find(|(name, _)| name == "result")
                .map(|(_, value)| value.clone())
                .unwrap_or_default())
        })
    }
}

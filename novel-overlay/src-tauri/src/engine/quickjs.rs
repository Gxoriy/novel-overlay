use std::thread;

use anyhow::{anyhow, Context as _, Result};
use rquickjs::{Context as JsContext, Runtime};
use tokio::sync::{mpsc, oneshot};

use crate::engine::js::{JsEvaluator, JsFuture, JsVars};
use crate::engine::shim;

struct Job {
    script: String,
    vars: JsVars,
    reply: oneshot::Sender<std::result::Result<String, String>>,
}

/// 内嵌 QuickJS 执行器。
///
/// QuickJS 的 Runtime 不是 Sync 的，所以引擎独占一条线程，其余线程通过 channel 投递任务。
/// 这样既满足 `JsEvaluator: Send + Sync`（`AppState` 要求），也不会阻塞 tokio 的 worker：
/// 调用方 `await` 一个 oneshot，而不是忙等。
pub struct QuickJsEvaluator {
    tx: mpsc::UnboundedSender<Job>,
}

impl QuickJsEvaluator {
    pub fn spawn() -> Result<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();

        thread::Builder::new()
            .name("quickjs".to_string())
            .spawn(move || {
                let runtime = match Runtime::new() {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("创建 QuickJS Runtime 失败: {error}")));
                        return;
                    }
                };
                let context = match JsContext::full(&runtime) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("创建 QuickJS Context 失败: {error}")));
                        return;
                    }
                };
                // `Context::with` 的签名是 `fn with<F, R>(&self, f: F) -> R`：
                // 它把闭包的返回值原样带出来，不额外包一层 Result。
                if let Err(error) = context.with(shim::install) {
                    let _ = ready_tx.send(Err(format!("安装宿主环境失败: {error}")));
                    return;
                }
                let _ = ready_tx.send(Ok(()));

                while let Some(job) = rx.blocking_recv() {
                    let outcome =
                        context.with(|ctx| shim::run_script(&ctx, &job.script, &job.vars));
                    let _ = job.reply.send(outcome);
                }
            })
            .context("启动 QuickJS 线程失败")?;

        ready_rx
            .recv()
            .context("QuickJS 线程未回报就绪状态")?
            .map_err(|message| anyhow!(message))?;

        Ok(Self { tx })
    }
}

impl JsEvaluator for QuickJsEvaluator {
    fn eval<'a>(&'a self, script: &'a str, vars: JsVars) -> JsFuture<'a> {
        Box::pin(async move {
            let (reply, wait) = oneshot::channel();
            self.tx
                .send(Job {
                    script: script.to_string(),
                    vars,
                    reply,
                })
                .map_err(|_| anyhow!("QuickJS 线程已退出"))?;
            wait.await
                .map_err(|_| anyhow!("QuickJS 未返回结果"))?
                .map_err(|message| anyhow!(message))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> QuickJsEvaluator {
        QuickJsEvaluator::spawn().expect("QuickJS 应能启动")
    }

    #[tokio::test]
    async fn returns_last_expression() {
        let js = engine();
        let out = js
            .eval("result + '·天蚕土豆'", JsVars::new().set("result", "斗破苍穹"))
            .await
            .unwrap();
        assert_eq!(out, "斗破苍穹·天蚕土豆");
    }

    #[tokio::test]
    async fn supports_top_level_return() {
        let js = engine();
        let out = js
            .eval(
                "var parts = result.split('|'); return parts[1];",
                JsVars::new().set("result", "a|b|c"),
            )
            .await
            .unwrap();
        assert_eq!(out, "b");
    }

    #[tokio::test]
    async fn md5_is_available() {
        let js = engine();
        let out = js.eval("md5('abc')", JsVars::new()).await.unwrap();
        assert_eq!(out, "900150983cd24fb0d6963f7d28e17f72");
    }

    #[tokio::test]
    async fn aes_roundtrip_inside_js() {
        let js = engine();
        let script = "var c = aesEncrypt('CBC','0123456789abcdef','abcdef0123456789', result); \
                      aesDecrypt('CBC','0123456789abcdef','abcdef0123456789', c)";
        let out = js
            .eval(script, JsVars::new().set("result", "t=1700000000"))
            .await
            .unwrap();
        assert_eq!(out, "t=1700000000");
    }

    #[tokio::test]
    async fn objects_become_json() {
        let js = engine();
        let out = js.eval("({a:1,b:'x'})", JsVars::new()).await.unwrap();
        assert_eq!(out, "{\"a\":1,\"b\":\"x\"}");
    }

    #[tokio::test]
    async fn java_kv_roundtrip() {
        let js = engine();
        let out = js
            .eval(
                "java.put('k', result); java.get('k')",
                JsVars::new().set("result", "v1"),
            )
            .await
            .unwrap();
        assert_eq!(out, "v1");
    }

    #[tokio::test]
    async fn base_url_is_visible_to_scripts() {
        let js = engine();
        let out = js
            .eval(
                "baseUrl + '|' + document.location.host",
                JsVars::new().set("baseUrl", "https://example.com/book/1"),
            )
            .await
            .unwrap();
        assert_eq!(out, "https://example.com/book/1|example.com");
    }

    #[tokio::test]
    async fn errors_are_reported() {
        let js = engine();
        let error = js
            .eval("throw new Error('签名失败')", JsVars::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("签名失败"), "实际错误: {error}");
    }

    #[tokio::test]
    async fn crypto_failures_surface_to_js() {
        let js = engine();
        let error = js
            .eval("aesEncrypt('CBC', 'short', '0123456789abcdef', 'x')", JsVars::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("AES"), "实际错误: {error}");
    }
}

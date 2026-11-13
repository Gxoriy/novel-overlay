//! QuickJS 宿主环境。
//!
//! 站点书源里的 `<js>` 片段期待一个"像浏览器/像阅读 App"的运行时。这里补三层：
//!
//! 1. **Rust 原生函数**：`md5` / `sha1` / `sha256` / `hmac*` / `base64*` / `hex*` / `aes*`
//! 2. **JS 预置脚本**：`java.get/put`、`console`、`atob/btoa`、`__toText`、
//!    以及 `window` / `navigator` / `document` 的最小可用替身
//! 3. **每次求值注入的变量**：`result`、`baseUrl`、`key` 等
//!
//! 明确不支持：真实 DOM、`java.ajax`（同步网络）、定时器。

use anyhow::{anyhow, Result};
use rquickjs::{Ctx, Function, Object, Value};
use rquickjs::Result as JsResult;

use crate::engine::crypto;
use crate::engine::js::JsVars;

/// 只跑一次，装在 context 上。
const PRELUDE: &str = r#"
var __kv = {};

var java = {
  get: function (key) {
    return Object.prototype.hasOwnProperty.call(__kv, key) ? __kv[key] : null;
  },
  put: function (key, value) { __kv[key] = value; return value; },
  log: function () {},
  toast: function () {}
};

var console = {
  log: function () {}, info: function () {}, warn: function () {},
  error: function () {}, debug: function () {}
};

var atob = function (input) { return base64Decode(input); };
var btoa = function (input) { return base64Encode(input); };

var __toText = function (value) {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (value instanceof Error) return value.name + ": " + value.message;
  if (typeof value === "object") {
    try { return JSON.stringify(value); } catch (error) { return String(value); }
  }
  return String(value);
};

var window = globalThis;
var navigator = {
  userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
};

var document = {
  cookie: "",
  get location() {
    var href = (typeof baseUrl === "string") ? baseUrl : "";
    return { href: href, host: href.replace(/^https?:\/\//, "").split("/")[0] };
  },
  querySelector: function () { return null; },
  querySelectorAll: function () { return []; },
  getElementById: function () { return null; }
};
"#;

fn js_error(message: impl std::fmt::Display) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("rust", "js", message.to_string())
}

/// 安装宿主环境。每个 Context 调用一次。
pub fn install<'js>(ctx: Ctx<'js>) -> Result<()> {
    let globals = ctx.globals();

    globals
        .set("md5", Function::new(ctx.clone(), |input: String| crypto::md5_hex(&input))?)
        .map_err(|error| anyhow!("注册 md5 失败: {error}"))?;
    globals
        .set("sha1", Function::new(ctx.clone(), |input: String| crypto::sha1_hex(&input))?)
        .map_err(|error| anyhow!("注册 sha1 失败: {error}"))?;
    globals
        .set(
            "sha256",
            Function::new(ctx.clone(), |input: String| crypto::sha256_hex(&input))?,
        )
        .map_err(|error| anyhow!("注册 sha256 失败: {error}"))?;

    globals
        .set(
            "hmacSha1",
            Function::new(ctx.clone(), |key: String, message: String| -> JsResult<String> {
                crypto::hmac_sha1_hex(&key, &message).map_err(js_error)
            })?,
        )
        .map_err(|error| anyhow!("注册 hmacSha1 失败: {error}"))?;
    globals
        .set(
            "hmacSha256",
            Function::new(ctx.clone(), |key: String, message: String| -> JsResult<String> {
                crypto::hmac_sha256_hex(&key, &message).map_err(js_error)
            })?,
        )
        .map_err(|error| anyhow!("注册 hmacSha256 失败: {error}"))?;

    globals
        .set(
            "base64Encode",
            Function::new(ctx.clone(), |input: String| crypto::base64_encode(&input))?,
        )
        .map_err(|error| anyhow!("注册 base64Encode 失败: {error}"))?;
    globals
        .set(
            "base64Decode",
            Function::new(ctx.clone(), |input: String| -> JsResult<String> {
                crypto::base64_decode(&input).map_err(js_error)
            })?,
        )
        .map_err(|error| anyhow!("注册 base64Decode 失败: {error}"))?;

    globals
        .set(
            "hexEncode",
            Function::new(ctx.clone(), |input: String| crypto::hex_encode(&input))?,
        )
        .map_err(|error| anyhow!("注册 hexEncode 失败: {error}"))?;
    globals
        .set(
            "hexDecode",
            Function::new(ctx.clone(), |input: String| -> JsResult<String> {
                crypto::hex_decode(&input).map_err(js_error)
            })?,
        )
        .map_err(|error| anyhow!("注册 hexDecode 失败: {error}"))?;

    globals
        .set(
            "aesEncrypt",
            Function::new(
                ctx.clone(),
                |mode: String, key: String, iv: String, data: String| -> JsResult<String> {
                    crypto::aes_encrypt(&mode, &key, &iv, &data).map_err(js_error)
                },
            )?,
        )
        .map_err(|error| anyhow!("注册 aesEncrypt 失败: {error}"))?;
    globals
        .set(
            "aesDecrypt",
            Function::new(
                ctx.clone(),
                |mode: String, key: String, iv: String, data: String| -> JsResult<String> {
                    crypto::aes_decrypt(&mode, &key, &iv, &data).map_err(js_error)
                },
            )?,
        )
        .map_err(|error| anyhow!("注册 aesDecrypt 失败: {error}"))?;

    ctx.eval::<(), _>(PRELUDE)
        .map_err(|error| anyhow!("执行 JS 预置脚本失败: {}", describe(&ctx, error)))?;

    Ok(())
}

/// 执行一段书源 JS。`vars` 会被注入为全局变量。
///
/// 支持两种写法：
/// - **最后表达式即返回值**（"阅读"书源的常见写法）
/// - **顶层 `return`**（program 模式下是语法错误，自动用函数包裹重试）
pub fn run_script<'js>(
    ctx: &Ctx<'js>,
    script: &str,
    vars: &JsVars,
) -> std::result::Result<String, String> {
    let globals: Object<'js> = ctx.globals();
    for (name, value) in vars.entries() {
        globals
            .set(name.as_str(), value.as_str())
            .map_err(|error| format!("注入 JS 变量 {name} 失败: {error}"))?;
    }

    match ctx.eval::<Value, _>(script) {
        Ok(value) => convert(ctx, value),
        Err(error) => {
            // 先把异常取走，否则 QuickJS 上下文会残留未处理的异常状态
            let first = describe(ctx, error);
            let wrapped = format!("__toText((function(){{ {script} \n}})())");
            match ctx.eval::<String, _>(wrapped.as_str()) {
                Ok(text) => Ok(text),
                Err(second) => {
                    let detail = describe(ctx, second);
                    Err(if detail.is_empty() { first } else { format!("{first}（函数包裹重试同样失败：{detail}）") })
                }
            }
        }
    }
}

/// 用 JS 侧的 `__toText` 把任意返回值转成字符串（对象会走 JSON.stringify）。
///
/// `Ctx<'js>` 对 `'js` 是不变型（invariant），所以 `ctx` 与 `value` 必须共用同一个
/// 具名生命周期，否则编译器无法把两者绑到一起。
fn convert<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> std::result::Result<String, String> {
    let globals: Object<'js> = ctx.globals();
    let helper: Function<'js> = globals
        .get("__toText")
        .map_err(|error| format!("宿主环境缺少 __toText: {error}"))?;
    helper
        .call((value,))
        .map_err(|error| format!("JS 返回值转换失败: {}", describe(ctx, error)))
}

/// 把 rquickjs 错误转成可读信息；遇到异常时取走并格式化异常值。
fn describe<'js>(ctx: &Ctx<'js>, error: rquickjs::Error) -> String {
    if matches!(error, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Ok(text) = convert(ctx, caught) {
            if !text.is_empty() && text != "{}" {
                return text;
            }
        }
        return "JS 抛出异常（无详情）".to_string();
    }
    error.to_string()
}

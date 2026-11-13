# novel-overlay

桌面端小说阅读器：主窗口管书库，阅读浮窗透明置顶，随时叠在其他应用上继续读。

基于 Tauri 2（Rust 后端 + 原生 WebView 前端），支持 macOS 与 Windows。

## 功能

- **书源管理**：书源是一份 JSON 规则文件（见 [`novel-overlay/sources/`](novel-overlay/sources/)），搜索、目录、正文三层都用同一条规则管道描述，支持 `css:` 选择器、`js:` 片段、正则清洗
- **搜索 / 书架 / 目录 / 下载**：多书源搜索、加入书架、全本下载缓存（SQLite），已下载章节离线可读
- **阅读浮窗**：透明、置顶、不抢焦点的无边框小窗，全局快捷键呼出/收起，键盘翻页，纯文字模式
- **合规采集层**：公开网页抓取使用诚实 UA、先查 `robots.txt`、令牌桶限速、失败熔断；按出口 IP 限流的站点可配置代理池轮换
- **离线回放模式**：把抓过的页面存成本地夹具，开发调试完全离线，不碰真实站点
- **自定义数据目录**：书库默认放在程序根目录 `docs/`，可在设置页改到任意可写路径，重启生效

## 构建与运行

前置：[Rust](https://rustup.rs)（stable）、Node.js ≥ 18；Windows 还需 WebView2、macOS 需 Xcode Command Line Tools。

```bash
cd novel-overlay
npm install

# 开发
npm run tauri dev

# 打包（macOS 出 app/dmg，Windows 出 nsis/msi）
npm run tauri build
```

## 书源格式

```jsonc
{
  "bookSourceName": "示例",
  "bookSourceUrl": "https://example.com",
  "enabled": true,
  "searchUrl": "/search?q={{key}}",           // 支持 <js>…</js> 片段算签名
  "searchBody": "s={{urlencode:key}}",        // 可选：配置即用 POST 发搜索（表单是 method="post" 的站点）
  "ruleSearch": {
    "bookList": "css:div.result@css:dl",       // 每个结果一个列表项
    "name": "css:dd@css:h3@css:a@text",
    "bookUrl": "css:dd@css:h3@css:a@attr:href",
    "author": "css:dd.book_other@css:span@text"
  },
  "ruleToc": {
    "chapterList": "css:div#list@css:dl@css:dd@skip:9@css:a",
    "chapterName": "text",
    "chapterUrl": "attr:href"
  },
  "ruleContent": {
    "content": "css:div#content@br"            // br 按换行切段
  }
}
```

规则是一条 `@` 分隔的管道：

| 步骤 | 作用 |
| --- | --- |
| `css:<selector>` | 在当前节点集合的**后代**里选元素 |
| `index:<n>` / `skip:<n>` | 取第 n 个 / 丢掉前 n 个（节点或字符串皆可） |
| `text` / `html` / `attr:<name>` | 取纯文本 / 内部 HTML / 属性 |
| `br` | 把 `<br>`/`<p>` 段落切成多行文本 |
| `regex:<pattern>` | 正则提取或替换 |
| `trim` | 去首尾空白 |
| `js:<code>` | QuickJS 求值，变量 `result` 为当前值 |

关键词占位符（`searchUrl` 与 `searchBody` 通用）：

| 写法 | 编码 | 适用 |
| --- | --- | --- |
| `{{key}}` | 原样 | 常规 UTF-8 站点 |
| `{{gbk:key}}` | GBK 百分号编码 | 老站（笔趣阁系），UTF-8 传过去只会「查无此书」 |
| `{{urlencode:key}}` | UTF-8 表单编码 | POST 表单体：中文原样字节会被服务端当成「没有输入」 |

`searchBody` 存在时搜索用 POST 发送，请求体支持同样的占位符与 `<js>` 片段；
引擎会自动补 `Content-Type: application/x-www-form-urlencoded`。若站点按地区
拒绝代理出口（403 页面含 "The region has been denied."），引擎会自动改用本机
直连重试一次，无需书源配置。

写新书源时用探针一步完成「转换 → 实测 → 入库」：

```bash
cd novel-overlay/src-tauri
cargo run --example source_probe -- --json=../sources/<你的书源>.json --keyword=书名
# 验证通过后追加 --import --db=../docs/novel-overlay.db
```

探针走的是应用同一条采集链路（同样的 UA、robots 闸门、限速与挑战检测），三层全过才算可用。

## 数据与环境变量

- 数据目录：默认 `<程序根目录>/docs/`（含 SQLite 书库）；自定义目录写入 `%APPDATA%/com.noveloverlay.desktop/settings.json`（macOS 在 `~/Library/Application Support/`）
- `NOVEL_OVERLAY_PROXIES`：逗号分隔的出口代理列表，如 `http://127.0.0.1:7897`；未设置时会自动探测本机常见代理端口
- `NOVEL_OVERLAY_REPLAY_DIRS`：设置后进入离线回放模式，只从本地夹具取页面

## 目录结构

```
novel-overlay/
├── src/            # 前端（TypeScript + Vite，无框架）
├── src-tauri/      # Rust 后端
│   ├── src/engine/ # 书源引擎：规则求值、QuickJS、内置 API 源
│   ├── src/net/    # 采集阶梯：robots、限速、熔断、代理池、回放
│   ├── src/parse/  # 规则管道 DSL
│   ├── examples/   # source_probe / live_chapter_probe 实测探针
│   └── tests/      # 契约测试
├── sources/        # 书源文件（含已实测可用的示例）
└── tools/          # 图标生成脚本
```

## 免责声明

本项目仅用于学习 Rust / Tauri 桌面开发与网页解析技术。书籍内容版权归原作者及原站点所有，请遵守目标站点的 `robots.txt` 与服务条款，仅作个人阅读使用，勿用于任何商业用途。

## License

[MIT](LICENSE)

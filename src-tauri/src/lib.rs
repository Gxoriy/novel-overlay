pub mod commands;
pub mod engine;
pub mod model;
pub mod net;
pub mod parse;
pub mod settings;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use crate::commands::AppState;
use crate::engine::js::{JsEvaluator, PassThroughJs};
use crate::engine::QuickJsEvaluator;
use crate::net::clock;
use crate::net::{
    Fetcher, Ladder, PageSource, ProxyPool, ProxySpec, PublicWebConfig, PublicWebTier, ReplayTier,
    TierSource, Transport, UnconfiguredTier,
};
use crate::store::Store;

/// 离线回放目录，冒号分隔（macOS 的 PATH 风格）。
///
/// 设置这个环境变量即进入**回放模式**：关掉公开页面抓取，只从本地夹具取页面。
/// 开发和手动调试都走这条路，不碰任何真实站点。
const REPLAY_DIRS_ENV: &str = "NOVEL_OVERLAY_REPLAY_DIRS";

fn replay_dirs() -> Vec<PathBuf> {
    std::env::var(REPLAY_DIRS_ENV)
        .map(|value| {
            value
                .split(':')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// 装配分级获取阶梯。
///
/// - L0 / L1：占位，未配置时如实跳过
/// - L2：公开页面。**只有在非回放模式下才显式打开** —— 库的默认值是关闭的，
///   这里打开是因为触发点始终是「用户主动点搜索/开始阅读，且目标是他自己配置的书源」
/// - L3：离线回放，仅在设置 [`REPLAY_DIRS_ENV`] 时挂载
fn build_ladder() -> Result<Arc<Ladder>, String> {
    let dirs = replay_dirs();
    let replay_mode = !dirs.is_empty();

    let clock = clock::system();

    // 出口代理池。`NOVEL_OVERLAY_PROXIES` 优先；否则 Fetcher 会自动探测本机
    // Clash Verge（7897）等端口——笔趣阁按出口 IP 限流，直连被封时必须换出口。
    //
    // 代理池在 Ladder **之下**，只影响「用哪个出口」，不影响 robots / 熔断。
    let proxies = ProxyPool::from_env(clock.clone()).map(Arc::new);
    if let Some(pool) = &proxies {
        log::info!("{}", pool.describe());
    } else {
        let local = crate::net::proxy::discover_local_proxies();
        if local.is_empty() {
            log::warn!(
                "未配置 NOVEL_OVERLAY_PROXIES，且未探测到本机代理；正文接口若返回诱饵请先打开 Clash Verge（mixed-port 7897）"
            );
        } else {
            log::info!(
                "将自动使用本机代理：{}",
                local
                    .iter()
                    .map(ProxySpec::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // 笔趣阁一类源在突发并发后会整站换成诱饵正文；默认压到温和速率。
    // 全书采集另有章间隔休眠（见 commands::collect_delay_ms）。
    let mut fetcher = Fetcher::with_clock(1.0, 2, clock.clone())
        .map_err(|error| format!("初始化采集客户端失败: {error}"))?;
    if let Some(pool) = proxies {
        fetcher = fetcher.with_proxies(pool);
    }
    let fetcher = Arc::new(fetcher);

    let config = if replay_mode {
        PublicWebConfig::new()
    } else {
        PublicWebConfig::new()
            .enabled()
            .breaker(15, 120.0)
    };
    let transport: Arc<dyn Transport> = fetcher;
    let public_web = PublicWebTier::new(transport, clock, config);

    let mut tiers: Vec<Box<dyn TierSource>> = vec![
        Box::new(UnconfiguredTier::official_api()),
        Box::new(UnconfiguredTier::authorized_data()),
        Box::new(public_web),
    ];

    if replay_mode {
        let replay = ReplayTier::from_dirs(&dirs)
            .map_err(|error| format!("加载离线回放目录失败: {error}"))?;
        log::info!("回放模式：公开页面抓取已关闭，{}", replay.describe());
        tiers.push(Box::new(replay));
    }

    Ok(Arc::new(Ladder::new(tiers)))
}

/// 启动阶段的致命错误：写进数据目录一份，用户能把文件发出来。
///
/// 这类失败（库被锁、目录不可写）只会让前端看到「每个命令都 state not managed」，
/// 光靠 stderr 无从排查。
fn report_startup_failure(dir: &std::path::Path, message: String) -> Box<dyn std::error::Error> {
    let path = dir.join("startup-error.log");
    let _ = std::fs::write(&path, format!("{message}\n"));
    log::error!("启动失败：{message}（已写入 {}）", path.display());
    message.into()
}

/// 内置书源按 URL 补种：已存在则不动（避免覆盖用户的停用选择）。
fn ensure_builtin_sources(store: &Store) -> Result<(), String> {
    let source = crate::engine::bqg::builtin_source();
    let exists = store
        .list_sources()
        .map_err(|error| error.to_string())?
        .iter()
        .any(|item| item.url.eq_ignore_ascii_case(&source.book_source_url));
    if exists {
        return Ok(());
    }
    store.upsert_source(&source).map_err(|error| error.to_string())?;
    log::info!("已写入内置书源：{}", source.book_source_name);
    Ok(())
}

/// 阅读浮窗全局快捷键：macOS ⌥⌘M；Windows/Linux 见 [`overlay_shortcut_candidates`]，
/// 依次尝试 Ctrl+Alt+M、Ctrl+Shift+M、Alt+Shift+M、Ctrl+Alt+O。
fn toggle_overlay(app: &AppHandle) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        // 走 `show_overlay` 而不是裸 `show()`：它显示之后会广播 `overlay://session`，
        // 让浮窗重新拉取当前章节。否则用快捷键唤起的浮窗会停在旧内容上 ——
        // 与主窗口点「阅读」时是同一个坑。
        let _ = crate::commands::show_overlay(app);
    }
}

/// 本次实际绑定成功的浮窗快捷键（给设置页展示用）。
pub struct BoundShortcut(pub std::sync::Mutex<Option<String>>);

/// 阅读浮窗的候选快捷键：按顺序尝试，被别的程序占用就退让到下一个。
///
/// macOS 上没有这个烦恼（⌥⌘M 极少冲突），Windows 上 Win+ 系组合几乎必冲突，
/// Ctrl+Alt 系也常被输入法/词典/音乐播放器注册，所以准备了一串备选。
fn overlay_shortcut_candidates() -> Vec<(Shortcut, &'static str)> {
    #[cfg(target_os = "macos")]
    {
        vec![(
            Shortcut::new(Some(Modifiers::ALT | Modifiers::SUPER), Code::KeyM),
            "⌥⌘M",
        )]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![
            (
                Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyM),
                "Ctrl+Alt+M",
            ),
            (
                Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyM),
                "Ctrl+Shift+M",
            ),
            (
                Shortcut::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::KeyM),
                "Alt+Shift+M",
            ),
            (
                Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyO),
                "Ctrl+Alt+O",
            ),
        ]
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let overlay_shortcuts = Arc::new(overlay_shortcut_candidates());
    let shortcuts_for_handler = overlay_shortcuts.clone();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if shortcuts_for_handler.iter().any(|(item, _)| item == shortcut) {
                        toggle_overlay(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let handle = app.handle().clone();

            // 数据目录：默认在程序根目录的 docs 下，用户可在「设置」页自定义。
            let data_dir = crate::settings::resolve(&handle);
            crate::settings::migrate_legacy_db(&handle, &data_dir);

            let store = match Store::open(&data_dir.join("novel-overlay.db")) {
                Ok(store) => store,
                Err(error) => {
                    return Err(report_startup_failure(
                        &data_dir,
                        format!("初始化数据库失败: {error:#}"),
                    ));
                }
            };
            if let Err(error) = ensure_builtin_sources(&store) {
                return Err(report_startup_failure(
                    &data_dir,
                    format!("写入内置书源失败: {error}"),
                ));
            }
            let pages = match build_ladder() {
                Ok(pages) => pages,
                Err(error) => {
                    return Err(report_startup_failure(
                        &data_dir,
                        format!("装配采集阶梯失败: {error}"),
                    ));
                }
            };
            log::info!("采集阶梯：{}", pages.describe());

            // 内嵌 JS 引擎失败不该让整个应用起不来：降级为直通，只是带签名的书源用不了。
            let js: Box<dyn JsEvaluator> = match QuickJsEvaluator::spawn() {
                Ok(evaluator) => {
                    log::info!("内嵌 QuickJS 就绪，<js> / js: 规则可执行站点签名");
                    Box::new(evaluator)
                }
                Err(error) => {
                    log::warn!("QuickJS 初始化失败，js 规则将直通（带签名的书源会失效）：{error}");
                    Box::new(PassThroughJs)
                }
            };

            app.manage(AppState::new(
                store,
                pages,
                js,
                crate::commands::DataDirInfo {
                    active: data_dir.clone(),
                    default: crate::settings::default_dir(&handle),
                },
            ));

            // 浮窗要「纯透明只留字」：绝不能挂 NSVisualEffectView 毛玻璃。
            // 底板深浅只靠 CSS rgba；需要半透明底板时也只用 CSS，不再用 vibrancy。
            #[cfg(target_os = "macos")]
            if let Some(overlay) = handle.get_webview_window("overlay") {
                crate::commands::prepare_overlay_window(&overlay);
            }

            // 快捷键被其他程序占用时不该让整个应用起不来：依次尝试候选组合，
            // 全被占就降级为无全局快捷键（浮窗仍可从主窗口打开）。
            let mut bound: Option<&'static str> = None;
            for (candidate, label) in overlay_shortcuts.iter() {
                match handle.global_shortcut().register(candidate.clone()) {
                    Ok(()) => {
                        bound = Some(label);
                        break;
                    }
                    Err(error) => log::debug!("快捷键 {label} 注册失败: {error}"),
                }
            }
            app.manage(BoundShortcut(std::sync::Mutex::new(
                bound.map(str::to_string),
            )));
            match bound {
                Some(label) => log::info!("阅读浮窗快捷键：{label}"),
                None => log::warn!("候选快捷键全部被占用，浮窗请从主窗口按钮打开"),
            }

            log::info!("novel-overlay 就绪，数据目录 {}", data_dir.display());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_sources,
            commands::add_source,
            commands::set_source_enabled,
            commands::delete_source,
            commands::source_json,
            commands::update_source,
            commands::search_novel,
            commands::fetch_toc,
            commands::cached_chapter_indices,
            commands::fetch_chapter,
            commands::collect_book,
            commands::prefetch_book,
            commands::shelf_list,
            commands::shelf_add,
            commands::shelf_remove,
            commands::shelf_progress,
            commands::start_reading,
            commands::overlay_get_chapter,
            commands::overlay_next_chapter,
            commands::overlay_prev_chapter,
            commands::overlay_goto_chapter,
            commands::open_overlay,
            commands::close_overlay,
            commands::overlay_state,
            commands::overlay_set_text_only,
            commands::data_settings,
            commands::set_data_dir,
            commands::overlay_shortcut,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}

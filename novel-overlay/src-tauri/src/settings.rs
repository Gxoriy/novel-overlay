//! 数据目录设置。
//!
//! 默认目录是**程序根目录下的 `docs`**：
//! - 开发态（`npm run tauri dev`）指仓库根目录，即 `<repo>/docs`；
//! - 打包后指可执行文件所在目录，即 `<安装目录>/docs`。
//!
//! 用户可以在「设置」页改成任意绝对路径。覆盖值存在
//! `app_config_dir()/settings.json`，不能存在数据目录里 ——
//! 否则「数据库在哪」和「记录数据库在哪」会互相依赖。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 落在配置文件里的内容。字段名用 camelCase，方便手改。
#[derive(Debug, Default, Deserialize, Serialize)]
struct SettingsFile {
    #[serde(default, rename = "dataDir")]
    data_dir: Option<String>,
}

/// 返回给前端的设置快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSettings {
    /// 本次进程实际使用的数据目录。
    pub active_dir: String,
    /// 默认数据目录（未自定义时使用）。
    pub default_dir: String,
    /// 已保存的自定义目录；None 表示跟随默认。
    pub custom_dir: Option<String>,
    /// 已保存的设置与当前进程不一致 —— 需要重启才生效。
    pub restart_required: bool,
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 配置文件路径：`<app_config_dir>/settings.json`。
fn settings_file(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|error| format!("定位配置目录失败: {error}"))
}

fn load(app: &AppHandle) -> SettingsFile {
    let Ok(path) = settings_file(app) else {
        return SettingsFile::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return SettingsFile::default();
    };
    // 记事本一类编辑器会写 UTF-8 BOM，serde 不认。
    let raw = raw.trim_start_matches('\u{feff}');
    serde_json::from_str(raw).unwrap_or_else(|error| {
        log::warn!("配置文件 {} 解析失败，按默认处理: {error}", path.display());
        SettingsFile::default()
    })
}

fn save(app: &AppHandle, file: &SettingsFile) -> Result<(), String> {
    let path = settings_file(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    let raw = serde_json::to_string_pretty(file)
        .map_err(|error| format!("序列化配置失败: {error}"))?;
    std::fs::write(&path, raw).map_err(|error| format!("写入配置失败: {error}"))
}

/// 向上找带 `package.json` + `src-tauri` 的目录，即仓库根。
fn project_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("package.json").is_file() && dir.join("src-tauri").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 默认数据目录：根目录下的 `docs`。
pub fn default_dir(app: &AppHandle) -> PathBuf {
    if let Some(root) = project_root() {
        return root.join("docs");
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("docs");
        }
    }
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("docs"))
        .unwrap_or_else(|_| PathBuf::from("docs"))
}

/// 用户保存的自定义目录（不保证可用）。
pub fn custom_dir(app: &AppHandle) -> Option<PathBuf> {
    load(app)
        .data_dir
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 建目录并真写一个探针文件 —— 共享盘/只读目录「能建不能写」很常见。
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".novel-overlay-write-test");
    std::fs::write(&probe, b"ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// 校验并落地用户填写的目录，返回规范化后的路径。
pub fn validate_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("目录不能为空".into());
    }
    let candidate = PathBuf::from(trimmed);
    if !candidate.is_absolute() {
        return Err(format!("请填写绝对路径：{trimmed}"));
    }
    ensure_dir(&candidate).map_err(|error| format!("目录不可写（{trimmed}）: {error}"))?;
    Ok(candidate)
}

/// 解析出本次运行要用的数据目录。
///
/// 自定义目录不可用时退回默认目录；默认目录也不可用（例如装在
/// Program Files 里没有写权限）再退回系统应用数据目录。
pub fn resolve(app: &AppHandle) -> PathBuf {
    if let Some(dir) = custom_dir(app) {
        match ensure_dir(&dir) {
            Ok(()) => return dir,
            Err(error) => log::warn!(
                "自定义数据目录 {} 不可用（{error}），改用默认目录",
                dir.display()
            ),
        }
    }

    let dir = default_dir(app);
    if let Err(error) = ensure_dir(&dir) {
        log::warn!(
            "默认数据目录 {} 不可用（{error}），改用系统应用数据目录",
            dir.display()
        );
        if let Ok(fallback) = app.path().app_data_dir() {
            let fallback = fallback.join("docs");
            match ensure_dir(&fallback) {
                Ok(()) => return fallback,
                Err(error) => log::warn!("回退目录 {} 也不可用: {error}", fallback.display()),
            }
        }
    }
    dir
}

/// 旧版本把数据库放在 `app_data_dir` 根下。换了数据目录后首次启动
/// 自动把旧库搬过来，避免用户以为「书全丢了」。
pub fn migrate_legacy_db(app: &AppHandle, data_dir: &Path) {
    if data_dir.join("novel-overlay.db").exists() {
        return;
    }
    let Ok(legacy_dir) = app.path().app_data_dir() else {
        return;
    };
    let legacy = legacy_dir.join("novel-overlay.db");
    if legacy == data_dir.join("novel-overlay.db") || !legacy.exists() {
        return;
    }
    // 数据库开着 WAL 时，最近的写入可能还在 -wal 文件里，必须一起搬。
    let mut migrated = false;
    for suffix in ["", "-wal", "-shm"] {
        let from = legacy_dir.join(format!("novel-overlay.db{suffix}"));
        if !from.exists() {
            continue;
        }
        let to = data_dir.join(format!("novel-overlay.db{suffix}"));
        match std::fs::copy(&from, &to) {
            Ok(_) => migrated |= suffix.is_empty(),
            Err(error) => log::warn!("迁移旧数据库 {} 失败: {error}", from.display()),
        }
    }
    if migrated {
        log::info!(
            "已把旧数据库从 {} 迁移到 {}",
            legacy.display(),
            data_dir.display()
        );
    }
}

/// 当前生效目录 + 已保存设置的快照。
pub fn snapshot(app: &AppHandle, active: &Path) -> DataSettings {
    let custom = custom_dir(app);
    let default = default_dir(app);
    let configured = custom.clone().unwrap_or_else(|| default.clone());
    DataSettings {
        active_dir: display(active),
        default_dir: display(&default),
        custom_dir: custom.as_deref().map(display),
        restart_required: configured != active,
    }
}

/// 保存新的数据目录设置（`None` = 恢复默认），返回最新快照。
pub fn update(app: &AppHandle, raw: Option<&str>, active: &Path) -> Result<DataSettings, String> {
    let file = match raw {
        Some(value) => SettingsFile {
            data_dir: Some(display(&validate_dir(value)?)),
        },
        None => SettingsFile::default(),
    };
    save(app, &file)?;
    Ok(snapshot(app, active))
}

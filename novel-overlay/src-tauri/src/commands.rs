use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager, State};

use crate::engine::bqg::{looks_like_bait, BAIT_ERROR};
use crate::engine::js::JsEvaluator;
use crate::engine::SourceEngine;
use crate::model::{
    BookSource, BookSourceBrief, Chapter, ChapterContent, DownloadProgress, DownloadResult,
    OverlayChapter, OverlayState, SearchBook, ShelfItem,
};
use crate::net::Ladder;
use crate::store::Store;
use crate::BoundShortcut;

pub struct ReadingSession {
    pub source_id: i64,
    pub book_url: String,
    pub book_name: String,
    pub chapters: Vec<Chapter>,
    pub index: i64,
}

/// 数据目录信息：本次进程实际用的、以及未自定义时的默认值。
pub struct DataDirInfo {
    pub active: std::path::PathBuf,
    pub default: std::path::PathBuf,
}

pub struct AppState {
    pub store: Store,
    /// 分级获取阶梯。采集内核只看到 `PageSource`，因此合规闸门、限速、熔断、
    /// 离线回放全部对它是透明的。
    pub pages: Arc<Ladder>,
    pub js: Box<dyn JsEvaluator>,
    pub session: tokio::sync::Mutex<Option<ReadingSession>>,
    pub data: DataDirInfo,
}

impl AppState {
    pub fn new(store: Store, pages: Arc<Ladder>, js: Box<dyn JsEvaluator>, data: DataDirInfo) -> Self {
        Self {
            store,
            pages,
            js,
            session: tokio::sync::Mutex::new(None),
            data,
        }
    }
}

fn err<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

fn parse_sources(raw: &str) -> anyhow::Result<Vec<BookSource>> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let items = match value {
        serde_json::Value::Array(list) => list,
        other => vec![other],
    };
    let mut out = Vec::new();
    for item in items {
        let source: BookSource = serde_json::from_value(item)?;
        if source.book_source_url.trim().is_empty() {
            return Err(anyhow::anyhow!("书源缺少 bookSourceUrl"));
        }
        out.push(source);
    }
    if out.is_empty() {
        return Err(anyhow::anyhow!("没有解析到任何书源"));
    }
    Ok(out)
}

// ---------- 书源 ----------

#[tauri::command]
pub fn list_sources(state: State<'_, AppState>) -> Result<Vec<BookSourceBrief>, String> {
    state.store.list_sources().map_err(err)
}

#[tauri::command]
pub fn add_source(state: State<'_, AppState>, json: String) -> Result<Vec<BookSourceBrief>, String> {
    let sources = parse_sources(&json).map_err(err)?;
    for source in &sources {
        state.store.upsert_source(source).map_err(err)?;
    }
    state.store.list_sources().map_err(err)
}

#[tauri::command]
pub fn set_source_enabled(
    state: State<'_, AppState>,
    id: i64,
    enabled: bool,
) -> Result<(), String> {
    state.store.set_source_enabled(id, enabled).map_err(err)
}

#[tauri::command]
pub fn delete_source(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.store.delete_source(id).map_err(err)
}

/// 导出书源 JSON：`id` 为空导出全部（含停用），否则只导出指定书源。
///
/// 弹出系统保存对话框，用户取消时返回 `None`；成功返回落盘路径。
/// 导出内容与导入格式完全一致（阅读兼容字段），导出的文件可以直接再导入。
#[tauri::command]
pub fn export_sources(
    app: AppHandle,
    state: State<'_, AppState>,
    id: Option<i64>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::{DialogExt, FilePath};

    let sources = match id {
        Some(id) => vec![state.store.get_source(id).map_err(err)?],
        None => state.store.all_sources().map_err(err)?,
    };
    if sources.is_empty() {
        return Err("没有可导出的书源".to_string());
    }
    // 单个导出为对象、多个导出为数组 —— 两种写法导入时都能解析。
    let value = serde_json::to_value(&sources).map_err(err)?;
    let value = match value {
        serde_json::Value::Array(mut list) if list.len() == 1 => list
            .pop()
            .unwrap_or(serde_json::Value::Null),
        other => other,
    };
    let contents = serde_json::to_string_pretty(&value).map_err(err)?;

    let default_name = match id {
        Some(_) => format!(
            "{}.json",
            sanitize_filename(&sources[0].book_source_name)
        ),
        None => "novel-overlay-sources.json".to_string(),
    };
    let picked = app
        .dialog()
        .file()
        .add_filter("书源 JSON", &["json"])
        .set_file_name(&default_name)
        .blocking_save_file();
    let Some(picked) = picked else {
        return Ok(None);
    };
    // FilePath 可能是 Url 形式，统一转成文件系统路径字符串。
    let path = match &picked {
        FilePath::Path(path) => path.display().to_string(),
        FilePath::Url(url) => url
            .to_file_path()
            .map(|path| path.display().to_string())
            .map_err(|_| format!("无法识别的保存路径: {picked}"))?,
    };
    std::fs::write(&path, &contents).map_err(|error| format!("写入 {path} 失败: {error}"))?;
    Ok(Some(path))
}

/// Windows 文件名里不允许出现的字符统一换成空格。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            _ => ch,
        })
        .collect()
}

// ---------- 采集 ----------

#[tauri::command]
pub async fn search_novel(
    state: State<'_, AppState>,
    query: String,
    source_id: Option<i64>,
) -> Result<Vec<SearchBook>, String> {
    let keyword = query.trim().to_string();
    if keyword.is_empty() {
        return Err("搜索关键词为空".into());
    }

    let targets: Vec<(i64, BookSource)> = match source_id {
        Some(id) => vec![(id, state.store.get_source(id).map_err(err)?)],
        None => state
            .store
            .enabled_sources()
            .map_err(err)?
            .into_iter()
            .filter(|(_, source)| !source.book_source_url.contains("fixture.test"))
            .collect(),
    };
    if targets.is_empty() {
        return Err("没有启用中的书源，请先在「书源」页导入".into());
    }

    let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for (id, source) in targets {
        match engine.search(&source, &keyword).await {
            Ok(mut books) => {
                for book in books.iter_mut() {
                    book.source_id = id;
                    book.source_name = source.book_source_name.clone();
                }
                results.extend(books);
            }
            Err(error) => failures.push(format!("{}：{error}", source.book_source_name)),
        }
    }
    if results.is_empty() && !failures.is_empty() {
        return Err(failures.join("；"));
    }
    Ok(results)
}

#[tauri::command]
pub fn cached_chapter_indices(
    state: State<'_, AppState>,
    source_id: i64,
    book_url: String,
) -> Result<Vec<i64>, String> {
    state
        .store
        .chapter_cache_indices(source_id, &book_url)
        .map_err(err)
}

#[tauri::command]
pub async fn fetch_toc(
    state: State<'_, AppState>,
    source_id: i64,
    book_url: String,
) -> Result<Vec<Chapter>, String> {
    let source = state.store.get_source(source_id).map_err(err)?;
    let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());
    let chapters = engine.toc(&source, &book_url).await.map_err(err)?;
    state
        .store
        .toc_cache_put(source_id, &book_url, &chapters)
        .map_err(err)?;
    Ok(chapters)
}

#[tauri::command]
pub async fn fetch_chapter(
    state: State<'_, AppState>,
    source_id: i64,
    book_url: String,
    idx: i64,
) -> Result<ChapterContent, String> {
    if let Some((name, content)) =
        take_cached_chapter(&state.store, source_id, &book_url, idx).map_err(err)?
    {
        return Ok(ChapterContent {
            index: idx,
            name,
            content,
            from_cache: true,
        });
    }

    let chapters = state
        .store
        .toc_cache_get(source_id, &book_url)
        .map_err(err)?
        .ok_or_else(|| "目录尚未缓存，请先打开目录".to_string())?;
    let chapter = chapters
        .get(idx as usize)
        .ok_or_else(|| format!("章节序号越界: {idx}"))?
        .clone();
    let source = state.store.get_source(source_id).map_err(err)?;
    let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());
    let content = engine.content(&source, &chapter.url).await.map_err(err)?;
    state
        .store
        .chapter_cache_put(source_id, &book_url, idx, &chapter.name, &content)
        .map_err(err)?;
    Ok(ChapterContent {
        index: idx,
        name: chapter.name,
        content,
        from_cache: false,
    })
}

/// 读缓存；若是风控诱饵则删掉并当作未命中，避免书架一直显示假文。
fn take_cached_chapter(
    store: &Store,
    source_id: i64,
    book_url: &str,
    idx: i64,
) -> anyhow::Result<Option<(String, String)>> {
    let Some((name, content)) = store.chapter_cache_get(source_id, book_url, idx)? else {
        return Ok(None);
    };
    if looks_like_bait(&content) {
        log::warn!("丢弃诱饵缓存 source={source_id} book={book_url} idx={idx}");
        store.chapter_cache_delete(source_id, book_url, idx)?;
        return Ok(None);
    }
    Ok(Some((name, content)))
}

/// 搜索结果上的主操作：加入书架，拉最新目录，再按 `limit` 采集章节。
///
/// `limit`：本次最多新采多少章（跳过已缓存）。`None` / `0` / 负数 = 不限制。
#[tauri::command]
pub async fn collect_book(
    app: AppHandle,
    state: State<'_, AppState>,
    book: SearchBook,
    limit: Option<i64>,
) -> Result<DownloadResult, String> {
    state.store.shelf_add(&book).map_err(err)?;
    collect_all_chapters(
        &app,
        &state,
        book.source_id,
        book.book_url,
        Some(book.name),
        limit,
    )
    .await
}

/// 按目录把章节写入本地缓存。已缓存的章节跳过。
///
/// 进度通过 `download://progress` 广播，调用本身在全部章节处理完后返回汇总。
#[tauri::command]
pub async fn prefetch_book(
    app: AppHandle,
    state: State<'_, AppState>,
    source_id: i64,
    book_url: String,
    book_name: Option<String>,
    limit: Option<i64>,
) -> Result<DownloadResult, String> {
    collect_all_chapters(&app, &state, source_id, book_url, book_name, limit).await
}

async fn collect_all_chapters(
    app: &AppHandle,
    state: &AppState,
    source_id: i64,
    book_url: String,
    book_name: Option<String>,
    limit: Option<i64>,
) -> Result<DownloadResult, String> {
    let source = state.store.get_source(source_id).map_err(err)?;
    let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());

    // 采集前清掉已入库的诱饵，否则会一直「跳过缓存」显示假书。
    let purged = state
        .store
        .chapter_cache_purge_where(source_id, &book_url, looks_like_bait)
        .map_err(err)?;
    if purged > 0 {
        log::warn!("采集前清除诱饵缓存 {purged} 章：{book_url}");
    }

    // 已有目录缓存则复用，避免采集前再打 book/booklist 把限流打得更死。
    let chapters = match state
        .store
        .toc_cache_get(source_id, &book_url)
        .map_err(err)?
    {
        Some(cached) if !cached.is_empty() => {
            log::info!("复用目录缓存 {} 章，跳过采集前的 toc 请求", cached.len());
            cached
        }
        _ => {
            let chapters = engine.toc(&source, &book_url).await.map_err(err)?;
            state
                .store
                .toc_cache_put(source_id, &book_url, &chapters)
                .map_err(err)?;
            chapters
        }
    };
    if chapters.is_empty() {
        return Err("目录为空，无法采集".into());
    }

    let title = book_name
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            state.store.shelf_list().ok().and_then(|items| {
                items
                    .into_iter()
                    .find(|item| item.source_id == source_id && item.book_url == book_url)
                    .map(|item| item.name)
            })
        })
        .unwrap_or_else(|| "未命名".into());

    let book_total = chapters.len() as i64;
    let mut pending = Vec::new();
    let mut from_cache = 0i64;
    let mut cached_idx = Vec::new();
    for chapter in chapters {
        match take_cached_chapter(&state.store, source_id, &book_url, chapter.index).map_err(err)? {
            Some(_) => {
                from_cache += 1;
                cached_idx.push(chapter.index);
            }
            None => pending.push(chapter),
        }
    }

    // 补采应接着往后读，而不是先填目录开头的窟窿（如未采的「楔子」）。
    // 否则第一条就撞风控时，整批中止，真正缺的后续章永远采不到。
    let prefer_from = state
        .store
        .shelf_list()
        .ok()
        .and_then(|items| {
            items
                .into_iter()
                .find(|item| item.source_id == source_id && item.book_url == book_url)
                .map(|item| item.last_read_index)
        })
        .unwrap_or_else(|| continue_from_cached_prefix(&cached_idx));
    order_pending_for_readthrough(&mut pending, prefer_from);

    let cap = limit
        .filter(|&n| n > 0)
        .map(|n| n as usize)
        .unwrap_or(usize::MAX);
    if pending.len() > cap {
        pending.truncate(cap);
    }

    // 本次任务进度按「本批待采章数」计；全书章数只写在日志里。
    let total = pending.len() as i64;
    let first_name = pending
        .first()
        .map(|chapter| chapter.name.clone())
        .unwrap_or_default();
    let batch_note = if limit.filter(|&n| n > 0).is_some() {
        format!("本批最多 {total} 章（全书 {book_total}，从「{first_name}」起）")
    } else {
        format!("全书未缓存 {total} 章（从「{first_name}」起）")
    };

    let _ = tauri::Emitter::emit(
        app,
        "download://progress",
        DownloadProgress {
            source_id,
            book_url: book_url.clone(),
            book_name: title.clone(),
            done: 0,
            total,
            chapter_name: if pending.is_empty() {
                format!("无需新采（已缓存 {from_cache} / {book_total}）")
            } else {
                format!(
                    "跳过已缓存 {from_cache} 章，{batch_note}，间隔 {}ms",
                    collect_delay_ms()
                )
            },
            from_cache: true,
            complete: pending.is_empty(),
        },
    );

    let delay = Duration::from_millis(collect_delay_ms());
    let cooldown = Duration::from_secs(bait_cooldown_secs());
    let bait_retries = bait_retry_limit();
    let downloaded;
    let failed;
    let mut bait_blocked = false;
    if pending.is_empty() {
        downloaded = 0;
        failed = 0;
    } else {
        log::info!(
            "串行慢采「{title}」{} 章，章间隔 {}ms，诱饵冷却 {}s",
            pending.len(),
            delay.as_millis(),
            cooldown.as_secs()
        );
        let mut got = 0i64;
        let mut miss = 0i64;
        let mut first = true;
        let pending_len = pending.len() as i64;

        for chapter in pending {
            if !first {
                tokio::time::sleep(delay).await;
            }
            first = false;

            let mut result = None;
            for attempt in 0..=bait_retries {
                let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());
                match engine.content(&source, &chapter.url).await {
                    Ok(content) if looks_like_bait(&content) => {
                        result = Some(Err(anyhow::anyhow!(BAIT_ERROR)));
                    }
                    Ok(content) => {
                        result = Some(Ok(content));
                        break;
                    }
                    Err(error) => {
                        let is_bait = error.to_string().contains(BAIT_ERROR);
                        result = Some(Err(error));
                        if !is_bait {
                            break;
                        }
                    }
                }

                if attempt < bait_retries {
                    let wait_secs = cooldown.as_secs() * (attempt as u64 + 1);
                    log::warn!(
                        "命中风控诱饵，休眠 {wait_secs}s 后重试 idx={}（{}/{}）",
                        chapter.index,
                        attempt + 1,
                        bait_retries
                    );
                    let _ = tauri::Emitter::emit(
                        app,
                        "download://progress",
                        DownloadProgress {
                            source_id,
                            book_url: book_url.clone(),
                            book_name: title.clone(),
                            done: got + miss,
                            total,
                            chapter_name: format!(
                                "风控休眠 {}s 后重试：{}",
                                wait_secs, chapter.name
                            ),
                            from_cache: false,
                            complete: false,
                        },
                    );
                    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                }
            }

            match result.unwrap_or_else(|| Err(anyhow::anyhow!("空结果"))) {
                Ok(content) => {
                    if let Err(error) = state.store.chapter_cache_put(
                        source_id,
                        &book_url,
                        chapter.index,
                        &chapter.name,
                        &content,
                    ) {
                        log::warn!(
                            "写入章节缓存失败 source={source_id} idx={}: {error}",
                            chapter.index
                        );
                        miss += 1;
                    } else {
                        got += 1;
                    }
                }
                Err(error) => {
                    let msg = error.to_string();
                    log::warn!(
                        "采集章节失败 source={source_id} book={book_url} idx={}: {msg}",
                        chapter.index
                    );
                    miss += 1;
                    if msg.contains(BAIT_ERROR) {
                        bait_blocked = true;
                        // 重试耗尽仍是诱饵：留下已采章节，停止继续撞墙。
                        let _ = tauri::Emitter::emit(
                            app,
                            "download://progress",
                            DownloadProgress {
                                source_id,
                                book_url: book_url.clone(),
                                book_name: title.clone(),
                                done: got + miss,
                                total,
                                chapter_name: format!("已中止（风控）：{}", chapter.name),
                                from_cache: false,
                                complete: true,
                            },
                        );
                        break;
                    }
                }
            }

            let _ = tauri::Emitter::emit(
                app,
                "download://progress",
                DownloadProgress {
                    source_id,
                    book_url: book_url.clone(),
                    book_name: title.clone(),
                    done: got + miss,
                    total,
                    chapter_name: chapter.name,
                    from_cache: false,
                    complete: (got + miss) >= pending_len || bait_blocked,
                },
            );
        }
        downloaded = got;
        failed = miss;
    }

    if downloaded == 0 && from_cache == 0 && total > 0 {
        if bait_blocked {
            return Err(BAIT_ERROR.into());
        }
        return Err("采集失败，没有缓存到任何章节".into());
    }

    Ok(DownloadResult {
        source_id,
        book_url,
        total,
        downloaded,
        from_cache,
        failed,
    })
}

/// 按「接着往后看」排序：优先 `prefer_from` 及之后的缺口，开头的窟窿放到后面。
fn order_pending_for_readthrough(pending: &mut Vec<crate::model::Chapter>, prefer_from: i64) {
    pending.sort_by_key(|chapter| {
        let lagging_hole = if chapter.index >= prefer_from { 0 } else { 1 };
        (lagging_hole, chapter.index)
    });
}

/// 已缓存前缀之后的第一处缺口。允许缺第 0 章（楔子），从 1 起算连续段。
fn continue_from_cached_prefix(cached_idx: &[i64]) -> i64 {
    if cached_idx.is_empty() {
        return 0;
    }
    let set: std::collections::HashSet<i64> = cached_idx.iter().copied().collect();
    let start = if set.contains(&0) { 0 } else { 1 };
    if start == 1 && !set.contains(&1) {
        return 0;
    }
    let mut run = start;
    while set.contains(&run) {
        run += 1;
    }
    run
}

/// 章与章之间的固定休眠。默认 2s，可用 `NOVEL_OVERLAY_COLLECT_DELAY_MS` 覆盖。
fn collect_delay_ms() -> u64 {
    std::env::var("NOVEL_OVERLAY_COLLECT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000)
        .clamp(200, 30_000)
}

/// 命中诱饵后的基础冷却。默认不再长睡重试（越重试封得越死）。
fn bait_cooldown_secs() -> u64 {
    std::env::var("NOVEL_OVERLAY_BAIT_COOLDOWN_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(45)
        .clamp(15, 600)
}

/// 诱饵重试次数。默认 0：一次假文就停，避免把 IP 限流打得更死。
fn bait_retry_limit() -> u32 {
    std::env::var("NOVEL_OVERLAY_BAIT_RETRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
        .clamp(0, 8)
}

// ---------- 书架 ----------

#[tauri::command]
pub fn shelf_list(state: State<'_, AppState>) -> Result<Vec<ShelfItem>, String> {
    state.store.shelf_list().map_err(err)
}

#[tauri::command]
pub fn shelf_add(state: State<'_, AppState>, book: SearchBook) -> Result<i64, String> {
    state.store.shelf_add(&book).map_err(err)
}

#[tauri::command]
pub fn shelf_remove(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.store.shelf_remove(id).map_err(err)
}

#[tauri::command]
pub fn shelf_progress(
    state: State<'_, AppState>,
    id: i64,
    idx: i64,
    offset: f64,
) -> Result<(), String> {
    state.store.shelf_progress(id, idx, offset).map_err(err)
}

// ---------- 阅读会话 ----------

#[tauri::command]
pub async fn start_reading(
    app: AppHandle,
    state: State<'_, AppState>,
    source_id: i64,
    book_url: String,
    book_name: String,
    idx: i64,
) -> Result<(), String> {
    let cached = state
        .store
        .toc_cache_get(source_id, &book_url)
        .map_err(err)?
        .filter(|list| !list.is_empty());

    let chapters = match cached {
        Some(list) => list,
        None => {
            let source = state.store.get_source(source_id).map_err(err)?;
            let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());
            let list = engine.toc(&source, &book_url).await.map_err(err)?;
            state
                .store
                .toc_cache_put(source_id, &book_url, &list)
                .map_err(err)?;
            list
        }
    };
    if chapters.is_empty() {
        return Err("目录为空，无法开始阅读".into());
    }

    let index = idx.clamp(0, chapters.len() as i64 - 1);
    {
        let mut guard = state.session.lock().await;
        *guard = Some(ReadingSession {
            source_id,
            book_url: book_url.clone(),
            book_name,
            chapters,
            index,
        });
    }
    show_overlay(&app).map_err(err)
}

async fn chapter_at(state: &AppState, index: i64) -> Result<Option<OverlayChapter>, String> {
    let (source_id, book_url, book_name, chapters) = {
        let guard = state.session.lock().await;
        match guard.as_ref() {
            Some(session) => (
                session.source_id,
                session.book_url.clone(),
                session.book_name.clone(),
                session.chapters.clone(),
            ),
            None => return Ok(None),
        }
    };
    if chapters.is_empty() {
        return Ok(None);
    }

    let clamped = index.clamp(0, chapters.len() as i64 - 1);
    let chapter = chapters[clamped as usize].clone();

    let content = match take_cached_chapter(&state.store, source_id, &book_url, clamped)
        .map_err(err)?
    {
        Some((_, text)) => text,
        None => {
            let source = state.store.get_source(source_id).map_err(err)?;
            let engine = SourceEngine::new(state.pages.as_ref(), state.js.as_ref());
            let text = engine.content(&source, &chapter.url).await.map_err(err)?;
            state
                .store
                .chapter_cache_put(source_id, &book_url, clamped, &chapter.name, &text)
                .map_err(err)?;
            text
        }
    };

    {
        let mut guard = state.session.lock().await;
        if let Some(session) = guard.as_mut() {
            session.index = clamped;
        }
    }
    let _ = state
        .store
        .shelf_progress_by_book(source_id, &book_url, clamped, 0.0);

    Ok(Some(OverlayChapter {
        book_name,
        index: clamped,
        total: chapters.len() as i64,
        name: chapter.name,
        content,
    }))
}

#[tauri::command]
pub async fn overlay_get_chapter(
    state: State<'_, AppState>,
) -> Result<Option<OverlayChapter>, String> {
    let index = { state.session.lock().await.as_ref().map(|session| session.index) };
    match index {
        Some(value) => chapter_at(&state, value).await,
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn overlay_next_chapter(
    state: State<'_, AppState>,
) -> Result<Option<OverlayChapter>, String> {
    let index = {
        state
            .session
            .lock()
            .await
            .as_ref()
            .map(|session| session.index + 1)
    };
    match index {
        Some(value) => chapter_at(&state, value).await,
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn overlay_prev_chapter(
    state: State<'_, AppState>,
) -> Result<Option<OverlayChapter>, String> {
    let index = {
        state
            .session
            .lock()
            .await
            .as_ref()
            .map(|session| session.index - 1)
    };
    match index {
        Some(value) => chapter_at(&state, value).await,
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn overlay_goto_chapter(
    state: State<'_, AppState>,
    idx: i64,
) -> Result<Option<OverlayChapter>, String> {
    chapter_at(&state, idx).await
}

// ---------- 浮窗控制 ----------

/// 显示浮窗并广播 `overlay://session`，让浮窗重新拉取当前章节。
///
/// `pub(crate)` 是因为 ⌥⌘M 的处理器（在 `lib.rs`）也要走同一条路径 ——
/// 否则用快捷键唤起的浮窗会停在旧内容上。
pub(crate) fn show_overlay(app: &AppHandle) -> anyhow::Result<()> {
    let window = app
        .get_webview_window("overlay")
        .ok_or_else(|| anyhow::anyhow!("阅读浮窗不存在"))?;
    #[cfg(target_os = "macos")]
    prepare_overlay_window(&window);
    window.show()?;
    window.set_always_on_top(true)?;
    // 通知浮窗重新拉取当前章节。
    //
    // 浮窗在应用启动时就已加载（窗口配置里 `visible: false`），那一刻还没有阅读
    // 会话，它渲染的是「未开始阅读」。之后 `show()` 只是把窗口显示出来，**不会
    // 触发任何重新加载** —— 不广播这个事件的话，用户点完「阅读」会一直盯着
    // 「未开始阅读」，直到随手按一下方向键才突然出现正文。
    let _ = tauri::Emitter::emit(app, "overlay://session", ());
    Ok(())
}

#[tauri::command]
pub fn open_overlay(app: AppHandle) -> Result<(), String> {
    show_overlay(&app).map_err(err)
}

#[tauri::command]
pub fn close_overlay(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("overlay")
        .ok_or_else(|| "阅读浮窗不存在".to_string())?;
    window.hide().map_err(err)
}

#[tauri::command]
pub fn overlay_state(app: AppHandle) -> Result<OverlayState, String> {
    let visible = app
        .get_webview_window("overlay")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false);
    Ok(OverlayState { visible })
}

/// 纯文字模式：确保原生层没有毛玻璃（底板透明度由前端 CSS 控制）。
#[tauri::command]
pub fn overlay_set_text_only(app: AppHandle, enabled: bool) -> Result<(), String> {
    let _ = enabled;
    let window = app
        .get_webview_window("overlay")
        .ok_or_else(|| "阅读浮窗不存在".to_string())?;
    #[cfg(target_os = "macos")]
    {
        // clear_vibrancy 必须在主线程；invoke 默认不在主线程，直接调会静默失败。
        let window = window.clone();
        let _ = app.run_on_main_thread(move || {
            prepare_overlay_window(&window);
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
    }
    Ok(())
}

// ---------- 设置 ----------

/// 本次实际绑定成功的浮窗快捷键；`None` 表示候选组合全被占用。
#[tauri::command]
pub fn overlay_shortcut(state: State<'_, BoundShortcut>) -> Option<String> {
    state.0.lock().ok().and_then(|value| value.clone())
}

/// 当前数据目录 + 已保存的自定义设置。
#[tauri::command]
pub fn data_settings(app: AppHandle, state: State<'_, AppState>) -> crate::settings::DataSettings {
    crate::settings::snapshot(&app, &state.data.active)
}

/// 保存数据目录设置（`path` 为 `None` 表示恢复默认）。下次启动生效。
#[tauri::command]
pub fn set_data_dir(
    app: AppHandle,
    state: State<'_, AppState>,
    path: Option<String>,
) -> Result<crate::settings::DataSettings, String> {
    crate::settings::update(&app, path.as_deref(), &state.data.active)
}

/// 清掉毛玻璃，并把 NSWindow 背景设成真正透明。
#[cfg(target_os = "macos")]
pub(crate) fn prepare_overlay_window(window: &tauri::WebviewWindow) {
    use window_vibrancy::clear_vibrancy;

    if let Err(error) = clear_vibrancy(window) {
        log::debug!("clear_vibrancy: {error}");
    }

    // 窗口层背景也必须是 clear，否则 transparent webview 背后仍是灰底。
    match window.ns_window() {
        Ok(ptr) if !ptr.is_null() => unsafe {
            use objc2_app_kit::{NSColor, NSWindow};
            let ns_window = &*ptr.cast::<NSWindow>();
            let clear = NSColor::clearColor();
            ns_window.setBackgroundColor(Some(&clear));
            ns_window.setOpaque(false);
            ns_window.setHasShadow(false);
        },
        Ok(_) => log::warn!("浮窗 ns_window 为空"),
        Err(error) => log::warn!("读取浮窗 ns_window 失败: {error}"),
    }

    if let Err(error) = window.set_background_color(Some(tauri::window::Color(0, 0, 0, 0))) {
        log::debug!("set_background_color: {error}");
    }
}

#[cfg(test)]
mod collect_order_tests {
    use super::{continue_from_cached_prefix, order_pending_for_readthrough};
    use crate::model::Chapter;

    fn ch(index: i64, name: &str) -> Chapter {
        Chapter {
            index,
            name: name.into(),
            url: format!("https://example.test/{index}"),
        }
    }

    #[test]
    fn skips_prologue_hole_when_mid_book_cached() {
        // 缺楔子(0)，已有 1..=66 → 应从 67 接着采
        assert_eq!(
            continue_from_cached_prefix(&(1..=66).collect::<Vec<_>>()),
            67
        );
    }

    #[test]
    fn orders_forward_chapters_before_early_holes() {
        let mut pending = vec![ch(0, "楔子"), ch(67, "第六十七章"), ch(68, "第六十八章")];
        order_pending_for_readthrough(&mut pending, 67);
        assert_eq!(
            pending.iter().map(|c| c.index).collect::<Vec<_>>(),
            vec![67, 68, 0]
        );
    }
}

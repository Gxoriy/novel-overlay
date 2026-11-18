use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{BookSource, BookSourceBrief, Chapter, SearchBook, ShelfItem};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS book_source (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL,
    url        TEXT NOT NULL UNIQUE,
    payload    TEXT NOT NULL,
    enabled    INTEGER NOT NULL DEFAULT 1,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS shelf (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id        INTEGER NOT NULL,
    book_url         TEXT NOT NULL,
    name             TEXT NOT NULL,
    author           TEXT NOT NULL DEFAULT '',
    intro            TEXT NOT NULL DEFAULT '',
    last_read_index  INTEGER NOT NULL DEFAULT 0,
    last_read_offset REAL NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    UNIQUE (source_id, book_url)
);

CREATE TABLE IF NOT EXISTS toc_cache (
    source_id INTEGER NOT NULL,
    book_url  TEXT NOT NULL,
    payload   TEXT NOT NULL,
    cached_at INTEGER NOT NULL,
    PRIMARY KEY (source_id, book_url)
);

CREATE TABLE IF NOT EXISTS chapter_cache (
    source_id INTEGER NOT NULL,
    book_url  TEXT NOT NULL,
    idx       INTEGER NOT NULL,
    name      TEXT NOT NULL DEFAULT '',
    content   TEXT NOT NULL,
    cached_at INTEGER NOT NULL,
    PRIMARY KEY (source_id, book_url, idx)
);
"#;

pub struct Store {
    conn: Mutex<Connection>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default()
}

/// SQLite 的 TEXT 列也可能以 BLOB 亲和性存着同一段 UTF-8（早期夹具导入就是这样）。
/// rusqlite 的 `get::<_, String>` 遇到 Blob 会直接报 `Invalid column type Blob`。
fn sqlite_text(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<String> {
    match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
            Ok(String::from_utf8_lossy(bytes).into_owned())
        }
        rusqlite::types::ValueRef::Null => Ok(String::new()),
        other => Err(rusqlite::Error::InvalidColumnType(
            idx,
            row.as_ref()
                .column_name(idx)
                .unwrap_or("?")
                .to_string(),
            other.data_type(),
        )),
    }
}

fn normalize_text_blobs(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE book_source SET payload = CAST(payload AS TEXT) WHERE typeof(payload) = 'blob'",
        [],
    )?;
    conn.execute(
        "UPDATE toc_cache SET payload = CAST(payload AS TEXT) WHERE typeof(payload) = 'blob'",
        [],
    )?;
    conn.execute(
        "UPDATE chapter_cache SET name = CAST(name AS TEXT) WHERE typeof(name) = 'blob'",
        [],
    )?;
    conn.execute(
        "UPDATE chapter_cache SET content = CAST(content AS TEXT) WHERE typeof(content) = 'blob'",
        [],
    )?;
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        // 应用多开、或采集探针正在写库时，SQLite 默认立刻返回 `database is locked`。
        // 启动阶段只是建表和几条短写入，等几秒就能过；不设等待会让整个 setup 失败，
        // 前端表现为「书源为空 + 每个命令都报 state not managed」。
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("设置数据库忙等超时失败")?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        normalize_text_blobs(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        normalize_text_blobs(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| anyhow!("数据库连接锁已损坏"))
    }

    // ---------- 书源 ----------

    pub fn list_sources(&self) -> Result<Vec<BookSourceBrief>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT id, name, url, enabled FROM book_source ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(BookSourceBrief {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                enabled: row.get::<_, i64>(3)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn upsert_source(&self, source: &BookSource) -> Result<i64> {
        let payload = serde_json::to_string(source)?;
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO book_source (name, url, payload, enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(url) DO UPDATE SET
                 name = excluded.name,
                 payload = excluded.payload,
                 enabled = excluded.enabled,
                 updated_at = excluded.updated_at",
            params![
                source.book_source_name,
                source.book_source_url,
                payload,
                source.enabled as i64,
                now_secs()
            ],
        )?;
        let id = conn.query_row(
            "SELECT id FROM book_source WHERE url = ?1",
            params![source.book_source_url],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    pub fn get_source(&self, id: i64) -> Result<BookSource> {
        let conn = self.conn()?;
        let payload = conn
            .query_row(
                "SELECT payload FROM book_source WHERE id = ?1",
                params![id],
                |row| sqlite_text(row, 0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("书源不存在: id={id}"))?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub fn enabled_sources(&self) -> Result<Vec<(i64, BookSource)>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT id, payload FROM book_source WHERE enabled = 1 ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, sqlite_text(row, 1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, payload) = row?;
            match serde_json::from_str::<BookSource>(&payload) {
                Ok(source) => out.push((id, source)),
                Err(err) => log::warn!("跳过无法解析的书源 id={id}: {err}"),
            }
        }
        Ok(out)
    }

    /// 按 id 覆盖书源（编辑保存用）。URL 与其它书源重复时由 UNIQUE 约束报错。
    pub fn update_source(&self, id: i64, source: &BookSource) -> Result<()> {
        let payload = serde_json::to_string(source)?;
        let conn = self.conn()?;
        let affected = conn.execute(
            "UPDATE book_source
             SET name = ?1, url = ?2, payload = ?3, enabled = ?4, updated_at = ?5
             WHERE id = ?6",
            params![
                source.book_source_name,
                source.book_source_url,
                payload,
                source.enabled as i64,
                now_secs(),
                id
            ],
        )?;
        if affected == 0 {
            return Err(anyhow!("书源不存在: id={id}"));
        }
        Ok(())
    }

    /// 全部书源（含停用的），导出 JSON 用。
    pub fn all_sources(&self) -> Result<Vec<BookSource>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT payload FROM book_source ORDER BY id")?;
        let rows = stmt.query_map([], |row| sqlite_text(row, 0))?;
        let mut out = Vec::new();
        for (index, row) in rows.enumerate() {
            let payload = row?;
            match serde_json::from_str::<BookSource>(&payload) {
                Ok(source) => out.push(source),
                Err(err) => log::warn!("跳过无法解析的书源 #{}: {err}", index + 1),
            }
        }
        Ok(out)
    }

    pub fn set_source_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE book_source SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, enabled as i64, now_secs()],
        )?;
        Ok(())
    }

    pub fn delete_source(&self, id: i64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM book_source WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---------- 书架 ----------

    pub fn shelf_add(&self, book: &SearchBook) -> Result<i64> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO shelf (source_id, book_url, name, author, intro, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(source_id, book_url) DO UPDATE SET
                 name = CASE WHEN excluded.name = '' THEN name ELSE excluded.name END,
                 author = CASE WHEN excluded.author = '' THEN author ELSE excluded.author END,
                 intro = CASE WHEN excluded.intro = '' THEN intro ELSE excluded.intro END,
                 updated_at = excluded.updated_at",
            params![
                book.source_id,
                book.book_url,
                book.name,
                book.author,
                book.intro,
                now_secs()
            ],
        )?;
        let id = conn.query_row(
            "SELECT id FROM shelf WHERE source_id = ?1 AND book_url = ?2",
            params![book.source_id, book.book_url],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    pub fn shelf_list(&self) -> Result<Vec<ShelfItem>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_id, book_url, name, author, intro, last_read_index, last_read_offset
             FROM shelf ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ShelfItem {
                id: row.get(0)?,
                source_id: row.get(1)?,
                book_url: row.get(2)?,
                name: row.get(3)?,
                author: row.get(4)?,
                intro: row.get(5)?,
                last_read_index: row.get(6)?,
                last_read_offset: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn shelf_remove(&self, id: i64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM shelf WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn shelf_progress(&self, id: i64, index: i64, offset: f64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE shelf SET last_read_index = ?2, last_read_offset = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, index, offset, now_secs()],
        )?;
        Ok(())
    }

    pub fn shelf_progress_by_book(
        &self,
        source_id: i64,
        book_url: &str,
        index: i64,
        offset: f64,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE shelf SET last_read_index = ?3, last_read_offset = ?4, updated_at = ?5
             WHERE source_id = ?1 AND book_url = ?2",
            params![source_id, book_url, index, offset, now_secs()],
        )?;
        Ok(())
    }

    // ---------- 目录缓存 ----------

    pub fn toc_cache_get(&self, source_id: i64, book_url: &str) -> Result<Option<Vec<Chapter>>> {
        let conn = self.conn()?;
        let payload = conn
            .query_row(
                "SELECT payload FROM toc_cache WHERE source_id = ?1 AND book_url = ?2",
                params![source_id, book_url],
                |row| sqlite_text(row, 0),
            )
            .optional()?;
        match payload {
            None => Ok(None),
            Some(raw) => Ok(serde_json::from_str(&raw).ok()),
        }
    }

    pub fn toc_cache_put(&self, source_id: i64, book_url: &str, chapters: &[Chapter]) -> Result<()> {
        let payload = serde_json::to_string(chapters)?;
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO toc_cache (source_id, book_url, payload, cached_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source_id, book_url) DO UPDATE SET
                 payload = excluded.payload, cached_at = excluded.cached_at",
            params![source_id, book_url, payload, now_secs()],
        )?;
        Ok(())
    }

    // ---------- 正文缓存 ----------

    /// 返回 `(章节名, 正文)`。
    pub fn chapter_cache_get(
        &self,
        source_id: i64,
        book_url: &str,
        index: i64,
    ) -> Result<Option<(String, String)>> {
        let conn = self.conn()?;
        let found = conn
            .query_row(
                "SELECT name, content FROM chapter_cache
                 WHERE source_id = ?1 AND book_url = ?2 AND idx = ?3",
                params![source_id, book_url, index],
                |row| Ok((sqlite_text(row, 0)?, sqlite_text(row, 1)?)),
            )
            .optional()?;
        Ok(found)
    }

    pub fn chapter_cache_put(
        &self,
        source_id: i64,
        book_url: &str,
        index: i64,
        name: &str,
        content: &str,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO chapter_cache (source_id, book_url, idx, name, content, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(source_id, book_url, idx) DO UPDATE SET
                 name = excluded.name, content = excluded.content, cached_at = excluded.cached_at",
            params![source_id, book_url, index, name, content, now_secs()],
        )?;
        Ok(())
    }

    pub fn chapter_cache_delete(
        &self,
        source_id: i64,
        book_url: &str,
        index: i64,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM chapter_cache
             WHERE source_id = ?1 AND book_url = ?2 AND idx = ?3",
            params![source_id, book_url, index],
        )?;
        Ok(())
    }

    /// 删除某书全部章节缓存。
    pub fn chapter_cache_clear_book(&self, source_id: i64, book_url: &str) -> Result<u64> {
        let conn = self.conn()?;
        let removed = conn.execute(
            "DELETE FROM chapter_cache WHERE source_id = ?1 AND book_url = ?2",
            params![source_id, book_url],
        )?;
        Ok(removed as u64)
    }

    /// 已缓存正文的章节序号（与目录 `Chapter.index` 一致）。
    pub fn chapter_cache_indices(&self, source_id: i64, book_url: &str) -> Result<Vec<i64>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT idx FROM chapter_cache
             WHERE source_id = ?1 AND book_url = ?2
               AND length(trim(content)) > 0
             ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![source_id, book_url], |row| row.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 按谓词清掉诱饵章。返回删除条数。
    pub fn chapter_cache_purge_where(
        &self,
        source_id: i64,
        book_url: &str,
        is_bad: impl Fn(&str) -> bool,
    ) -> Result<u64> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT idx, content FROM chapter_cache
             WHERE source_id = ?1 AND book_url = ?2",
        )?;
        let rows = stmt.query_map(params![source_id, book_url], |row| {
            Ok((row.get::<_, i64>(0)?, sqlite_text(row, 1)?))
        })?;
        let mut bad_idx = Vec::new();
        for row in rows {
            let (idx, content) = row?;
            if is_bad(&content) {
                bad_idx.push(idx);
            }
        }
        drop(stmt);
        let mut removed = 0u64;
        for idx in bad_idx {
            removed += conn.execute(
                "DELETE FROM chapter_cache
                 WHERE source_id = ?1 AND book_url = ?2 AND idx = ?3",
                params![source_id, book_url, idx],
            )? as u64;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_source() -> BookSource {
        serde_json::from_str(
            r#"{
              "bookSourceName": "示例源",
              "bookSourceUrl": "https://example.com",
              "searchUrl": "/search?q={{key}}",
              "ruleSearch": { "bookList": "css:div.item", "name": "css:a@text" },
              "ruleToc": { "chapterList": "css:a", "chapterName": "text", "chapterUrl": "attr:href" },
              "ruleContent": { "content": "css:div#content@br" }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn source_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_source(&sample_source()).unwrap();
        let briefs = store.list_sources().unwrap();
        assert_eq!(briefs.len(), 1);
        assert!(briefs[0].enabled);
        let loaded = store.get_source(id).unwrap();
        assert_eq!(loaded.book_source_name, "示例源");
        assert_eq!(store.enabled_sources().unwrap().len(), 1);

        store.set_source_enabled(id, false).unwrap();
        assert_eq!(store.enabled_sources().unwrap().len(), 0);
    }

    #[test]
    fn upsert_is_idempotent_by_url() {
        let store = Store::open_in_memory().unwrap();
        let first = store.upsert_source(&sample_source()).unwrap();
        let second = store.upsert_source(&sample_source()).unwrap();
        assert_eq!(first, second);
        assert_eq!(store.list_sources().unwrap().len(), 1);
    }

    /// 编辑保存按 id 覆盖：规则、名称、启停都要一起更新，且不新增行。
    #[test]
    fn update_source_overwrites_in_place() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_source(&sample_source()).unwrap();

        let mut edited = sample_source();
        edited.book_source_name = "改名后的源".into();
        edited.search_url = "/find?kw={{key}}".into();
        edited.enabled = false;
        store.update_source(id, &edited).unwrap();

        let briefs = store.list_sources().unwrap();
        assert_eq!(briefs.len(), 1, "编辑不应产生新书源");
        assert_eq!(briefs[0].id, id);
        assert_eq!(briefs[0].name, "改名后的源");
        assert!(!briefs[0].enabled);

        let loaded = store.get_source(id).unwrap();
        assert_eq!(loaded.search_url, "/find?kw={{key}}");
    }

    /// 编辑时把地址改成与另一书源重复，应如实报错而不是静默覆盖。
    #[test]
    fn update_source_rejects_duplicate_url() {
        let store = Store::open_in_memory().unwrap();
        let first_id = store.upsert_source(&sample_source()).unwrap();

        let mut other = sample_source();
        other.book_source_url = "https://other.example.com".into();
        store.upsert_source(&other).unwrap();

        // 把第一个源改到第二个源已占用的地址，应报 UNIQUE 冲突。
        let mut conflicting = sample_source();
        conflicting.book_source_url = "https://other.example.com".into();
        assert!(store.update_source(first_id, &conflicting).is_err());
    }

    /// 不存在的 id 要报错，避免「保存成功但其实没写进去」。
    #[test]
    fn update_source_unknown_id_is_error() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.update_source(999, &sample_source()).is_err());
    }

    #[test]
    fn shelf_and_cache() {
        let store = Store::open_in_memory().unwrap();
        let book = SearchBook {
            source_id: 1,
            source_name: "示例源".into(),
            name: "斗破苍穹".into(),
            author: "天蚕土豆".into(),
            kind: String::new(),
            intro: "简介".into(),
            cover_url: String::new(),
            book_url: "https://example.com/book/1".into(),
            last_chapter: String::new(),
        };
        let id = store.shelf_add(&book).unwrap();
        store.shelf_progress(id, 12, 0.5).unwrap();
        let items = store.shelf_list().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].last_read_index, 12);

        let chapters = vec![Chapter {
            index: 0,
            name: "第一章".into(),
            url: "https://example.com/1".into(),
        }];
        store.toc_cache_put(1, &book.book_url, &chapters).unwrap();
        assert_eq!(
            store.toc_cache_get(1, &book.book_url).unwrap().unwrap().len(),
            1
        );

        assert!(store
            .chapter_cache_get(1, &book.book_url, 0)
            .unwrap()
            .is_none());
        store
            .chapter_cache_put(1, &book.book_url, 0, "第一章", "正文内容")
            .unwrap();
        let (name, content) = store
            .chapter_cache_get(1, &book.book_url, 0)
            .unwrap()
            .unwrap();
        assert_eq!(name, "第一章");
        assert_eq!(content, "正文内容");
    }

    #[test]
    fn blob_payload_still_loads() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_source(&sample_source()).unwrap();
        {
            let conn = store.conn().unwrap();
            let bytes = serde_json::to_vec(&sample_source()).unwrap();
            conn.execute(
                "UPDATE book_source SET payload = ?1 WHERE id = ?2",
                params![bytes, id],
            )
            .unwrap();
            let kind: String = conn
                .query_row(
                    "SELECT typeof(payload) FROM book_source WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(kind, "blob");
        }
        let loaded = store.get_source(id).unwrap();
        assert_eq!(loaded.book_source_name, "示例源");
        assert_eq!(store.enabled_sources().unwrap().len(), 1);
    }
}

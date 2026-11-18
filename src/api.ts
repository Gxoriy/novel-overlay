import { invoke } from "@tauri-apps/api/core";
import type {
  BookSourceBrief,
  Chapter,
  ChapterContent,
  DataSettings,
  DownloadResult,
  OverlayChapter,
  OverlayState,
  SearchBook,
  ShelfItem,
} from "./types";

export const api = {
  listSources: () => invoke<BookSourceBrief[]>("list_sources"),
  addSource: (json: string) => invoke<BookSourceBrief[]>("add_source", { json }),
  setSourceEnabled: (id: number, enabled: boolean) =>
    invoke<void>("set_source_enabled", { id, enabled }),
  deleteSource: (id: number) => invoke<void>("delete_source", { id }),
  /** 书源 JSON 文本（pretty）：id 为空返回全部数组，否则返回单个对象。 */
  sourceJson: (id: number | null) => invoke<string>("source_json", { id }),
  /** 按 id 保存编辑后的书源（JSON 整体覆盖规则）。 */
  updateSource: (id: number, json: string) => invoke<void>("update_source", { id, json }),

  searchNovel: (query: string, sourceId: number | null) =>
    invoke<SearchBook[]>("search_novel", { query, sourceId }),

  fetchToc: (sourceId: number, bookUrl: string) =>
    invoke<Chapter[]>("fetch_toc", { sourceId, bookUrl }),

  cachedChapterIndices: (sourceId: number, bookUrl: string) =>
    invoke<number[]>("cached_chapter_indices", { sourceId, bookUrl }),

  fetchChapter: (sourceId: number, bookUrl: string, idx: number) =>
    invoke<ChapterContent>("fetch_chapter", { sourceId, bookUrl, idx }),

  collectBook: (book: SearchBook, limit: number | null) =>
    invoke<DownloadResult>("collect_book", { book, limit }),

  prefetchBook: (sourceId: number, bookUrl: string, bookName: string, limit: number | null) =>
    invoke<DownloadResult>("prefetch_book", { sourceId, bookUrl, bookName, limit }),

  shelfList: () => invoke<ShelfItem[]>("shelf_list"),
  shelfAdd: (book: SearchBook) => invoke<number>("shelf_add", { book }),
  shelfRemove: (id: number) => invoke<void>("shelf_remove", { id }),
  shelfProgress: (id: number, idx: number, offset: number) =>
    invoke<void>("shelf_progress", { id, idx, offset }),

  startReading: (sourceId: number, bookUrl: string, bookName: string, idx: number) =>
    invoke<void>("start_reading", { sourceId, bookUrl, bookName, idx }),

  overlayGetChapter: () => invoke<OverlayChapter | null>("overlay_get_chapter"),
  overlayNextChapter: () => invoke<OverlayChapter | null>("overlay_next_chapter"),
  overlayPrevChapter: () => invoke<OverlayChapter | null>("overlay_prev_chapter"),
  overlayGotoChapter: (idx: number) =>
    invoke<OverlayChapter | null>("overlay_goto_chapter", { idx }),

  openOverlay: () => invoke<void>("open_overlay"),
  closeOverlay: () => invoke<void>("close_overlay"),
  overlayState: () => invoke<OverlayState>("overlay_state"),
  overlaySetTextOnly: (enabled: boolean) =>
    invoke<void>("overlay_set_text_only", { enabled }),

  dataSettings: () => invoke<DataSettings>("data_settings"),
  setDataDir: (path: string | null) => invoke<DataSettings>("set_data_dir", { path }),
  overlayShortcut: () => invoke<string | null>("overlay_shortcut"),
};

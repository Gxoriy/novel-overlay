import "./styles/main.css";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import type {
  BookSourceBrief,
  Chapter,
  DataSettings,
  DownloadProgress,
  SearchBook,
  ShelfItem,
} from "./types";

function q<T extends HTMLElement>(sel: string): T {
  const found = document.querySelector(sel);
  if (!found) throw new Error(`missing element: ${sel}`);
  return found as T;
}

const dom = {
  searchSource: q<HTMLSelectElement>("#search-source"),
  searchInput: q<HTMLInputElement>("#search-input"),
  searchGo: q<HTMLButtonElement>("#search-go"),
  searchStatus: q<HTMLDivElement>("#search-status"),
  searchResults: q<HTMLUListElement>("#search-results"),
  shelfList: q<HTMLUListElement>("#shelf-list"),
  shelfStatus: q<HTMLDivElement>("#shelf-status"),
  sourceJson: q<HTMLTextAreaElement>("#source-json"),
  sourceImport: q<HTMLButtonElement>("#source-import"),
  sourceExport: q<HTMLButtonElement>("#source-export"),
  sourceStatus: q<HTMLDivElement>("#source-status"),
  sourceList: q<HTMLUListElement>("#source-list"),
  tocPanel: q<HTMLElement>("#toc-panel"),
  tocTitle: q<HTMLSpanElement>("#toc-title"),
  tocFilter: q<HTMLInputElement>("#toc-filter"),
  tocStatus: q<HTMLDivElement>("#toc-status"),
  tocList: q<HTMLUListElement>("#toc-list"),
  tocClose: q<HTMLButtonElement>("#toc-close"),
  tocDownload: q<HTMLButtonElement>("#toc-download"),
  collectLimit: q<HTMLInputElement>("#collect-limit"),
  tocCollectLimit: q<HTMLInputElement>("#toc-collect-limit"),
  btnOverlay: q<HTMLButtonElement>("#btn-overlay"),
  dataDirInput: q<HTMLInputElement>("#data-dir-input"),
  dataDirSave: q<HTMLButtonElement>("#data-dir-save"),
  dataDirReset: q<HTMLButtonElement>("#data-dir-reset"),
  dataDirStatus: q<HTMLDivElement>("#data-dir-status"),
  shortcutHint: q<HTMLParagraphElement>("#shortcut-hint"),
  toast: q<HTMLDivElement>("#toast"),
};

const COLLECT_LIMIT_KEY = "novel-overlay.collect-limit";

// ---------- 状态 ----------

/**
 * 同一个书名+作者在多个书源上会出现多次。用户想找的是「这本书在哪能读」，
 * 不是「每个源各给我一条」，所以按 `书名|作者` 聚合成一张卡片，卡片内切换来源。
 */
interface BookGroup {
  name: string;
  author: string;
  variants: SearchBook[];
  /** 当前展示的是哪个来源。 */
  active: number;
}

interface TocState {
  sourceId: number;
  bookUrl: string;
  bookName: string;
  chapters: Chapter[];
  /** 正在读第几章（0 基），用于高亮与「继续阅读」。 */
  current: number;
  filter: string;
  /** 本地已有正文的章节序号 */
  cached: Set<number>;
}

let sources: BookSourceBrief[] = [];
let groups: BookGroup[] = [];
let shelfItems: ShelfItem[] = [];
let toc: TocState | null = null;
let toastTimer = 0;
let downloadingKey = "";
const collectProgress = new Map<string, DownloadProgress>();
let defaultedSearchSource = false;

function esc(value: string): string {
  return value.replace(/[&<>"']/g, (ch) => {
    switch (ch) {
      case "&":
        return "&amp;";
      case "<":
        return "&lt;";
      case ">":
        return "&gt;";
      case '"':
        return "&quot;";
      default:
        return "&#39;";
    }
  });
}

function errText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function toast(message: string, kind: "info" | "error" = "info"): void {
  dom.toast.textContent = message;
  dom.toast.classList.remove("hidden");
  dom.toast.classList.toggle("error", kind === "error");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => dom.toast.classList.add("hidden"), 4000);
}

function setStatus(target: HTMLElement, message: string): void {
  target.textContent = message;
}

/** 占位封面上的一个字。没有封面图，也不去抓 —— 见 styles 里的说明。 */
function initial(name: string): string {
  return name.trim().slice(0, 1) || "书";
}

function renderEmpty(target: HTMLElement, title: string, hint: string): void {
  target.innerHTML = `<li class="empty"><div class="empty-title">${esc(title)}</div><div class="empty-hint">${esc(hint)}</div></li>`;
}

/** 加载占位。比一行「搜索中…」更能说明「马上就有东西了」，也不会让布局跳动。 */
function renderSkeleton(target: HTMLElement, rows = 3): void {
  target.innerHTML = Array.from(
    { length: rows },
    () => `
    <li class="card skeleton" aria-hidden="true">
      <div class="cover"></div>
      <div class="card-body">
        <div class="bar w45"></div>
        <div class="bar w70"></div>
        <div class="bar w55"></div>
      </div>
    </li>`,
  ).join("");
}

function activateTab(name: string): void {
  document.querySelectorAll<HTMLButtonElement>(".tab").forEach((tab) => {
    tab.classList.toggle("active", tab.dataset.tab === name);
  });
  document.querySelectorAll<HTMLElement>(".pane").forEach((pane) => {
    pane.classList.toggle("active", pane.id === `pane-${name}`);
  });
}

// ---------- 书架 ----------

function shelfKeys(): Set<string> {
  return new Set(shelfItems.map((item) => `${item.sourceId}|${item.bookUrl}`));
}

async function refreshShelf(): Promise<void> {
  shelfItems = await api.shelfList();
  setStatus(dom.shelfStatus, shelfItems.length ? `共 ${shelfItems.length} 本` : "");
  if (!shelfItems.length) {
    renderEmpty(dom.shelfList, "书架是空的", "在「搜索」页找到书，填章数后点「采集」");
    return;
  }
  dom.shelfList.innerHTML = shelfItems
    .map(
      (item, i) => `
      <li class="card">
        <div class="cover">${esc(initial(item.name))}</div>
        <div class="card-body">
          <div class="card-head">
            <span class="card-title">${esc(item.name)}</span>
            <span class="tag">读到第 ${item.lastReadIndex + 1} 章</span>
          </div>
          <div class="card-foot">
            <div class="card-meta"><span>${esc(item.author || "佚名")}</span></div>
            <div class="card-actions">
              <button class="primary" data-shelf-read="${i}">继续阅读</button>
              <button class="ghost" data-shelf-collect="${i}">${esc(collectActionLabel(true))}</button>
              <button class="ghost" data-shelf-toc="${i}">目录</button>
              <button class="ghost danger" data-shelf-drop="${i}">移除</button>
            </div>
          </div>
        </div>
      </li>`,
    )
    .join("");
}

// ---------- 搜索结果 ----------

function groupHits(hits: SearchBook[]): BookGroup[] {
  const map = new Map<string, BookGroup>();
  for (const hit of hits) {
    const key = `${hit.name.trim().toLowerCase()}|${hit.author.trim().toLowerCase()}`;
    const existing = map.get(key);
    if (existing) {
      if (!existing.variants.some((variant) => variant.bookUrl === hit.bookUrl)) {
        existing.variants.push(hit);
      }
      continue;
    }
    map.set(key, { name: hit.name, author: hit.author, variants: [hit], active: 0 });
  }
  return [...map.values()];
}

/** 组内第一个非空字段 —— 不同书源填的字段完整度不一样。 */
function pick(group: BookGroup, field: "kind" | "intro" | "lastChapter"): string {
  for (const variant of group.variants) {
    const value = variant[field];
    if (value) return value;
  }
  return "";
}

function readCollectLimit(): number | null {
  const raw = (dom.collectLimit.value || dom.tocCollectLimit.value).trim();
  if (!raw) return null;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return null;
  return Math.floor(n);
}

function syncCollectLimitInputs(from: HTMLInputElement, to: HTMLInputElement): void {
  to.value = from.value;
  try {
    localStorage.setItem(COLLECT_LIMIT_KEY, from.value.trim());
  } catch {
    /* ignore quota / private mode */
  }
}

function restoreCollectLimit(): void {
  try {
    const saved = localStorage.getItem(COLLECT_LIMIT_KEY);
    if (saved !== null) {
      dom.collectLimit.value = saved;
      dom.tocCollectLimit.value = saved;
    }
  } catch {
    /* ignore */
  }
}

function collectActionLabel(saved: boolean): string {
  const limit = readCollectLimit();
  if (limit) return saved ? `再采 ${limit} 章` : `采集 ${limit} 章`;
  return saved ? "补采剩余" : "采集全书";
}

function collectLabel(book: SearchBook, saved: boolean): string {
  const key = tocKey(book.sourceId, book.bookUrl);
  const progress = collectProgress.get(key);
  if (downloadingKey === key && progress && !progress.complete) {
    return `采集 ${progress.done}/${progress.total}`;
  }
  if (progress?.complete) {
    return "本批完成";
  }
  return collectActionLabel(saved);
}

function renderSearchRows(): void {
  if (!groups.length) {
    renderEmpty(dom.searchResults, "没有结果", "换个关键词，或到「书源」页确认书源已启用");
    return;
  }
  const onShelf = shelfKeys();
  dom.searchResults.innerHTML = groups
    .map((group, i) => {
      const book = group.variants[group.active];
      const saved = onShelf.has(`${book.sourceId}|${book.bookUrl}`);
      const busy = downloadingKey === tocKey(book.sourceId, book.bookUrl);
      const kind = pick(group, "kind");
      const intro = pick(group, "intro");
      const lastChapter = pick(group, "lastChapter");
      const chips = group.variants
        .map(
          (variant, j) =>
            `<button class="chip${j === group.active ? " on" : ""}" data-pick="${i}:${j}">${esc(
              variant.sourceName || "未命名书源",
            )}</button>`,
        )
        .join("");
      return `
      <li class="card">
        <div class="cover">${esc(initial(group.name))}</div>
        <div class="card-body">
          <div class="card-head">
            <span class="card-title">${esc(group.name)}</span>
            ${saved ? `<span class="badge">已在书架</span>` : ""}
          </div>
          <div class="card-meta">
            <span>${esc(group.author || "佚名")}</span>
            ${kind ? `<span class="tag">${esc(kind)}</span>` : ""}
            ${lastChapter ? `<span class="tag">最新：${esc(lastChapter)}</span>` : ""}
          </div>
          ${intro ? `<p class="card-intro">${esc(intro)}</p>` : ""}
          <div class="card-foot">
            <div class="chips"><span class="chips-label">来源</span>${chips}</div>
            <div class="card-actions">
              <button class="primary" data-hit-collect="${i}" ${busy ? "disabled" : ""}>${esc(
                collectLabel(book, saved),
              )}</button>
              <button class="ghost" data-hit-toc="${i}">目录</button>
            </div>
          </div>
        </div>
      </li>`;
    })
    .join("");
}

async function doSearch(): Promise<void> {
  const query = dom.searchInput.value.trim();
  if (!query) return;
  if (!sources.some((item) => item.enabled)) {
    toast("还没有启用中的书源，先到「书源」页导入", "error");
    activateTab("sources");
    return;
  }
  const raw = dom.searchSource.value;
  const sourceId = raw ? Number(raw) : null;

  dom.searchGo.disabled = true;
  setStatus(dom.searchStatus, "搜索中…");
  renderSkeleton(dom.searchResults);
  try {
    const hits = await api.searchNovel(query, sourceId);
    groups = groupHits(hits);
    renderSearchRows();
    setStatus(
      dom.searchStatus,
      groups.length
        ? `共 ${hits.length} 条结果，${groups.length} 本书`
        : "没有结果。换个关键词，或检查书源规则。",
    );
  } catch (error) {
    groups = [];
    renderEmpty(dom.searchResults, "搜索失败", "错误详情见下方提示");
    setStatus(dom.searchStatus, "");
    toast(errText(error), "error");
  } finally {
    dom.searchGo.disabled = false;
  }
}

// ---------- 目录 ----------

function tocKey(sourceId: number, bookUrl: string): string {
  return `${sourceId}|${bookUrl}`;
}

function resetDownloadButton(): void {
  const current = toc ? tocKey(toc.sourceId, toc.bookUrl) : "";
  const busy = Boolean(current && downloadingKey === current);
  dom.tocDownload.disabled = busy || !toc;
  if (busy) {
    dom.tocDownload.textContent = "采集中…";
  } else {
    const limit = readCollectLimit();
    dom.tocDownload.textContent = limit ? `采集 ${limit} 章` : "采集全书";
  }
}

function applyDownloadProgress(progress: DownloadProgress): void {
  const key = tocKey(progress.sourceId, progress.bookUrl);
  collectProgress.set(key, progress);
  if (groups.length) renderSearchRows();
  if (progress.complete) {
    setStatus(dom.searchStatus, `${progress.bookName}：已采集 ${progress.total} 章`);
  } else {
    setStatus(
      dom.searchStatus,
      `采集 ${progress.bookName} ${progress.done}/${progress.total}：${progress.chapterName}`,
    );
  }
  if (!toc || toc.sourceId !== progress.sourceId || toc.bookUrl !== progress.bookUrl) {
    return;
  }
  if (progress.complete) {
    setStatus(
      dom.tocStatus,
      progress.total
        ? `共 ${progress.total} 章，已采集到本地`
        : `共 ${toc.chapters.length} 章`,
    );
    resetDownloadButton();
    return;
  }
  setStatus(
    dom.tocStatus,
    `采集 ${progress.done}/${progress.total}：${progress.chapterName}`,
  );
  dom.tocDownload.disabled = true;
  dom.tocDownload.textContent = `${progress.done}/${progress.total}`;
}

async function collectBook(book: SearchBook): Promise<void> {
  const key = tocKey(book.sourceId, book.bookUrl);
  if (downloadingKey === key) return;
  const limit = readCollectLimit();
  downloadingKey = key;
  renderSearchRows();
  resetDownloadButton();
  try {
    const result = await api.collectBook(book, limit);
    const got = result.downloaded;
    const suffix = result.failed ? `，${result.failed} 章失败` : "";
    const scope = limit ? `本批 ${limit} 章上限` : "全书未缓存";
    setStatus(
      dom.searchStatus,
      `${book.name}：新采 ${got} 章（${scope}）${suffix}`,
    );
    toast(
      result.failed
        ? `${book.name}：新采 ${got}/${result.total}，失败 ${result.failed}`
        : got === 0
          ? `${book.name}：没有需要新采的章节`
          : `${book.name}：新采 ${got} 章`,
      result.failed ? "error" : "info",
    );
    await refreshShelf();
    // 采集后刷新目录上的「已存/未采」标记
    if (toc && toc.sourceId === book.sourceId && toc.bookUrl === book.bookUrl) {
      const cachedIdx = await api.cachedChapterIndices(book.sourceId, book.bookUrl);
      toc.cached = new Set(cachedIdx);
      renderTocRows();
    }
  } catch (error) {
    toast(errText(error), "error");
  } finally {
    if (downloadingKey === key) downloadingKey = "";
    renderSearchRows();
    resetDownloadButton();
  }
}

async function downloadAll(): Promise<void> {
  if (!toc) return;
  await collectBook({
    sourceId: toc.sourceId,
    sourceName: "",
    name: toc.bookName,
    author: "",
    kind: "",
    intro: "",
    coverUrl: "",
    bookUrl: toc.bookUrl,
    lastChapter: "",
  });
}

function closeToc(): void {
  dom.tocPanel.classList.add("hidden");
}

function renderTocRows(): void {
  if (!toc) return;
  const keyword = toc.filter.trim().toLowerCase();
  const matched = toc.chapters
    .map((chapter, index) => ({ chapter, index }))
    .filter(({ chapter }) => !keyword || chapter.name.toLowerCase().includes(keyword));

  const cachedCount = toc.cached.size;
  setStatus(
    dom.tocStatus,
    keyword
      ? `${matched.length} / ${toc.chapters.length} 章 · 已缓存正文 ${cachedCount}`
      : `目录 ${toc.chapters.length} 章 · 已缓存正文 ${cachedCount} 章`,
  );
  if (!matched.length) {
    renderEmpty(dom.tocList, "没有匹配的章节", "换个关键词试试");
    return;
  }
  dom.tocList.innerHTML = matched
    .map(({ chapter, index }) => {
      const has = toc!.cached.has(index);
      return `
      <li class="chapter${index === toc!.current ? " on" : ""}${has ? " cached" : " missing"}" data-read="${index}">
        <span class="chapter-no">${index + 1}</span>
        <span class="chapter-name">${esc(chapter.name)}</span>
        <span class="chapter-flag">${has ? "已存" : "未采"}</span>
      </li>`;
    })
    .join("");

  const active = dom.tocList.querySelector<HTMLElement>(".chapter.on");
  active?.scrollIntoView({ block: "center" });
}

async function openToc(sourceId: number, bookUrl: string, bookName: string): Promise<void> {
  const saved = shelfItems.find((item) => item.sourceId === sourceId && item.bookUrl === bookUrl);
  dom.tocPanel.classList.remove("hidden");
  dom.tocTitle.textContent = bookName;
  dom.tocFilter.value = "";
  dom.tocDownload.disabled = true;
  resetDownloadButton();
  renderSkeleton(dom.tocList, 6);
  setStatus(dom.tocStatus, "拉取目录中…");
  try {
    const [chapters, cachedIdx] = await Promise.all([
      api.fetchToc(sourceId, bookUrl),
      api.cachedChapterIndices(sourceId, bookUrl),
    ]);
    toc = {
      sourceId,
      bookUrl,
      bookName,
      chapters,
      current: saved ? saved.lastReadIndex : 0,
      filter: "",
      cached: new Set(cachedIdx),
    };
    resetDownloadButton();
    renderTocRows();
    if (cachedIdx.length < chapters.length) {
      toast(
        `目录 ${chapters.length} 章，本地正文仅 ${cachedIdx.length} 章。未采章节需点「采集」补下`,
        "error",
      );
    }
  } catch (error) {
    toc = null;
    resetDownloadButton();
    renderEmpty(dom.tocList, "目录拉取失败", "错误详情见下方提示");
    setStatus(dom.tocStatus, "");
    toast(errText(error), "error");
  }
}

async function startReading(index: number): Promise<void> {
  if (!toc) return;
  const chapter = toc.chapters[index];
  if (!chapter) return;
  try {
    await api.startReading(toc.sourceId, toc.bookUrl, toc.bookName, index);
    toc.current = index;
    renderTocRows();
    // 进度落库了，书架那一页的「读到第 N 章」得跟上。
    await refreshShelf();
    toast(`开始阅读：${chapter.name}`);
  } catch (error) {
    toast(errText(error), "error");
  }
}

// ---------- 浮窗状态 ----------

function applyOverlayState(visible: boolean): void {
  dom.btnOverlay.dataset.state = visible ? "open" : "closed";
  const label = dom.btnOverlay.querySelector(".label");
  if (label) {
    label.textContent = visible ? "浮窗 · 已打开" : "阅读浮窗";
  }
}

/** 一律回查真实状态，不靠事件猜 —— `close_overlay` 不发事件。 */
async function refreshOverlayState(): Promise<void> {
  try {
    const state = await api.overlayState();
    applyOverlayState(state.visible);
  } catch {
    // 浮窗状态查不到不影响主窗口使用。
  }
}

// ---------- 书源 ----------

async function refreshSources(): Promise<void> {
  sources = await api.listSources();
  const usable = sources.filter(
    (item) => item.enabled && !item.url.includes("fixture.test"),
  );
  const previous = dom.searchSource.value;
  dom.searchSource.innerHTML =
    `<option value="">全部书源</option>` +
    usable.map((item) => `<option value="${item.id}">${esc(item.name)}</option>`).join("");
  const bqg = usable.find((item) => item.url.includes("bqg616.cc"));
  if (!defaultedSearchSource && bqg) {
    dom.searchSource.value = String(bqg.id);
    defaultedSearchSource = true;
  } else if (usable.some((item) => String(item.id) === previous)) {
    dom.searchSource.value = previous;
  }

  if (!sources.length) {
    renderEmpty(dom.sourceList, "还没有书源", "展开上方「导入书源」，粘贴书源 JSON");
    return;
  }
  dom.sourceList.innerHTML = sources
    .map(
      (item) => `
      <li class="card">
        <div class="card-body">
          <div class="card-head">
            <span class="card-title">${esc(item.name)}</span>
            <span class="badge${item.enabled ? "" : " muted"}">${
              item.enabled ? "启用中" : "已停用"
            }</span>
          </div>
          <div class="card-foot">
            <div class="card-meta"><span>${esc(item.url)}</span></div>
            <div class="card-actions">
              <button class="ghost" data-toggle="${item.id}">${item.enabled ? "停用" : "启用"}</button>
              <button class="ghost" data-export="${item.id}">导出</button>
              <button class="ghost danger" data-drop="${item.id}">删除</button>
            </div>
          </div>
        </div>
      </li>`,
    )
    .join("");
}

// ---------- 设置 ----------

function renderDataSettings(settings: DataSettings): void {
  dom.dataDirInput.value = settings.customDir ?? "";
  dom.dataDirInput.placeholder = settings.defaultDir;
  const parts = [`当前使用：${settings.activeDir}`];
  if (settings.restartRequired) {
    parts.push("已保存新目录，重启应用后生效");
  }
  setStatus(dom.dataDirStatus, parts.join(" · "));
}

/** 显示本次启动实际绑定到的浮窗快捷键（候选组合可能被其他软件占用）。 */
async function refreshShortcutHint(): Promise<void> {
  try {
    const shortcut = await api.overlayShortcut();
    dom.shortcutHint.textContent = shortcut
      ? `阅读浮窗快捷键：${shortcut}（任意界面可呼出/收起浮窗）`
      : "全局快捷键已被系统或其他软件全部占用，请用主窗口右上角按钮打开浮窗";
  } catch {
    dom.shortcutHint.textContent = "";
  }
}

async function refreshDataSettings(): Promise<void> {
  try {
    renderDataSettings(await api.dataSettings());
  } catch (error) {
    setStatus(dom.dataDirStatus, "");
    toast(errText(error), "error");
  }
}

async function saveDataDir(path: string | null): Promise<void> {
  dom.dataDirSave.disabled = true;
  dom.dataDirReset.disabled = true;
  try {
    const settings = await api.setDataDir(path);
    renderDataSettings(settings);
    toast(
      settings.restartRequired
        ? "已保存新的数据目录，重启应用后生效"
        : "数据目录设置已保存",
    );
  } catch (error) {
    toast(errText(error), "error");
  } finally {
    dom.dataDirSave.disabled = false;
    dom.dataDirReset.disabled = false;
  }
}

// ---------- 事件绑定 ----------

function bindTabs(): void {
  document.querySelectorAll<HTMLButtonElement>(".tab").forEach((tab) => {
    tab.addEventListener("click", () => activateTab(tab.dataset.tab ?? "search"));
  });
}

function bindKeyboard(): void {
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      if (!dom.tocPanel.classList.contains("hidden")) {
        closeToc();
        return;
      }
      if (dom.searchInput.value) {
        dom.searchInput.value = "";
        dom.searchInput.focus();
      }
      return;
    }
    if ((event.metaKey || event.ctrlKey) && (event.key === "f" || event.key === "k")) {
      event.preventDefault();
      activateTab("search");
      dom.searchInput.focus();
      dom.searchInput.select();
    }
  });
}

function bindEvents(): void {
  dom.searchGo.addEventListener("click", () => void doSearch());
  dom.searchInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") void doSearch();
  });

  dom.btnOverlay.addEventListener("click", () => {
    void (async () => {
      try {
        const state = await api.overlayState();
        if (state.visible) await api.closeOverlay();
        else await api.openOverlay();
        await refreshOverlayState();
      } catch (error) {
        toast(errText(error), "error");
      }
    })();
  });

  dom.dataDirSave.addEventListener("click", () => {
    const raw = dom.dataDirInput.value.trim();
    void saveDataDir(raw ? raw : null);
  });
  dom.dataDirInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") dom.dataDirSave.click();
  });
  dom.dataDirReset.addEventListener("click", () => {
    dom.dataDirInput.value = "";
    void saveDataDir(null);
  });

  dom.tocClose.addEventListener("click", closeToc);
  dom.tocDownload.addEventListener("click", () => void downloadAll());
  dom.tocFilter.addEventListener("input", () => {
    if (!toc) return;
    toc.filter = dom.tocFilter.value;
    renderTocRows();
  });

  const onLimitChange = (from: HTMLInputElement, to: HTMLInputElement) => {
    syncCollectLimitInputs(from, to);
    resetDownloadButton();
    if (groups.length) renderSearchRows();
    if (shelfItems.length) void refreshShelf();
  };
  dom.collectLimit.addEventListener("input", () =>
    onLimitChange(dom.collectLimit, dom.tocCollectLimit),
  );
  dom.tocCollectLimit.addEventListener("input", () =>
    onLimitChange(dom.tocCollectLimit, dom.collectLimit),
  );

  /** 导出书源 JSON：弹出系统保存对话框；用户取消时静默返回。 */
  async function runExport(id: number | null): Promise<void> {
    const path = await api.exportSources(id);
    if (!path) {
      toast("已取消导出");
      return;
    }
    setStatus(dom.sourceStatus, `已导出：${path}`);
    toast(`已导出到 ${path}`);
  }

  dom.sourceImport.addEventListener("click", () => {
    const raw = dom.sourceJson.value.trim();
    if (!raw) {
      toast("先粘贴书源 JSON", "error");
      return;
    }
    void (async () => {
      try {
        const added = await api.addSource(raw);
        setStatus(dom.sourceStatus, `已导入 ${added.length} 个书源`);
        dom.sourceJson.value = "";
        await refreshSources();
        toast(`导入成功：${added.map((item) => item.name).join("、")}`);
      } catch (error) {
        setStatus(dom.sourceStatus, "");
        toast(errText(error), "error");
      }
    })();
  });

  dom.sourceList.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    const toggleId = target.dataset.toggle;
    const dropId = target.dataset.drop;
    const exportId = target.dataset.export;
    void (async () => {
      try {
        if (toggleId) {
          const source = sources.find((item) => item.id === Number(toggleId));
          if (source) await api.setSourceEnabled(source.id, !source.enabled);
          await refreshSources();
        } else if (dropId) {
          await api.deleteSource(Number(dropId));
          await refreshSources();
          toast("书源已删除");
        } else if (exportId) {
          await runExport(Number(exportId));
        }
      } catch (error) {
        toast(errText(error), "error");
      }
    })();
  });

  dom.sourceExport.addEventListener("click", () => {
    void runExport(null).catch((error) => toast(errText(error), "error"));
  });

  dom.searchResults.addEventListener("click", (event) => {
    const target = (event.target as HTMLElement).closest<HTMLElement>(
      "[data-pick],[data-hit-collect],[data-hit-toc]",
    );
    if (!target) return;
    void (async () => {
      try {
        const pickAt = target.dataset.pick;
        if (pickAt) {
          const [groupAt, variantAt] = pickAt.split(":").map(Number);
          const group = groups[groupAt];
          if (group) {
            group.active = variantAt;
            renderSearchRows();
          }
          return;
        }
        const collectAt = target.dataset.hitCollect;
        if (collectAt !== undefined) {
          const group = groups[Number(collectAt)];
          if (!group) return;
          await collectBook(group.variants[group.active]);
          return;
        }
        const tocAt = target.dataset.hitToc;
        if (tocAt !== undefined) {
          const group = groups[Number(tocAt)];
          if (!group) return;
          const book = group.variants[group.active];
          await openToc(book.sourceId, book.bookUrl, book.name);
        }
      } catch (error) {
        toast(errText(error), "error");
      }
    })();
  });

  dom.shelfList.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    void (async () => {
      try {
        if (target.dataset.shelfRead) {
          const item = shelfItems[Number(target.dataset.shelfRead)];
          if (!item) return;
          await openToc(item.sourceId, item.bookUrl, item.name);
          await startReading(item.lastReadIndex);
        } else if (target.dataset.shelfCollect) {
          const item = shelfItems[Number(target.dataset.shelfCollect)];
          if (!item) return;
          await collectBook({
            sourceId: item.sourceId,
            sourceName: "",
            name: item.name,
            author: item.author,
            kind: "",
            intro: item.intro,
            coverUrl: "",
            bookUrl: item.bookUrl,
            lastChapter: "",
          });
        } else if (target.dataset.shelfToc) {
          const item = shelfItems[Number(target.dataset.shelfToc)];
          if (item) await openToc(item.sourceId, item.bookUrl, item.name);
        } else if (target.dataset.shelfDrop) {
          const item = shelfItems[Number(target.dataset.shelfDrop)];
          if (!item) return;
          await api.shelfRemove(item.id);
          await refreshShelf();
          renderSearchRows();
        }
      } catch (error) {
        toast(errText(error), "error");
      }
    })();
  });

  dom.tocList.addEventListener("click", (event) => {
    const target = (event.target as HTMLElement).closest<HTMLElement>("[data-read]");
    const readAt = target?.dataset.read;
    if (readAt) void startReading(Number(readAt));
  });
}

function bindOverlayEvents(): void {
  // 后端在浮窗被显示时广播 `overlay://session`（广播到所有 webview，主窗口也收得到）。
  void listen("overlay://session", () => void refreshOverlayState());
  void listen<DownloadProgress>("download://progress", (event) => {
    applyDownloadProgress(event.payload);
  });
}

/** 标在 <html> 上给 CSS 用来区分平台（macOS 交通灯留白等）。 */
function markPlatform(): void {
  const ua = navigator.userAgent;
  const os = /Windows/i.test(ua) ? "windows" : /Mac OS X/i.test(ua) ? "macos" : "linux";
  document.documentElement.dataset.os = os;
}

async function boot(): Promise<void> {
  markPlatform();
  restoreCollectLimit();
  bindTabs();
  bindKeyboard();
  bindEvents();
  bindOverlayEvents();
  try {
    await refreshSources();
    await refreshShelf();
    await refreshOverlayState();
    await refreshDataSettings();
    await refreshShortcutHint();
  } catch (error) {
    // 后端 setup 失败时所有命令都会报错，列表区留白会让人以为是「没有书源」。
    renderEmpty(
      dom.sourceList,
      "书源加载失败",
      "后端未就绪：数据库被占用或启动出错，重启应用；仍失败可查看数据目录下的 startup-error.log",
    );
    toast(errText(error), "error");
  }
}

void boot();

import "./styles/overlay.css";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import { planScroll } from "./scroll";
import type { OverlayChapter } from "./types";

function q<T extends HTMLElement>(sel: string): T {
  const found = document.querySelector(sel);
  if (!found) throw new Error(`missing element: ${sel}`);
  return found as T;
}

const dom = {
  frame: q<HTMLDivElement>("#frame"),
  chapter: q<HTMLSpanElement>("#chapter"),
  viewport: q<HTMLDivElement>("#viewport"),
  text: q<HTMLDivElement>("#text"),
  pct: q<HTMLSpanElement>("#pct"),
  hint: q<HTMLSpanElement>("#hint"),
  prev: q<HTMLButtonElement>("#prev"),
  play: q<HTMLButtonElement>("#play"),
  next: q<HTMLButtonElement>("#next"),
  speed: q<HTMLSpanElement>("#speed"),
  speedDec: q<HTMLButtonElement>("#speed-dec"),
  speedInc: q<HTMLButtonElement>("#speed-inc"),
  fontDec: q<HTMLButtonElement>("#font-dec"),
  fontInc: q<HTMLButtonElement>("#font-inc"),
  alphaDec: q<HTMLButtonElement>("#alpha-dec"),
  alphaInc: q<HTMLButtonElement>("#alpha-inc"),
  close: q<HTMLButtonElement>("#close"),
};

// v4：默认纯透明（无毛玻璃）；点正文只切换 CSS 半透明底板。
const STORE_KEY = "novel-overlay.prefs.v4";

interface Prefs {
  fontSize: number;
  lineHeight: number;
  panelAlpha: number;
  /** true = 纯透明只留字；false = CSS 半透明底板（不用系统毛玻璃） */
  textOnly: boolean;
  /** 退出纯文字时恢复的底板不透明度 */
  panelAlphaRestore: number;
  speed: number;
}

const DEFAULT_PREFS: Prefs = {
  fontSize: 15,
  lineHeight: 1.9,
  panelAlpha: 0,
  textOnly: true,
  panelAlphaRestore: 0.55,
  // px / 秒。浮窗常失焦，用 setInterval 驱动，别太慢。
  speed: 48,
};

/** 自动滚动速度的可调范围与每档倍率：8px/s 慢读，320px/s 速览。 */
const SPEED_MIN = 8;
const SPEED_MAX = 320;
const SPEED_FACTOR = 1.25;

function loadPrefs(): Prefs {
  const raw = localStorage.getItem(STORE_KEY);
  if (!raw) return { ...DEFAULT_PREFS };
  try {
    return { ...DEFAULT_PREFS, ...(JSON.parse(raw) as Partial<Prefs>) };
  } catch {
    return { ...DEFAULT_PREFS };
  }
}

const prefs = loadPrefs();
prefs.speed = clampSpeed(prefs.speed);
let playing = true;
let loading = false;
let currentIndex = -1;
/** 滚到文末留白底部后的停留累计，毫秒 */
let endDwellMs = 0;
/** 文末真正到底后再停一会，避免最后一句还在视野里就翻章 */
const END_DWELL_MS = 2800;
const TICK_MS = 33;
/** 上一帧没落地的亚像素余量（<1px），低速时靠它攒够整像素再滚 */
let scrollRemainder = 0;
/** 上一帧的时间戳，用来按真实帧间隔算位移（定时器抖动/被节流时不出偏差） */
let lastTickAt = 0;

/** 手动改过 scrollTop 之后调用：丢掉旧余量，从新位置重新起步。 */
function resetScrollFlow(): void {
  scrollRemainder = 0;
}

/** 文末垫高，让最后一段能滚到浮窗中部再继续往下「空滚」。 */
function applyEndPadding(): void {
  const pad = Math.max(96, Math.floor(dom.viewport.clientHeight * 0.55));
  dom.text.style.paddingBottom = `${pad}px`;
}

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

function savePrefs(): void {
  localStorage.setItem(STORE_KEY, JSON.stringify(prefs));
}

function applyPrefs(): void {
  const root = document.documentElement;
  root.style.setProperty("--reader-font", `${prefs.fontSize}px`);
  root.style.setProperty("--reader-lh", String(prefs.lineHeight));
  const alpha = prefs.textOnly ? 0 : Math.max(prefs.panelAlpha, 0.35);
  dom.frame.style.setProperty("--panel-alpha", alpha.toFixed(3));
  dom.frame.classList.toggle("text-only", prefs.textOnly);
  // 再清一次原生毛玻璃（须走主线程命令）；底板只靠上面的 CSS。
  void api.overlaySetTextOnly(prefs.textOnly).catch(() => {
    /* 非 macOS / 旧二进制忽略 */
  });
}

async function setTextOnly(enabled: boolean): Promise<void> {
  if (enabled === prefs.textOnly) return;
  if (enabled) {
    if (prefs.panelAlpha >= 0.05) {
      prefs.panelAlphaRestore = prefs.panelAlpha;
    }
    prefs.textOnly = true;
  } else {
    prefs.textOnly = false;
    prefs.panelAlpha = Math.max(0.35, prefs.panelAlphaRestore || DEFAULT_PREFS.panelAlphaRestore);
  }
  applyPrefs();
  savePrefs();
  syncPlayButton();
}

function updateProgress(): void {
  const max = dom.viewport.scrollHeight - dom.viewport.clientHeight;
  const ratio = max <= 0 ? 1 : Math.min(1, dom.viewport.scrollTop / max);
  dom.pct.textContent = `${Math.round(ratio * 100)}%`;
}

function render(chapter: OverlayChapter | null): void {
  endDwellMs = 0;
  resetScrollFlow();
  if (!chapter) {
    dom.chapter.textContent = "未开始阅读";
    dom.text.innerHTML = `<p class="muted">在主窗口的书架或目录里点「阅读」开始。点正文可切换纯透明。</p>`;
    applyEndPadding();
    dom.viewport.scrollTop = 0;
    updateProgress();
    return;
  }
  currentIndex = chapter.index;
  dom.chapter.textContent = `${chapter.bookName} · ${chapter.name} · ${chapter.index + 1}/${chapter.total}`;
  const paragraphs = chapter.content
    .split(/\n+/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  dom.text.innerHTML = paragraphs.map((line) => `<p>${esc(line)}</p>`).join("");
  applyEndPadding();
  dom.viewport.scrollTop = 0;
  updateProgress();
}

function renderFetchError(message: string): void {
  playing = false;
  syncPlayButton();
  dom.chapter.textContent = "本章正文未就绪";
  dom.text.innerHTML = `<p class="muted">${esc(message)}</p><p class="muted">目录有标题不等于已下载正文。请回主窗口目录点「采集」补下后续章节。</p>`;
  applyEndPadding();
  updateProgress();
}

async function refresh(): Promise<void> {
  try {
    render(await api.overlayGetChapter());
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    renderFetchError(message || "拉取正文失败");
  }
}

async function step(direction: 1 | -1): Promise<void> {
  if (loading) return;
  loading = true;
  endDwellMs = 0;
  try {
    const chapter =
      direction === 1 ? await api.overlayNextChapter() : await api.overlayPrevChapter();
    if (chapter && chapter.index === currentIndex) {
      dom.viewport.scrollTop = dom.viewport.scrollHeight;
      resetScrollFlow();
    } else {
      render(chapter);
    }
  } catch (error) {
    playing = false;
    syncPlayButton();
    const message = error instanceof Error ? error.message : String(error);
    // 下一章没正文时停住并说明，避免看起来像「空白章」
    if (direction === 1) {
      renderFetchError(message || "下一章正文未下载或书源暂时不可用");
    }
  } finally {
    loading = false;
  }
}

function clampSpeed(value: number): number {
  if (!Number.isFinite(value)) return DEFAULT_PREFS.speed;
  return Math.min(SPEED_MAX, Math.max(SPEED_MIN, Math.round(value)));
}

/** 同步工具栏上的速度数值（- / + 之间那个数字）。 */
function renderSpeed(): void {
  dom.speed.textContent = String(prefs.speed);
  dom.speed.title = `自动滚动 ${prefs.speed} 像素/秒，点击恢复默认`;
}

function setSpeed(next: number): void {
  prefs.speed = clampSpeed(next);
  renderSpeed();
  savePrefs();
  syncPlayButton();
}

/** 按倍率加减一档；到边界时数值不变。 */
function stepSpeed(direction: 1 | -1): void {
  setSpeed(prefs.speed * (direction === 1 ? SPEED_FACTOR : 1 / SPEED_FACTOR));
}

function syncPlayButton(): void {
  dom.play.textContent = playing ? "‖" : "▶";
  const mode = prefs.textOnly ? "纯文字" : "有底板";
  dom.hint.textContent = playing
    ? `自动滚动 ${prefs.speed}px/s · 空格/小键盘回车·0 暂停 · 点正文=${mode}`
    : `已暂停 · 空格/小键盘回车·0 继续 · 点正文=${mode}`;
}

function tick(): void {
  const now = performance.now();
  // 定时器会被节流：按真实间隔算位移，但单帧上限 250ms，避免回到前台时猛跳一大段
  const elapsed = lastTickAt === 0 ? TICK_MS : Math.min(250, now - lastTickAt);
  lastTickAt = now;

  if (playing && !loading) {
    const viewport = dom.viewport;
    const maxScroll = viewport.scrollHeight - viewport.clientHeight;
    if (maxScroll <= 1) {
      // 一屏装得下（含文末留白后仍装得下）：停一会再翻章
      scrollRemainder = 0;
      if (currentIndex >= 0) {
        endDwellMs += elapsed;
        if (endDwellMs >= END_DWELL_MS) {
          endDwellMs = 0;
          void step(1);
        }
      }
    } else {
      // 低速时每帧不足 1px：整像素落地，余量留给下一帧（见 scroll.ts 的说明）
      const plan = planScroll(
        viewport.scrollTop,
        scrollRemainder,
        (prefs.speed * elapsed) / 1000,
        maxScroll,
      );
      scrollRemainder = plan.remainder;
      if (plan.scrollTop !== viewport.scrollTop) {
        viewport.scrollTop = plan.scrollTop;
      }
      const atEnd =
        viewport.scrollTop + viewport.clientHeight >= viewport.scrollHeight - 2;
      if (atEnd) {
        // 最后一段已滚到中部并空滚完留白：再停约 2.8s 才翻章
        endDwellMs += elapsed;
        if (endDwellMs >= END_DWELL_MS) {
          endDwellMs = 0;
          void step(1);
        }
      } else {
        endDwellMs = 0;
      }
    }
  }
  updateProgress();
}

function bindEvents(): void {
  dom.play.addEventListener("click", (event) => {
    event.stopPropagation();
    playing = !playing;
    syncPlayButton();
  });

  dom.next.addEventListener("click", (event) => {
    event.stopPropagation();
    void step(1);
  });
  dom.prev.addEventListener("click", (event) => {
    event.stopPropagation();
    void step(-1);
  });

  dom.speedDec.addEventListener("click", (event) => {
    event.stopPropagation();
    stepSpeed(-1);
  });
  dom.speedInc.addEventListener("click", (event) => {
    event.stopPropagation();
    stepSpeed(1);
  });
  // 数字本身可点：一键回到默认速度，省得一下下点回去。
  dom.speed.addEventListener("click", (event) => {
    event.stopPropagation();
    setSpeed(DEFAULT_PREFS.speed);
  });
  dom.close.addEventListener("click", (event) => {
    event.stopPropagation();
    void api.closeOverlay();
  });

  dom.fontInc.addEventListener("click", (event) => {
    event.stopPropagation();
    prefs.fontSize = Math.min(30, prefs.fontSize + 1);
    applyPrefs();
    savePrefs();
  });
  dom.fontDec.addEventListener("click", (event) => {
    event.stopPropagation();
    prefs.fontSize = Math.max(11, prefs.fontSize - 1);
    applyPrefs();
    savePrefs();
  });

  dom.alphaInc.addEventListener("click", (event) => {
    event.stopPropagation();
    if (prefs.textOnly) {
      void setTextOnly(false);
      return;
    }
    prefs.panelAlpha = Math.min(1, Number((prefs.panelAlpha + 0.06).toFixed(3)));
    prefs.panelAlphaRestore = prefs.panelAlpha;
    applyPrefs();
    savePrefs();
  });
  dom.alphaDec.addEventListener("click", (event) => {
    event.stopPropagation();
    if (prefs.textOnly) return;
    prefs.panelAlpha = Math.max(0, Number((prefs.panelAlpha - 0.06).toFixed(3)));
    if (prefs.panelAlpha < 0.05) {
      void setTextOnly(true);
      return;
    }
    prefs.panelAlphaRestore = prefs.panelAlpha;
    applyPrefs();
    savePrefs();
  });

  // 点正文区域：切换纯透明（只显示文字）
  dom.viewport.addEventListener("click", (event) => {
    if ((event.target as HTMLElement).closest("button")) return;
    void setTextOnly(!prefs.textOnly);
  });

  window.addEventListener("keydown", (event) => {
    // 空格 / 小键盘回车 / 小键盘 0：暂停或继续自动滚动
    if (
      event.code === "Space" ||
      event.code === "NumpadEnter" ||
      event.code === "Numpad0"
    ) {
      event.preventDefault();
      playing = !playing;
      syncPlayButton();
    } else if (event.code === "ArrowDown") {
      dom.viewport.scrollTop += 48;
      resetScrollFlow();
    } else if (event.code === "ArrowUp") {
      dom.viewport.scrollTop -= 48;
      resetScrollFlow();
    } else if (event.code === "ArrowRight") {
      void step(1);
    } else if (event.code === "ArrowLeft") {
      void step(-1);
    } else if (event.code === "KeyT") {
      void setTextOnly(!prefs.textOnly);
    } else if (event.code === "Minus" || event.code === "NumpadSubtract") {
      stepSpeed(-1);
    } else if (event.code === "Equal" || event.code === "NumpadAdd") {
      stepSpeed(1);
    }
  });

  dom.viewport.addEventListener(
    "wheel",
    (event) => {
      dom.viewport.scrollTop += event.deltaY;
      resetScrollFlow();
      event.preventDefault();
    },
    { passive: false },
  );
}

async function boot(): Promise<void> {
  applyPrefs();
  renderSpeed();
  syncPlayButton();
  bindEvents();
  window.addEventListener("resize", () => {
    applyEndPadding();
    resetScrollFlow();
    updateProgress();
  });
  await refresh();

  void listen("overlay://session", () => {
    playing = true;
    syncPlayButton();
    void refresh();
  });

  // 浮窗经常不是 key window，rAF 会被大幅节流；用 interval 保自动滚动。
  window.setInterval(tick, TICK_MS);
}

void boot();

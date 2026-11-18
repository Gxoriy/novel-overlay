export interface BookSourceBrief {
  id: number;
  name: string;
  url: string;
  enabled: boolean;
}

export interface SearchBook {
  sourceId: number;
  sourceName: string;
  name: string;
  author: string;
  kind: string;
  intro: string;
  coverUrl: string;
  bookUrl: string;
  lastChapter: string;
}

export interface Chapter {
  index: number;
  name: string;
  url: string;
}

export interface ChapterContent {
  index: number;
  name: string;
  content: string;
  fromCache: boolean;
}

export interface ShelfItem {
  id: number;
  sourceId: number;
  bookUrl: string;
  name: string;
  author: string;
  intro: string;
  lastReadIndex: number;
  lastReadOffset: number;
}

export interface OverlayChapter {
  bookName: string;
  index: number;
  total: number;
  name: string;
  content: string;
}

export interface OverlayState {
  visible: boolean;
}

export interface DownloadProgress {
  sourceId: number;
  bookUrl: string;
  bookName: string;
  done: number;
  total: number;
  chapterName: string;
  fromCache: boolean;
  complete: boolean;
}

export interface DownloadResult {
  sourceId: number;
  bookUrl: string;
  total: number;
  downloaded: number;
  fromCache: number;
  failed: number;
}

export interface DataSettings {
  /** 本次进程实际使用的数据目录 */
  activeDir: string;
  /** 默认数据目录（程序根目录 docs） */
  defaultDir: string;
  /** 已保存的自定义目录，null 表示跟随默认 */
  customDir: string | null;
  /** 已保存的设置与本次进程不一致，重启后生效 */
  restartRequired: boolean;
}

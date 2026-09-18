// Rust 커맨드·이벤트의 타입과 얇은 래퍼. Rust 쪽 serde 구조와 1:1 로 맞춘다.
// Tauri 밖(브라우저 미리보기)에서는 `mock.ts` 의 가짜 백엔드로 바꿔 끼운다.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { mockApi, mockOn } from "./mock";

export const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// ---- 피드 ------------------------------------------------------------------ //

export type FeedKind = "session" | "commit" | "meeting" | "note";
export type SourceState = "ok" | "skipped" | "error" | "disabled";

export interface FeedItem {
  id: string;
  kind: FeedKind;
  start: string | null;
  end: string | null;
  time: string;
  end_time: string | null;
  project: string | null;
  label: string;
  detail: string | null;
  agent: string | null;
  files: number;
  insertions: number;
  deletions: number;
  active: boolean;
  note_id: number | null;
  tags: string[];
  mentions: string[];
  /** 저장된 기록에만 남은 항목 — 원본(세션 로그·커밋)이 더 이상 없다. */
  archived: boolean;
}

export interface FeedKpis {
  commits: number;
  sessions: number;
  meetings: number;
  notes: number;
  tokens: number;
  insertions: number;
  deletions: number;
}

/** 저장된 하루 스냅샷에서 뽑은 지표(달력·목록에 붙이는 한 줄). */
export interface DayKpis {
  date: string;
  kpis: FeedKpis;
  stored_at: string;
}

export interface SourceStatus {
  name: string;
  state: SourceState;
  count: number;
  note: string | null;
}

/** 피드를 어디서 얻었는지. live=오늘 엔진 스냅샷, stored=day_feeds 에서 읽음, collected=방금 다시 수집. */
export type FeedSource = "live" | "stored" | "collected";

export interface Feed {
  date: string;
  tz_name: string;
  items: FeedItem[];
  kpis: FeedKpis;
  statuses: SourceStatus[];
  warnings: string[];
  built_at: string;
  last_event_at: string | null;
  source: FeedSource;
  /** 저장된 기록을 마지막으로 쓴 시각. live 면 null. */
  stored_at: string | null;
}

export interface FeedDelta {
  added: FeedItem[];
  updated: FeedItem[];
  removed: string[];
  kpis: FeedKpis;
  last_event_at: string | null;
}

export interface FeedChanged {
  reason: string;
  delta: FeedDelta;
  feed: Feed;
}

// ---- 메모 · 실행 · 문서 ------------------------------------------------------- //

export interface Note {
  id: number;
  date: string;
  ts: string;
  text: string;
  tags: string[];
  mentions: string[];
  source: string;
  created: string;
  updated: string;
  deleted: boolean;
}

export interface Run {
  id: number;
  date: string;
  kind: "manual" | "auto" | "retry";
  started: string;
  finished: string | null;
  status: "running" | "ok" | "failed" | "cancelled" | "abandoned";
  error: string | null;
  duration_ms: number | null;
}

export interface Document {
  date: string;
  summary_md: string | null;
  full_md: string;
  generated_at: string;
  edited_at: string | null;
  run_id: number | null;
  /** 이 문서를 만든 일지 템플릿 id. 템플릿이 생기기 전 문서는 null. */
  template: string | null;
}

export interface DocStatus {
  date: string;
  edited: boolean;
}

export interface SinkResult {
  name: string;
  ok: boolean;
  location: string | null;
  error: string | null;
}

// ---- 생성 ------------------------------------------------------------------ //

export interface GenStatus {
  run_id: number;
  date: string;
  kind: string;
  /** 이번 실행이 쓰는 일지 템플릿 id. */
  template: string;
  step: string;
  detail: string;
  started: string;
}

/** 고를 수 있는 일지 템플릿 한 개(설정 칩 · '다시 생성' 메뉴). */
export interface TemplateInfo {
  id: string;
  /** 표준 · 보고용 · 회고용 */
  name: string;
  description: string;
  /** 문서에 들어가는 절 제목(차례대로). */
  sections: string[];
}

export interface GenProgress {
  run_id: number;
  step: string;
  detail: string;
}

export interface GenDone {
  run_id: number;
  date: string;
  status: "ok" | "failed" | "cancelled";
  error: string | null;
  sinks: SinkResult[];
  has_summary: boolean;
}

export interface Reminder {
  date: string;
  mode: "notify" | "generate";
  run_id: number | null;
  missed: boolean;
  at: string;
}

export const EDITED_PREFIX = "편집된 일지가 있습니다";

// ---- 설정 ------------------------------------------------------------------ //

export interface Config {
  timezone: string;
  include_raw_data: boolean;
  summarizer: {
    provider: string;
    model: string;
    language: string;
    max_tokens: number;
    map_reduce_chars: number;
    map_workers: number;
    /** 기본 일지 템플릿 id — standard | report | retro. */
    template: string;
  };
  outputs: {
    markdown: { enabled: boolean; dir?: string };
    obsidian: { enabled: boolean; vault_dir: string; subdir: string };
    notion: {
      enabled: boolean;
      parent_type: string;
      parent_id: string;
      title_prop: string;
      version: string;
      token: string;
    };
  };
  sources: {
    git: {
      enabled: boolean;
      repos: string[];
      scan_roots: string[];
      scan_all_drives: boolean;
      scan_depth: number;
      author: string;
      authors: string[];
      include_claude_cwds: boolean;
    };
    claude: {
      enabled: boolean;
      projects_dir: string;
      include_read: boolean;
      max_intent_len: number;
      max_qa_turns: number;
      max_answer_len: number;
    };
    codex: {
      enabled: boolean;
      sessions_dir: string;
      include_read: boolean;
      max_intent_len: number;
      max_qa_turns: number;
      max_answer_len: number;
      max_lines: number;
    };
    naverworks: {
      enabled: boolean;
      user_id: string;
      calendar_id: string;
      calendar_ids: string[];
      calendars: CalendarInfo[];
      scope: string;
      client_id: string;
      client_secret: string;
      service_account: string;
      private_key: string;
      private_key_path: string;
    };
    /** 요약 프롬프트에서 뺄 저장소·폴더 글롭 (예: `D:\works\a-corp\**`). */
    exclude: string[];
  };
  automation: {
    schedule: {
      enabled: boolean;
      time: string;
      weekdays: number[];
      mode: "notify" | "generate";
    };
    realtime: {
      enabled: boolean;
      meeting_poll_min: number;
      full_rescan_min: number;
    };
    autostart: boolean;
    notify: boolean;
    global_shortcut: string;
  };
  /** 화면 모양 — 테마 · 글꼴 · 글자 크기. 값은 `appearance.ts` 가 <html> 의 data-* 로 옮긴다. */
  appearance: {
    theme: "system" | "light" | "dark";
    font: "rounded" | "sketch" | "plain";
    text_size: "small" | "normal" | "large";
  };
}

export interface CalendarInfo {
  calendar_id: string;
  name: string;
}

export interface SecretsPresence {
  naverworks_client_secret: boolean;
  naverworks_private_key: boolean;
  notion_token: boolean;
}

export interface SettingsView {
  config: Config;
  secrets: SecretsPresence;
  path: string;
  status: "loaded" | "missing" | "corrupted" | "read_failed";
  status_detail: string | null;
  safe_to_save: boolean;
  autostart_enabled: boolean;
  shortcut_error: string | null;
  claude_cli: string | null;
}

/** 요약기 준비 상태 — '지금 일지 만들기' 옆 배지(누르기 전에 미리 알리는 경고). */
export interface SummarizerStatus {
  /** 실제로 쓰이게 될 provider — claude_cli | anthropic_api | none */
  provider: string;
  /** 설정에 적힌 값 그대로 — auto | claude_cli | anthropic_api | none | (알 수 없는 값) */
  configured: string;
  /** false 면 문서는 만들어지되 AI 요약이 빠진다(부분 성공). */
  ready: boolean;
  /** 사람이 읽는 한 줄 이유. */
  detail: string;
}

export interface Check {
  ok: boolean;
  message: string;
}

export interface DriveInfo {
  path: string;
  label: string;
}

export interface UpdateInfo {
  version: string;
  current: string;
  notes: string | null;
  date: string | null;
}

export interface AppInfo {
  version: string;
  settings_path: string;
  db_path: string;
  markdown_dir: string;
  store_ok: boolean;
  store_error: string | null;
  refreshing: boolean;
  generating: GenStatus | null;
  last_reminder: Reminder | null;
  shortcut_error: string | null;
}

// ---- 커맨드 ----------------------------------------------------------------- //

const tauriApi = {
  appInfo: () => invoke<AppInfo>("app_info"),
  appQuit: () => invoke<void>("app_quit"),

  feedToday: () => invoke<Feed | null>("feed_today"),
  /** 특정 날짜의 피드. 오늘이면 실시간 스냅샷, 지난 날짜면 저장된 기록(없거나 recollect 면 원본에서 다시 수집). */
  feedFor: (date: string, recollect = false) => invoke<Feed>("feed_for", { date, recollect }),
  /** [from, to] 안에서 저장된 스냅샷이 있는 날의 지표(날짜 오름차순). 오늘은 실시간 스냅샷이 있으면 그쪽. */
  dayKpis: (from: string, to: string) => invoke<DayKpis[]>("day_kpis", { from, to }),
  refreshNow: () => invoke<void>("refresh_now"),
  refreshCalendar: () => invoke<void>("refresh_calendar"),
  rescanRepos: () => invoke<void>("rescan_repos"),

  /** at: RFC3339 시각(선택) — 주면 그 시각(=그 날짜)으로 메모를 남긴다. */
  noteAdd: (text: string, source?: string, at?: string) => invoke<Note>("note_add", { text, source, at }),
  noteEdit: (id: number, text: string) => invoke<boolean>("note_edit", { id, text }),
  noteDelete: (id: number) => invoke<boolean>("note_delete", { id }),
  notesFor: (date?: string) => invoke<Note[]>("notes_for", { date }),

  /**
   * 편집된 일지가 있으면 "편집된 일지가 있습니다…" 오류 — 확인 후 overwriteEdited=true 로 다시 부른다.
   * template 을 주면 이번 한 번만 그 템플릿으로 만든다(비우면 설정의 기본 템플릿).
   */
  generateStart: (date?: string, overwriteEdited = false, template?: string) =>
    invoke<number>("generate_start", { date, overwriteEdited, template }),
  generateCancel: () => invoke<boolean>("generate_cancel"),
  generateStatus: () => invoke<GenStatus | null>("generate_status"),
  /** 고를 수 있는 일지 템플릿 목록(바뀌지 않으므로 화면에서 한 번만 읽어 둔다). */
  templates: () => invoke<TemplateInfo[]>("templates"),
  runsRecent: (limit?: number) => invoke<Run[]>("runs_recent", { limit }),

  documentGet: (date: string) => invoke<Document | null>("document_get", { date }),
  documentSave: (date: string, fullMd: string, exportFiles = false) =>
    invoke<SinkResult[]>("document_save", { date, fullMd, export: exportFiles }),
  documentDates: (limit?: number) => invoke<string[]>("document_dates", { limit }),
  documentCalendar: (from: string, to: string) =>
    invoke<DocStatus[]>("document_calendar", { from, to }),

  settingsGet: () => invoke<SettingsView>("settings_get"),
  settingsSet: (config: Config) => invoke<SettingsView>("settings_set", { config }),
  /** 요약기가 지금 쓸 수 있는 상태인지(배지·메뉴용). */
  summarizerStatus: () => invoke<SummarizerStatus>("summarizer_status"),
  testConnection: (kind: string, config?: Config) =>
    invoke<Check>("test_connection", { kind, config }),
  naverworksCalendars: (config?: Config) =>
    invoke<CalendarInfo[]>("naverworks_calendars", { config }),

  openPath: (path: string) => invoke<void>("open_path", { path }),
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  pickPath: (kind: "folder" | "file", start?: string) =>
    invoke<string | null>("pick_path", { kind, start }),
  drives: () => invoke<DriveInfo[]>("drives"),

  updateCheck: () => invoke<UpdateInfo | null>("update_check"),
  updateInstall: () => invoke<void>("update_install"),

  showMain: () => invoke<void>("show_main"),
  quickShow: () => invoke<void>("quick_show"),
  quickHide: () => invoke<void>("quick_hide"),
};

// ---- 이벤트 ----------------------------------------------------------------- //

export interface Events {
  "feed:changed": FeedChanged;
  "feed:refreshing": boolean;
  "generate:progress": GenProgress;
  "generate:done": GenDone;
  "reminder:fired": Reminder;
  "update:available": UpdateInfo;
  "engine:error": string;
  "quick:show": null;
  /** settings_set 이 저장한 뒤 모든 창에 알리는 설정(비밀 값은 지워진 채). */
  "settings:changed": Config;
}

export type Api = typeof tauriApi;
export const api: Api = isTauri ? tauriApi : (mockApi as unknown as Api);

export function on<K extends keyof Events>(
  name: K,
  handler: (payload: Events[K]) => void,
): Promise<UnlistenFn> {
  if (!isTauri) return mockOn(name, handler);
  return listen<Events[K]>(name, (e) => handler(e.payload));
}

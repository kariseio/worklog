// 브라우저(Tauri 밖)에서 화면을 확인하기 위한 가짜 백엔드. `ipc.ts` 가 Tauri 가 없을 때만 쓴다.
// 데이터는 와이어프레임의 예시 하루를 흉내 낸다. 생성은 타이머로 진행 이벤트를 흘린다.
import type {
  AppInfo,
  CalendarInfo,
  Check,
  Config,
  DocStatus,
  Document,
  DriveInfo,
  Events,
  Feed,
  FeedItem,
  GenStatus,
  Note,
  Run,
  SettingsView,
  SinkResult,
  UpdateInfo,
} from "./ipc";

type Handler<K extends keyof Events> = (payload: Events[K]) => void;
const listeners = new Map<keyof Events, Set<Handler<keyof Events>>>();

function emit<K extends keyof Events>(name: K, payload: Events[K]) {
  listeners.get(name)?.forEach((h) => (h as Handler<K>)(payload));
}

export function mockOn<K extends keyof Events>(name: K, handler: Handler<K>): Promise<() => void> {
  if (!listeners.has(name)) listeners.set(name, new Set());
  listeners.get(name)!.add(handler as Handler<keyof Events>);
  return Promise.resolve(() => listeners.get(name)?.delete(handler as Handler<keyof Events>));
}

const pad = (n: number) => String(n).padStart(2, "0");
const today = new Date();
const dateStr = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const at = (hh: number, mm: number, dayOffset = 0) => {
  const d = new Date(today);
  d.setDate(d.getDate() + dayOffset);
  d.setHours(hh, mm, 0, 0);
  return d.toISOString();
};
const TODAY = dateStr(today);
const shift = (n: number) => {
  const d = new Date(today);
  d.setDate(d.getDate() + n);
  return dateStr(d);
};
/** "YYYY-MM-DD" + 시:분 → ISO(로컬 기준). */
const atOn = (day: string, hh: number, mm: number) => {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(y, m - 1, d, hh, mm, 0, 0).toISOString();
};
/** 오늘로부터 며칠 전인지. */
const daysAgo = (day: string) => {
  const [y, m, d] = day.split("-").map(Number);
  const a = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  return Math.round((a - new Date(y, m - 1, d).getTime()) / 86_400_000);
};

let noteSeq = 4;
const notes: Note[] = [
  mkNote(1, at(10, 35), "김팀장 구두 요청 — 결제 API 타임아웃 3초 → 10초로 늘려달라 #요청 @김팀장", "app"),
  mkNote(2, at(12, 40), "배포 일정 목요일 오후로 확정 (박PM 전화) #결정", "quick"),
  mkNote(3, at(14, 20), "내일 할 일 — 타임아웃 변경 QA 요청 #할일", "app"),
];

function mkNote(id: number, ts: string, text: string, source: string): Note {
  const tags = [...text.matchAll(/#([^\s#@]+)/g)].map((m) => m[1]);
  const mentions = [...text.matchAll(/@([^\s#@]+)/g)].map((m) => m[1]);
  return { id, date: dateStr(new Date(ts)), ts, text, tags, mentions, source, created: ts, updated: ts, deleted: false };
}

function sysItems(): FeedItem[] {
  const base = { detail: null, agent: null, files: 0, insertions: 0, deletions: 0, active: false, note_id: null, tags: [], mentions: [], archived: false };
  return [
    { ...base, id: "s:1", kind: "session", start: at(9, 12), end: at(11, 40), time: "09:12", end_time: "11:40", project: "Daily Work Log", label: "업무일지 생성기 UI 재개편", detail: "main", agent: "claude", files: 12 },
    { ...base, id: "m:1", kind: "meeting", start: at(10, 0), end: at(10, 30), time: "10:00", end_time: "10:30", project: null, label: "주간 스프린트 회의", detail: "참석 6명" },
    { ...base, id: "c:1", kind: "commit", start: at(11, 2), end: null, time: "11:02", end_time: null, project: "kms_backend", label: "fix(api): 결제 타임아웃 10초로 상향", detail: "a1b2c3d", insertions: 12, deletions: 4, files: 2 },
    { ...base, id: "s:2", kind: "session", start: at(13, 30), end: at(14, 25), time: "13:30", end_time: "14:25", project: "kms_frontend", label: "로그인 상태 배지 갱신 버그", detail: "feature/badge", agent: "codex", files: 3, active: true },
  ];
}

function noteItem(n: Note): FeedItem {
  const t = new Date(n.ts);
  return {
    id: `n:${n.id}`, kind: "note", start: n.ts, end: null, time: `${pad(t.getHours())}:${pad(t.getMinutes())}`, end_time: null,
    project: null, label: n.text, detail: null, agent: null, files: 0, insertions: 0, deletions: 0, active: false,
    note_id: n.id, tags: n.tags, mentions: n.mentions, archived: false,
  };
}

function todayNotes(): Note[] {
  return notes.filter((n) => !n.deleted && n.date === TODAY);
}

function buildFeed(): Feed {
  const items = [...sysItems(), ...todayNotes().map(noteItem)].sort((a, b) => (a.start ?? "").localeCompare(b.start ?? ""));
  return {
    date: TODAY, tz_name: "Asia/Seoul", items,
    kpis: { commits: 1, sessions: 2, meetings: 1, notes: todayNotes().length, tokens: 18_400, insertions: 12, deletions: 4 },
    statuses: [
      { name: "git", state: "ok", count: 1, note: null },
      { name: "claude", state: "ok", count: 1, note: null },
      { name: "codex", state: "ok", count: 1, note: null },
      { name: "naverworks", state: "ok", count: 1, note: null },
    ],
    warnings: [], built_at: new Date().toISOString(), last_event_at: at(14, 26),
    source: "live", stored_at: null,
  };
}

/**
 * 지난 날짜의 가짜 기록 — 세션 2 · 커밋 1 · 회의 1 + 그날 메모.
 * 7일보다 오래된 날은 세션 하나가 '기록만 남음'(원본 트랜스크립트가 지워진 상황).
 */
function pastFeed(day: string, recollect: boolean): Feed {
  const base = { detail: null, agent: null, files: 0, insertions: 0, deletions: 0, active: false, note_id: null, tags: [], mentions: [], archived: false };
  const t = (hh: number, mm: number) => atOn(day, hh, mm);
  const gone = daysAgo(day) > 7;
  const sys: FeedItem[] = [
    { ...base, id: `s:${day}:1`, kind: "session", start: t(9, 40), end: t(11, 5), time: "09:40", end_time: "11:05", project: "kms_backend", label: "검색 노드 V2 인덱스 갱신 스크립트 정리", detail: "main", agent: "claude", files: 6, archived: gone },
    { ...base, id: `m:${day}:1`, kind: "meeting", start: t(11, 0), end: t(11, 40), time: "11:00", end_time: "11:40", project: null, label: "코드리뷰 — 검색 API 응답 구조 합의", files: 4 },
    { ...base, id: `c:${day}:1`, kind: "commit", start: t(11, 48), end: null, time: "11:48", end_time: null, project: "kms_backend", label: "refactor(search): 인덱스 갱신 스크립트 정리", detail: "9f3c1aa", insertions: 84, deletions: 27, files: 5 },
    { ...base, id: `s:${day}:2`, kind: "session", start: t(13, 10), end: t(15, 2), time: "13:10", end_time: "15:02", project: "kms_frontend", label: "검색 결과 하이라이트 컴포넌트 초안", detail: "feature/highlight", agent: "codex", files: 3 },
  ];
  const mine = notes.filter((n) => !n.deleted && n.date === day).map(noteItem);
  const items = [...sys, ...mine].sort((a, b) => (a.start ?? "").localeCompare(b.start ?? ""));
  return {
    date: day, tz_name: "Asia/Seoul", items,
    kpis: { commits: 1, sessions: 2, meetings: 1, notes: mine.length, tokens: 12_600, insertions: 84, deletions: 27 },
    statuses: [
      { name: "git", state: "ok", count: 1, note: null },
      { name: "claude", state: gone ? "skipped" : "ok", count: gone ? 0 : 1, note: gone ? "세션 로그 보관 기간(30일)이 지났습니다" : null },
      { name: "codex", state: "ok", count: 1, note: null },
      { name: "naverworks", state: "ok", count: 1, note: null },
    ],
    warnings: [], built_at: atOn(day, 18, 40), last_event_at: atOn(day, 15, 2),
    source: recollect ? "collected" : "stored",
    stored_at: recollect ? new Date().toISOString() : atOn(day, 18, 42),
  };
}

let feed = buildFeed();
function publish(reason: string, added: FeedItem[] = [], updated: FeedItem[] = [], removed: string[] = []) {
  feed = buildFeed();
  emit("feed:changed", { reason, delta: { added, updated, removed, kpis: feed.kpis, last_event_at: feed.last_event_at }, feed });
}

const docs = new Map<string, Document>();
const SAMPLE_MD = (d: string) => `# 📝 업무일지 ${d}

## 한 줄 요약
검색 노드 V2 인덱스 스크립트 정리(커밋), 코드리뷰 회의, 이팀장 구두 요청(검색 결과 하이라이트) 초안 착수.

## 🕘 시간대별 흐름
**09–12시**
- 09:20 kms_backend — 검색 노드 V2 인덱스 갱신 스크립트 정리 → 10:48 커밋
- 11:00 (회의) 코드리뷰 — 검색 API 응답 구조 합의
- 11:40 [메모 · 요청] 이팀장: 검색 결과에 키워드 하이라이트 넣어달라 (다음 스프린트)

**13–18시**
- 13:10 kms_frontend — 검색 결과 하이라이트 컴포넌트 초안 (3파일)
- 15:30 [메모 · 결정] 검색 V2 배포는 다음 주 화요일로 (박PM)
- 17:05 [메모 · 할일] 하이라이트 성능 확인, QA 케이스 작성

## 📁 프로젝트별
- **kms_backend** — 검색 노드 V2 인덱스 갱신 스크립트 정리
- **kms_frontend** — 검색 결과 하이라이트 초안 (요청: 이팀장)

## ✅ 할 일
- 하이라이트 성능 확인, QA 케이스 작성

## 📊 지표
커밋 1 (+84 / −27) · 세션 2 · 회의 1 · 메모 3 · 활동 09:20–17:05
`;
for (const [off, edited] of [[-1, true], [-2, false], [-4, false], [-5, false], [-8, false]] as const) {
  const d = shift(off);
  docs.set(d, {
    date: d, summary_md: "## 한 줄 요약\n…", full_md: SAMPLE_MD(d), generated_at: at(18, 42, off),
    edited_at: edited ? at(20, 15, off) : null, run_id: 100 + off,
  });
}

let runSeq = 200;
const runs: Run[] = [
  { id: 104, date: shift(-1), kind: "manual", started: at(18, 40, -1), finished: at(18, 42, -1), status: "ok", error: null, duration_ms: 134_000 },
  { id: 103, date: shift(-2), kind: "auto", started: at(18, 30, -2), finished: at(18, 32, -2), status: "ok", error: null, duration_ms: 118_000 },
  { id: 102, date: shift(-3), kind: "manual", started: at(18, 31, -3), finished: at(18, 35, -3), status: "failed", error: "claude CLI 요약 실패: 시간 초과(240초)", duration_ms: 240_000 },
  { id: 101, date: shift(-4), kind: "manual", started: at(18, 50, -4), finished: at(18, 53, -4), status: "ok", error: "일부 내보내기 실패 — notion: HTTP 401", duration_ms: 182_000 },
];

const config: Config = {
  timezone: "Asia/Seoul", include_raw_data: false,
  summarizer: { provider: "auto", model: "claude-opus-4-8", language: "ko", max_tokens: 4000, map_reduce_chars: 20000, map_workers: 4 },
  outputs: {
    markdown: { enabled: true, dir: "" },
    obsidian: { enabled: true, vault_dir: "D:\\notes\\vault", subdir: "업무일지" },
    notion: { enabled: false, parent_type: "page", parent_id: "", title_prop: "Name", version: "2022-06-28", token: "" },
  },
  sources: {
    git: { enabled: true, repos: [], scan_roots: [], scan_all_drives: true, scan_depth: 5, author: "", authors: [], include_claude_cwds: true },
    claude: { enabled: true, projects_dir: "", include_read: false, max_intent_len: 300, max_qa_turns: 120, max_answer_len: 180 },
    codex: { enabled: true, sessions_dir: "", include_read: false, max_intent_len: 300, max_qa_turns: 120, max_answer_len: 180, max_lines: 200000 },
    naverworks: { enabled: true, user_id: "me@company.com", calendar_id: "", calendar_ids: ["cal-1"], calendars: [{ calendar_id: "cal-1", name: "내 캘린더" }, { calendar_id: "cal-2", name: "팀 일정" }], scope: "calendar.read", client_id: "abc123", client_secret: "", service_account: "svc@company", private_key: "", private_key_path: "" },
  },
  automation: {
    schedule: { enabled: true, time: "18:30", weekdays: [1, 2, 3, 4, 5], mode: "notify" },
    realtime: { enabled: true, meeting_poll_min: 15, full_rescan_min: 30 },
    autostart: true, notify: true, global_shortcut: "Ctrl+Shift+Space",
  },
  appearance: { theme: "system", font: "rounded", text_size: "normal" },
};

let gen: GenStatus | null = null;
let genTimer: ReturnType<typeof setTimeout> | null = null;

function settingsView(): SettingsView {
  return {
    config: structuredClone(config),
    secrets: { naverworks_client_secret: true, naverworks_private_key: true, notion_token: false },
    path: "C:\\Users\\me\\.worklog\\settings.json", status: "loaded", status_detail: null, safe_to_save: true,
    autostart_enabled: config.automation.autostart, shortcut_error: null, claude_cli: "C:\\Users\\me\\AppData\\Roaming\\npm\\claude.cmd",
  };
}

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

export const mockApi = {
  appInfo: async (): Promise<AppInfo> => ({
    version: "0.2.0", settings_path: "C:\\Users\\me\\.worklog\\settings.json", db_path: "C:\\Users\\me\\.worklog\\worklog.db",
    markdown_dir: "C:\\Users\\me\\Documents\\업무일지", store_ok: true, store_error: null, refreshing: false, generating: gen,
    last_reminder: null, shortcut_error: null,
  }),
  appQuit: async () => {},
  feedToday: async () => feed,
  feedFor: async (date: string, recollect = false): Promise<Feed> => {
    const day = date === "today" ? TODAY : date === "yesterday" ? shift(-1) : date;
    await wait(recollect ? 800 : 220);
    return day === TODAY ? feed : pastFeed(day, recollect);
  },
  refreshNow: async () => {
    emit("feed:refreshing", true);
    await wait(900);
    emit("feed:refreshing", false);
    publish("manual");
  },
  refreshCalendar: async () => publish("calendar"),
  rescanRepos: async () => {
    emit("feed:refreshing", true);
    await wait(1500);
    emit("feed:refreshing", false);
    publish("rescan");
  },
  noteAdd: async (text: string, source?: string, at?: string) => {
    const t = text.trim();
    if (!t) throw "빈 메모는 저장하지 않습니다.";
    const n = mkNote(++noteSeq, at ?? new Date().toISOString(), t, source ?? "app");
    notes.push(n);
    // 지난 날짜에 남긴 메모는 오늘 피드에 안 들어간다(백엔드도 같은 동작).
    publish("notes", n.date === TODAY ? [noteItem(n)] : []);
    return n;
  },
  noteEdit: async (id: number, text: string) => {
    const n = notes.find((x) => x.id === id);
    if (!n) return false;
    Object.assign(n, mkNote(id, n.ts, text.trim(), n.source), { updated: new Date().toISOString() });
    publish("notes", [], [noteItem(n)]);
    return true;
  },
  noteDelete: async (id: number) => {
    const n = notes.find((x) => x.id === id);
    if (!n) return false;
    n.deleted = true;
    publish("notes", [], [], [`n:${id}`]);
    return true;
  },
  notesFor: async () => notes.filter((n) => !n.deleted),

  generateStart: async (date?: string, overwriteEdited = false) => {
    const d = date ?? TODAY;
    if (gen) throw `이미 생성 중입니다 (${gen.date} · ${gen.step})`;
    const doc = docs.get(d);
    if (doc?.edited_at && !overwriteEdited) throw `편집된 일지가 있습니다(${d}). 다시 만들면 편집한 내용이 사라집니다.`;
    const id = ++runSeq;
    gen = { run_id: id, date: d, kind: "manual", step: "준비", detail: "", started: new Date().toISOString() };
    runs.unshift({ id, date: d, kind: "manual", started: gen.started, finished: null, status: "running", error: null, duration_ms: null });
    const steps: [string, string, number][] = [["수집", "", 900], ["요약", "세션 0/7", 700], ["요약", "세션 3/7", 900], ["요약", "세션 7/7", 700], ["요약", "종합", 1200], ["저장", "", 600]];
    let i = 0;
    const tick = () => {
      if (!gen) return;
      if (i < steps.length) {
        const [step, detail, ms] = steps[i++];
        gen = { ...gen, step, detail };
        emit("generate:progress", { run_id: id, step, detail });
        genTimer = setTimeout(tick, ms);
      } else {
        docs.set(d, { date: d, summary_md: "## 한 줄 요약\n…", full_md: SAMPLE_MD(d), generated_at: new Date().toISOString(), edited_at: null, run_id: id });
        const r = runs.find((x) => x.id === id)!;
        Object.assign(r, { finished: new Date().toISOString(), status: "ok", duration_ms: Date.now() - new Date(r.started).getTime() });
        gen = null;
        emit("generate:done", { run_id: id, date: d, status: "ok", error: null, has_summary: true, sinks: [{ name: "markdown", ok: true, location: `C:\\Users\\me\\Documents\\업무일지\\${d}.md`, error: null }, { name: "obsidian", ok: true, location: `D:\\notes\\vault\\업무일지\\${d}.md`, error: null }] });
      }
    };
    genTimer = setTimeout(tick, 300);
    return id;
  },
  generateCancel: async () => {
    if (!gen) return false;
    if (genTimer) clearTimeout(genTimer);
    emit("generate:progress", { run_id: gen.run_id, step: "취소 중", detail: "" });
    const id = gen.run_id, d = gen.date;
    const r = runs.find((x) => x.id === id);
    if (r) Object.assign(r, { finished: new Date().toISOString(), status: "cancelled", duration_ms: Date.now() - new Date(r.started).getTime() });
    gen = null;
    setTimeout(() => emit("generate:done", { run_id: id, date: d, status: "cancelled", error: null, sinks: [], has_summary: false }), 400);
    return true;
  },
  generateStatus: async () => gen,
  runsRecent: async (limit = 20) => runs.slice(0, limit),

  // 실제 백엔드처럼 매번 새 객체를 돌려준다(같은 객체를 돌려주면 화면이 갱신을 못 알아챈다).
  documentGet: async (date: string) => {
    const d = docs.get(date);
    return d ? { ...d } : null;
  },
  documentSave: async (date: string, fullMd: string, exportFiles = false): Promise<SinkResult[]> => {
    const doc = docs.get(date);
    if (!doc) throw `${date} 문서가 없습니다. 먼저 생성하세요.`;
    doc.full_md = fullMd;
    doc.edited_at = new Date().toISOString();
    return exportFiles ? [{ name: "markdown", ok: true, location: `…\\${date}.md`, error: null }] : [];
  },
  documentDates: async () => [...docs.keys()].sort().reverse(),
  documentCalendar: async (from: string, to: string): Promise<DocStatus[]> =>
    [...docs.values()].filter((d) => d.date >= from && d.date <= to).map((d) => ({ date: d.date, edited: !!d.edited_at })),

  settingsGet: async () => settingsView(),
  settingsSet: async (c: Config) => {
    Object.assign(config, structuredClone(c));
    // 실제 백엔드처럼 저장 뒤 모든 창에 알린다(빠른 메모 창이 모양 변경을 받아 간다).
    emit("settings:changed", structuredClone(config));
    return settingsView();
  },
  testConnection: async (kind: string): Promise<Check> => {
    await wait(700);
    return kind === "notion" ? { ok: false, message: "Notion 토큰을 입력하세요." } : { ok: true, message: "연결됨 · 액세스 토큰 발급 성공" };
  },
  naverworksCalendars: async (): Promise<CalendarInfo[]> => {
    await wait(600);
    return [{ calendar_id: "cal-1", name: "내 캘린더" }, { calendar_id: "cal-2", name: "팀 일정" }, { calendar_id: "cal-3", name: "회의실 예약" }];
  },
  openPath: async () => {},
  openUrl: async (url: string) => {
    window.open(url, "_blank", "noopener");
  },
  pickPath: async (kind: "folder" | "file") => (kind === "file" ? "C:\\keys\\naverworks.pem" : "D:\\notes\\vault"),
  drives: async (): Promise<DriveInfo[]> => [{ path: "C:\\", label: "Windows" }, { path: "D:\\", label: "Data" }],
  updateCheck: async (): Promise<UpdateInfo | null> => {
    await wait(800);
    return null;
  },
  updateInstall: async () => {},
  showMain: async () => {},
  quickShow: async () => {},
  quickHide: async () => {},
};

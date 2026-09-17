// 일지 화면 — 왼쪽: 검색 · 달력 · 일지 목록, 오른쪽: 문서(보기/편집) · 저장 상태.
import { For, Match, Show, Switch, createEffect, createMemo, createResource, createSignal, on, onCleanup, onMount, untrack } from "solid-js";
import { Button, Chip, Empty, Icon, Pill, Spinner } from "../components/ui";
import { api, type DocStatus, type Document, type FeedKpis, type SinkResult } from "../ipc";
import {
  errText,
  feed,
  feedReason,
  genProgressRatio,
  generating,
  gotoDay,
  info,
  journalDate,
  lastDone,
  setJournalDate,
  settings,
  startGenerate,
  toast,
} from "../store";
import { TEMPLATE_FALLBACK, loadTemplates, templateName } from "../templates";
import {
  debounce,
  fmtDateLong,
  fmtDateShort,
  fmtTime,
  isWeekend,
  parseDate,
  renderMarkdown,
  shareableMarkdown,
  toDateStr,
  todayStr,
} from "../util";
import "./journal.css";

// ---- 날짜 도우미 --------------------------------------------------------------- //

const WEEKDAYS = ["일", "월", "화", "수", "목", "금", "토"];
const pad2 = (n: number) => String(n).padStart(2, "0");

/** "2026-09-14" → "2026-09". */
const ymOf = (date: string) => date.slice(0, 7);

function shiftMonth(ym: string, delta: number): string {
  const [y, m] = ym.split("-").map(Number);
  const d = new Date(y, m - 1 + delta, 1);
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}`;
}

function monthTitle(ym: string): string {
  const [y, m] = ym.split("-").map(Number);
  return `${y}년 ${m}월`;
}

interface DayCell {
  date: string;
  day: number;
  inMonth: boolean;
  weekend: boolean;
}

/** 달력 격자: 그 달을 품는 일요일~토요일 주들(앞뒤 다른 달 날짜 포함). */
function monthGrid(ym: string): DayCell[] {
  const [y, m] = ym.split("-").map(Number);
  const first = new Date(y, m - 1, 1);
  const last = new Date(y, m, 0);
  const start = new Date(y, m - 1, 1 - first.getDay());
  const end = new Date(y, m - 1, last.getDate() + (6 - last.getDay()));
  const cells: DayCell[] = [];
  for (const d = new Date(start); d <= end; d.setDate(d.getDate() + 1)) {
    const dow = d.getDay();
    cells.push({ date: toDateStr(d), day: d.getDate(), inMonth: d.getMonth() === m - 1, weekend: dow === 0 || dow === 6 });
  }
  return cells;
}

function daysInMonth(ym: string): number {
  const [y, m] = ym.split("-").map(Number);
  return new Date(y, m, 0).getDate();
}

/** unknown: 달력 자료가 아직 그 날짜 범위를 덮지 않는다(달을 바꾼 직후 · 읽기 실패). */
type DayStatus = "generated" | "edited" | "none" | "unknown";

/** 달력 자료 — 어느 범위를 읽어 온 것인지 같이 둔다(달을 바꾸는 동안 이전 달 상태를 새 달에 씌우지 않게). */
interface CalData {
  from: string;
  to: string;
  list: DocStatus[];
}

/** 날짜별 지표 — 달력과 같은 범위를 한 번에 읽는다(범위를 같이 둬 달을 바꾸는 동안 섞이지 않게). */
interface KpiData {
  from: string;
  to: string;
  map: Map<string, FeedKpis>;
}

const kpiEmpty = (k: FeedKpis) => !k.commits && !k.sessions && !k.meetings && !k.notes;

/** "커밋 2 · 세션 3 · 메모 1" — 0 인 항목은 빼고, 모두 0 이면 "활동 없음". */
function kpiLine(k: FeedKpis): string {
  const parts: string[] = [];
  if (k.commits) parts.push(`커밋 ${k.commits}`);
  if (k.sessions) parts.push(`세션 ${k.sessions}`);
  if (k.meetings) parts.push(`회의 ${k.meetings}`);
  if (k.notes) parts.push(`메모 ${k.notes}`);
  return parts.length ? parts.join(" · ") : "활동 없음";
}

// ---- 문서 표시 ----------------------------------------------------------------- //

/** 렌더된 HTML 의 "[메모 · 요청]" 을 노란 칩으로. (우리 출력에서 이 패턴은 태그 속성 안에 오지 않는다.) */
function memoChips(html: string): string {
  return html.replace(/\[메모(?:\s*·\s*([^\]]+))?\]/g, (_m, kind?: string) =>
    `<span class="chip chip-memo">메모${kind ? ` · ${kind.trim()}` : ""}</span>`,
  );
}

/** 검색어가 든 줄을 짧게 잘라 보여준다. */
function snippet(md: string, q: string): string | null {
  const i = md.toLowerCase().indexOf(q);
  if (i < 0) return null;
  const ls = md.lastIndexOf("\n", i) + 1;
  let le = md.indexOf("\n", i);
  if (le < 0) le = md.length;
  let line = md.slice(ls, le).replace(/^[#>*\-\s]+/, "").trim();
  if (line.length > 80) {
    const from = Math.max(0, i - ls - 30);
    line = `${from > 0 ? "…" : ""}${line.slice(from, from + 80)}…`;
  }
  return line;
}

/**
 * 클립보드에 텍스트와 HTML 을 함께 넣는다 — 붙여 넣는 곳(메일·Notion·워드)이 서식을 살릴 수 있게.
 * 웹뷰가 `ClipboardItem` 을 막으면 마크다운 텍스트만 넣는다.
 */
async function writeClipboard(text: string, html: string): Promise<void> {
  if (typeof ClipboardItem === "function" && typeof navigator.clipboard?.write === "function") {
    try {
      await navigator.clipboard.write([
        new ClipboardItem({
          "text/plain": new Blob([text], { type: "text/plain" }),
          "text/html": new Blob([html], { type: "text/html" }),
        }),
      ]);
      return;
    } catch {
      // 서식 복사를 막는 웹뷰 — 아래 텍스트 복사로 떨어진다.
    }
  }
  await navigator.clipboard.writeText(text);
}

const SINK_LABEL: Record<string, string> = { markdown: "로컬 md", obsidian: "Obsidian", notion: "Notion" };
const sinkLabel = (name: string) => SINK_LABEL[name] ?? name;

interface SinkRow {
  key: string;
  label: string;
  /** ok: 저장됨 · failed: 실패 · idle: 켜져 있으나 문서 없음 · off: 꺼짐 */
  state: "ok" | "failed" | "idle" | "off";
  error?: string | null;
  time?: string;
}

// ---- 임시 편집본 --------------------------------------------------------------- //

/**
 * 다른 화면으로 넘어가면 이 컴포넌트가 사라진다(확인창을 띄울 수 없다). 저장하지 않은 편집을 날짜별로
 * 남겨 두었다가 돌아와서 '편집'을 누르면 되살린다. 저장하거나 버리면 지운다.
 */
const drafts = new Map<string, string>();
const [draftsVer, setDraftsVer] = createSignal(0);

function stashDraft(date: string, text: string | null) {
  if (text === null) drafts.delete(date);
  else drafts.set(date, text);
  setDraftsVer((v) => v + 1);
}

// ---- 화면 ------------------------------------------------------------------- //

export default function Journal() {
  // 오늘: 1분마다 다시 계산(자정을 넘기면 바뀐다). 피드의 날짜가 더 앞서면(엔진 시간대 기준) 그쪽을 따른다.
  const [clockToday, setClockToday] = createSignal(todayStr());
  const clock = setInterval(() => {
    const t = todayStr();
    if (t !== clockToday()) setClockToday(t);
  }, 60_000);
  const today = createMemo(() => {
    const f = feed()?.date;
    const t = clockToday();
    return f && f > t ? f : t;
  });

  // 선택 날짜 · 보이는 달
  const [selected, setSelected] = createSignal<string | null>(null);
  const [month, setMonth] = createSignal(ymOf(today()));

  // 편집
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  let textareaEl: HTMLTextAreaElement | undefined;

  // 검색
  const [query, setQuery] = createSignal("");
  const [q, setQ] = createSignal("");
  const [searching, setSearching] = createSignal(false);
  // 디바운스 중에 지워졌으면 옛 값을 쓰지 않는다.
  const applyQuery = debounce((v: string) => {
    if (!disposed && query() === v) setQ(v);
  }, 120);

  // 이 화면에서 시작한 생성(스토어의 generating 이 채워지기 전 잠깐을 메운다).
  const [localGen, setLocalGen] = createSignal<{ run_id: number; date: string } | null>(null);
  // 날짜별 마지막 저장 결과(생성 완료 · 편집 저장).
  const [sinks, setSinks] = createSignal<Map<string, SinkResult[]>>(new Map());

  // 문서 캐시(검색 · 목록의 생성 시각). 갱신은 cacheVer 로 알린다.
  const cache = new Map<string, Document>();
  const missing = new Set<string>();
  const [cacheVer, setCacheVer] = createSignal(0);
  const remember = (d: Document) => {
    cache.set(d.date, d);
    missing.delete(d.date);
    setCacheVer((v) => v + 1);
  };
  const forget = (date: string) => {
    cache.delete(date);
    missing.delete(date);
    setCacheVer((v) => v + 1);
  };
  let disposed = false;

  // ---- 자료 ----------------------------------------------------------------- //
  // 자료 읽기 실패는 fetcher 안에서 잡는다(실패한 resource 를 읽으면 throw 되어 화면이 깨진다).

  const [docErr, setDocErr] = createSignal<{ date: string; msg: string } | null>(null);
  const [doc, { refetch: refetchDoc }] = createResource(selected, async (date): Promise<Document | null> => {
    try {
      const d = await api.documentGet(date);
      if (d) remember(d);
      else forget(date);
      setDocErr(null);
      return d;
    } catch (e) {
      const msg = errText(e);
      toast(msg, "error");
      setDocErr({ date, msg });
      return null;
    }
  });
  /** 선택 날짜의 문서(자료가 이전 날짜 것이면 null). */
  const cur = () => {
    const d = doc();
    return d && d.date === selected() ? d : null;
  };
  /** 선택 날짜의 문서를 읽지 못했으면 그 오류. */
  const curErr = () => {
    const e = docErr();
    return e && e.date === selected() ? e.msg : null;
  };
  const docLoading = () => doc.loading && !cur();

  const grid = createMemo(() => monthGrid(month()));
  const [calErr, setCalErr] = createSignal<string | null>(null);
  const [cal, { refetch: refetchCal }] = createResource(month, async (ym): Promise<CalData | null> => {
    const g = monthGrid(ym);
    const from = g[0].date;
    const to = g[g.length - 1].date;
    try {
      const list = await api.documentCalendar(from, to);
      setCalErr(null);
      return { from, to, list };
    } catch (e) {
      const msg = errText(e);
      toast(msg, "error");
      setCalErr(msg);
      return null;
    }
  });
  const statusMap = createMemo(() => new Map((cal()?.list ?? []).map((s) => [s.date, s.edited])));

  // 지표는 곁들이는 정보다 — 읽지 못해도 목록은 그대로 두고(빈 Map), 토스트는 한 번만 띄운다.
  let kpiToasted = false;
  const [kpiData, { refetch: refetchKpis }] = createResource(month, async (ym): Promise<KpiData> => {
    const g = monthGrid(ym);
    const from = g[0].date;
    const to = g[g.length - 1].date;
    try {
      return { from, to, map: new Map((await api.dayKpis(from, to)).map((d) => [d.date, d.kpis])) };
    } catch (e) {
      if (!kpiToasted) {
        kpiToasted = true;
        toast(`지표를 불러오지 못했어요 · ${errText(e)}`, "error");
      }
      return { from, to, map: new Map() };
    }
  });

  /**
   * 그날 지표(없으면 null — 줄을 그리지 않는다). 오늘은 실시간 피드를 먼저 쓰고,
   * 보이는 달을 벗어난 날짜(검색 결과)는 검색 구간 지표로 받는다.
   */
  const kpisOf = (date: string): FeedKpis | null => {
    if (date === today()) {
      const f = feed();
      if (f && f.date === date) return f.kpis;
    }
    const d = kpiData();
    if (d && date >= d.from && date <= d.to) return d.map.get(date) ?? null;
    const s = searchKpis();
    if (s && date >= s.from && date <= s.to) return s.map.get(date) ?? null;
    return null;
  };

  /** 그날 스냅샷에 활동이 하나라도 있는지(문서 없는 주말 줄을 살릴지 판단). */
  const hasActivity = (date: string): boolean => {
    const k = kpisOf(date);
    return !!k && !kpiEmpty(k);
  };

  /** 날짜 상태. null: 표시할 것 없음(미래 · 문서 없는 주말). 달을 바꾸는 동안 cal() 은 이전 달 자료라 범위를 확인한다. */
  const statusOf = (date: string): DayStatus | null => {
    if (date > today()) return null;
    const c = cal();
    if (!c || date < c.from || date > c.to) return "unknown";
    const e = statusMap().get(date);
    if (e !== undefined) return e ? "edited" : "generated";
    return isWeekend(date) ? null : "none";
  };

  const searchOn = createMemo(() => (q().trim() ? true : undefined));
  const [datesErr, setDatesErr] = createSignal<string | null>(null);
  const [allDates, { refetch: refetchDates }] = createResource(searchOn, async (): Promise<string[]> => {
    try {
      const ds = await api.documentDates();
      setDatesErr(null);
      return ds;
    } catch (e) {
      const msg = errText(e);
      toast(msg, "error");
      setDatesErr(msg);
      return [];
    }
  });

  // 검색 결과는 보이는 달을 벗어난다 — 검색 대상 전체 구간("가장 이른 날..가장 늦은 날")의 지표를 한 번 읽어 둔다.
  // 구간이 그대로면 다시 읽지 않는다(검색어를 바꿔도 같은 구간이면 그대로).
  const searchSpan = createMemo<string | null>(() => {
    if (!q().trim()) return null;
    const ds = allDates();
    if (!ds || !ds.length) return null;
    let min = ds[0];
    let max = ds[0];
    for (const d of ds) {
      if (d < min) min = d;
      if (d > max) max = d;
    }
    return `${min}..${max}`;
  });
  const [searchKpis, { refetch: refetchSearchKpis }] = createResource(searchSpan, async (span): Promise<KpiData> => {
    const [from, to] = span.split("..");
    try {
      return { from, to, map: new Map((await api.dayKpis(from, to)).map((d) => [d.date, d.kpis])) };
    } catch (e) {
      if (!kpiToasted) {
        kpiToasted = true;
        toast(`지표를 불러오지 못했어요 · ${errText(e)}`, "error");
      }
      return { from, to, map: new Map() };
    }
  });

  // ---- 생성 상태 ------------------------------------------------------------- //

  /** 지금 만들고 있는 날짜(없으면 null). 스토어가 날짜를 채우기 전(시작 직후)에는 이 화면의 기록을 쓴다. */
  const genDate = (): string | null => generating()?.date || localGen()?.date || null;
  const genForSelected = () => !!selected() && genDate() === selected();
  const genLabel = () => {
    const g = generating();
    if (!g) return "시작 중…";
    return g.detail ? `${g.step} · ${g.detail}` : g.step;
  };

  /** `template` 을 주면 이번 한 번만 그 템플릿으로 만든다(비우면 설정의 기본 템플릿). */
  async function generate(date: string, template?: string) {
    if (genDate()) {
      toast(`이미 생성 중입니다 (${fmtDateShort(genDate()!)})`, "info");
      return;
    }
    setLocalGen({ run_id: -1, date });
    const id = await startGenerate(date, undefined, template);
    setLocalGen(id == null ? null : { run_id: id, date });
  }

  // ---- 템플릿('다시 생성' 옆 메뉴 · 문서에 붙는 칩) ---------------------------------- //

  const [tpls] = createResource(loadTemplates);
  const tplList = () => tpls() ?? TEMPLATE_FALLBACK;
  const [tplOpen, setTplOpen] = createSignal(false);
  // 메뉴가 열려 있는 동안만 바깥 클릭·Esc 를 듣는다. 편집·생성이 시작되면 스스로 닫힌다.
  createEffect(() => {
    if (!tplOpen()) return;
    if (editing() || genDate()) {
      setTplOpen(false);
      return;
    }
    const onDown = (e: MouseEvent) => {
      if (!(e.target as Element | null)?.closest?.(".journal-regen")) setTplOpen(false);
    };
    const onEsc = (e: KeyboardEvent) => {
      if (e.key === "Escape") setTplOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onEsc);
    onCleanup(() => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onEsc);
    });
  });

  // ---- 선택 · 초기화 ---------------------------------------------------------- //

  /** 편집 중 변경이 있으면 확인. 떠나도 되면 true. */
  function leaveEdit(): boolean {
    const date = selected();
    if (editing() && date) {
      if (draft() !== (cur()?.full_md ?? "") && !window.confirm("저장하지 않은 변경 사항이 있습니다. 버릴까요?")) return false;
      stashDraft(date, null);
    }
    setEditing(false);
    return true;
  }

  function select(date: string): boolean {
    if (date === selected()) return true;
    if (!leaveEdit()) return false;
    setSelected(date);
    setMonth(ymOf(date));
    return true;
  }

  // 다른 화면에서 넘어온 날짜(오늘 화면의 '일지 보기' 등). 같은 화면에서 또 와도 반응한다.
  createEffect(
    on(journalDate, (d) => {
      if (!d) return;
      select(d);
      setJournalDate(null);
    }),
  );

  onMount(async () => {
    const done = lastDone();
    if (done && done.status === "ok") setSinks((m) => new Map(m).set(done.date, done.sinks));
    if (selected() || journalDate()) return;
    let pick = today();
    try {
      const ds = await api.documentDates(1);
      if (ds[0]) pick = ds[0];
    } catch (e) {
      toast(errText(e), "error");
    }
    if (!disposed && !selected()) select(pick);
  });

  // 생성이 끝나면 문서 · 달력 · 검색 목록을 새로 읽는다.
  createEffect(
    on(
      lastDone,
      (d) => {
        if (!d) return;
        setLocalGen(null);
        if (d.status !== "ok") return;
        forget(d.date);
        setSinks((m) => new Map(m).set(d.date, d.sinks));
        if (d.date === selected()) void refetchDoc();
        void refetchCal();
        void refetchKpis();
        if (searchKpis.state === "ready") void refetchSearchKpis();
        if (allDates.state === "ready") void refetchDates();
      },
      { defer: true },
    ),
  );

  // 메모가 바뀌면(오늘 화면 · 빠른 메모 창) 그날 지표의 메모 수가 달라진다 — 보이는 달을 다시 읽는다.
  createEffect(
    on(
      feed,
      () => {
        if (untrack(feedReason).includes("note")) void refetchKpis();
      },
      { defer: true },
    ),
  );

  // Ctrl+S 저장(편집 중).
  const onKey = (e: KeyboardEvent) => {
    if (editing() && (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      void save();
    }
  };
  window.addEventListener("keydown", onKey);

  onCleanup(() => {
    disposed = true;
    clearInterval(clock);
    window.removeEventListener("keydown", onKey);
    // 편집 중에 화면을 떠나면 확인창을 띄울 수 없다 — 변경이 있으면 임시본으로 남긴다.
    const date = selected();
    if (date && editing()) stashDraft(date, draft() !== (cache.get(date)?.full_md ?? "") ? draft() : null);
  });

  // ---- 편집 ----------------------------------------------------------------- //

  /** 선택 날짜에 남겨 둔 임시 편집본이 있는지. */
  const hasDraft = () => {
    draftsVer();
    const s = selected();
    return !!s && drafts.has(s);
  };

  function startEdit() {
    const d = cur();
    if (!d) return;
    const kept = drafts.get(d.date);
    setDraft(kept ?? d.full_md);
    if (kept !== undefined) toast("저장하지 않았던 편집을 이어서 합니다", "info");
    setEditing(true);
    queueMicrotask(() => textareaEl?.focus());
  }

  async function save() {
    const date = selected();
    if (!date || !editing() || saving()) return;
    setSaving(true);
    try {
      const res = await api.documentSave(date, draft(), true);
      if (res.length) setSinks((m) => new Map(m).set(date, res));
      const ok = res.filter((r) => r.ok).map((r) => sinkLabel(r.name));
      toast(ok.length ? `저장했습니다 · ${ok.join(", ")}` : "저장했습니다", "ok");
      for (const r of res.filter((r) => !r.ok)) toast(`${sinkLabel(r.name)} 내보내기 실패 · ${r.error ?? ""}`, "error");
      setEditing(false);
      stashDraft(date, null);
      forget(date);
      await refetchDoc();
      void refetchCal();
    } catch (e) {
      toast(errText(e), "error");
    } finally {
      setSaving(false);
    }
  }

  async function copyDoc() {
    const d = cur();
    if (!d) return;
    try {
      await navigator.clipboard.writeText(d.full_md);
      toast("복사했습니다", "ok");
    } catch (e) {
      toast(errText(e), "error");
    }
  }

  /** 남에게 보낼 몫만 — 지표·타임라인·수집 원본·로컬 절대경로를 뺀 마크다운. 원본 문서는 건드리지 않는다. */
  async function copyShare() {
    const d = cur();
    if (!d) return;
    const md = shareableMarkdown(d.full_md);
    try {
      await writeClipboard(md, renderMarkdown(md));
      toast("공유용으로 복사했습니다 · 지표·타임라인·경로 제외", "ok");
    } catch (e) {
      toast(errText(e), "error");
    }
  }

  async function openFolder() {
    try {
      await api.openPath(info()?.markdown_dir ?? "");
    } catch (e) {
      toast(errText(e), "error");
    }
  }

  /** 문서 안의 링크: 웹뷰가 이동하지 않게 막고 http(s) 만 기본 브라우저로 연다. */
  const onDocClick = (e: MouseEvent) => {
    if (e.type === "auxclick" && e.button !== 1) return;
    const t = e.target;
    const a = t instanceof Element ? t.closest("a") : null;
    if (!a) return;
    e.preventDefault();
    const href = a.getAttribute("href") ?? "";
    if (/^https?:\/\//i.test(href)) api.openUrl(href).catch((err) => toast(errText(err), "error"));
  };

  // ---- 검색 ----------------------------------------------------------------- //
  // 검색 대상 문서를 캐시에 채운다(한 번에 8개씩). 동시에 하나만 돌고, 도는 동안 들어온 요청은 끝나고 이어서 처리한다.
  // (반응형 시그널로 진행 여부를 판단하면 effect 가 자기 자신을 다시 깨운다 — 일반 변수로 둔다.)
  let searchInFlight = false;
  let searchRequested = 0;

  async function loadDocs() {
    if (searchInFlight) return;
    searchInFlight = true;
    setSearching(true);
    try {
      let served = -1;
      while (served !== searchRequested && !disposed) {
        served = searchRequested;
        const dates = untrack(() => (q().trim() ? (allDates() ?? []) : []));
        const todo = dates.filter((d) => !cache.has(d) && !missing.has(d));
        for (let i = 0; i < todo.length && !disposed; i += 8) {
          const chunk = todo.slice(i, i + 8);
          const got = await Promise.all(chunk.map((d) => api.documentGet(d).catch(() => null)));
          got.forEach((d, k) => {
            if (d) cache.set(d.date, d);
            else missing.add(chunk[k]);
          });
          setCacheVer((v) => v + 1);
        }
      }
    } finally {
      searchInFlight = false;
      setSearching(false);
    }
  }
  createEffect(() => {
    const active = !!q().trim();
    const ds = allDates();
    if (!active || !ds) return;
    searchRequested += 1;
    void untrack(loadDocs);
  });

  interface Hit {
    date: string;
    doc: Document | undefined;
    snip: string | null;
  }
  const results = createMemo<Hit[] | null>(() => {
    const needle = q().trim().toLowerCase();
    if (!needle) return null;
    cacheVer();
    const out: Hit[] = [];
    for (const date of allDates() ?? []) {
      const d = cache.get(date);
      const dateHit = date.includes(needle) || fmtDateShort(date).toLowerCase().includes(needle);
      const snip = d ? snippet(d.full_md, needle) : null;
      if (dateHit || snip !== null) out.push({ date, doc: d, snip });
    }
    return out;
  });

  function clearQuery() {
    setQuery("");
    setQ("");
  }

  // ---- 목록 ----------------------------------------------------------------- //

  interface Entry {
    date: string;
    status: DayStatus;
    doc: Document | undefined;
  }
  const entries = createMemo<Entry[]>(() => {
    const ym = month();
    cacheVer();
    const out: Entry[] = [];
    for (let d = daysInMonth(ym); d >= 1; d--) {
      const date = `${ym}-${pad2(d)}`;
      const s = statusOf(date);
      const isToday = date === today();
      if (!isToday) {
        if (date > today()) continue;
        // 오늘은 주말이어도 항상 보인다(오늘 지표·만들기 버튼). 그 외 문서 없는 주말은
        // 그날 활동이 있을 때만 '미생성'으로 세운다(만들기 버튼이 나오게).
        if (s === null && !hasActivity(date)) continue;
        if (s === "unknown" && isWeekend(date)) continue;
      }
      out.push({ date, status: s ?? "none", doc: cache.get(date) });
    }
    return out;
  });

  // ---- 저장 상태 ------------------------------------------------------------- //

  const html = createMemo(() => {
    const d = cur();
    return d ? memoChips(renderMarkdown(d.full_md)) : "";
  });

  const sinkRows = createMemo<SinkRow[]>(() => {
    const o = settings()?.config.outputs;
    if (!o) return [];
    const sel = selected();
    const res = sel ? sinks().get(sel) : undefined;
    const d = cur();
    const time = d ? fmtTime(d.edited_at ?? d.generated_at) : "";
    const row = (key: string, enabled: boolean, offLabel?: string): SinkRow | null => {
      const label = sinkLabel(key);
      if (!enabled) return offLabel ? { key, label: offLabel, state: "off" } : null;
      const r = res?.find((x) => x.name === key);
      if (r && !r.ok) return { key, label, state: "failed", error: r.error };
      return d ? { key, label, state: "ok", time } : { key, label, state: "idle" };
    };
    return [row("markdown", o.markdown.enabled), row("obsidian", o.obsidian.enabled), row("notion", o.notion.enabled, "Notion 꺼짐")].filter(
      (r): r is SinkRow => r !== null,
    );
  });

  // ---- 렌더 ----------------------------------------------------------------- //

  const dayLabel = (c: DayCell, s: DayStatus | null) => {
    const d = parseDate(c.date);
    const st = s === "generated" ? "생성됨" : s === "edited" ? "편집됨" : s === "none" ? "미생성" : "";
    return `${d.getFullYear()}년 ${d.getMonth() + 1}월 ${d.getDate()}일 ${WEEKDAYS[d.getDay()]}요일${st ? ` · ${st}` : ""}`;
  };

  const EntryMeta = (p: { date: string; status: DayStatus; doc: Document | undefined }) => (
    <>
      <Show when={p.date === today()}>
        <span class="journal-entry-today">오늘</span>
      </Show>
      <Switch>
        <Match when={p.status === "unknown"}>
          <span class="muted small journal-entry-status" aria-label="확인 중">
            …
          </span>
        </Match>
        <Match when={p.status === "none"}>
          <span class="muted small journal-entry-status">{p.date === today() ? "아직 안 만듦" : "미생성"}</span>
        </Match>
        <Match when={p.doc}>{(d) => <span class="muted small journal-entry-status">{fmtTime(d().generated_at)} 생성</span>}</Match>
        <Match when={p.status === "generated"}>
          <span class="muted small journal-entry-status">생성됨</span>
        </Match>
      </Switch>
      <Show when={p.status === "edited"}>
        <Chip>편집됨</Chip>
      </Show>
    </>
  );

  /** 목록 항목 아래 한 줄로 붙는 그날 지표(스냅샷이 있는 날만). */
  const EntryKpis = (p: { date: string }) => (
    <Show when={kpisOf(p.date)}>
      {(k) => (
        <div class="journal-entry-kpis muted small" classList={{ "is-empty": kpiEmpty(k()) }}>
          {kpiLine(k())}
        </div>
      )}
    </Show>
  );

  const onEntryKey = (e: KeyboardEvent, date: string) => {
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      select(date);
    }
  };

  return (
    <section class="screen journal">
      <div class="journal-split">
        {/* ---- 왼쪽: 검색 · 달력 · 목록 ---- */}
        <aside class="journal-aside" aria-label="일지 목록">
          <div class="journal-search">
            <Icon name="search" size={15} class="journal-search-ic" />
            <input
              type="search"
              class="input journal-search-input"
              placeholder="일지 검색 (내용 · 태그 · @이름)"
              aria-label="일지 검색"
              value={query()}
              onInput={(e) => {
                setQuery(e.currentTarget.value);
                applyQuery(e.currentTarget.value);
              }}
              onKeyDown={(e) => {
                if (e.key === "Escape" && !e.isComposing && e.keyCode !== 229) clearQuery();
              }}
            />
            <Show when={query()}>
              <button type="button" class="journal-search-clear" aria-label="검색어 지우기" onClick={clearQuery}>
                <Icon name="x" size={13} />
              </button>
            </Show>
          </div>

          <div class="journal-cal-head">
            <Button variant="ghost" size="sm" icon="left" aria-label="이전 달" onClick={() => setMonth((m) => shiftMonth(m, -1))} />
            <button type="button" class="journal-cal-title" title="이번 달로" onClick={() => setMonth(ymOf(today()))}>
              {monthTitle(month())}
            </button>
            <Button variant="ghost" size="sm" icon="right" aria-label="다음 달" onClick={() => setMonth((m) => shiftMonth(m, 1))} />
          </div>

          <div class="journal-cal journal-cal-wd" aria-hidden="true">
            <For each={WEEKDAYS}>{(w) => <span>{w}</span>}</For>
          </div>
          <div class="journal-cal" aria-label={monthTitle(month())}>
            <For each={grid()}>
              {(c) => {
                const s = () => statusOf(c.date);
                const dotClass = () => {
                  const st = s();
                  return st && st !== "unknown" ? `journal-dot-${st}` : "journal-dot-blank";
                };
                return (
                  <button
                    type="button"
                    class="journal-day"
                    classList={{
                      "is-out": !c.inMonth,
                      "is-weekend": c.weekend,
                      "is-today": c.date === today(),
                      "is-selected": c.date === selected(),
                    }}
                    disabled={c.date > today()}
                    tabIndex={c.inMonth ? 0 : -1}
                    aria-current={c.date === selected() ? "date" : undefined}
                    aria-label={dayLabel(c, s())}
                    onClick={() => select(c.date)}
                  >
                    <span>{c.day}</span>
                    <span class={`journal-dot ${dotClass()}`} />
                  </button>
                );
              }}
            </For>
          </div>
          <div class="journal-legend">
            <span>
              <span class="journal-dot journal-dot-generated" /> 생성됨
            </span>
            <span>
              <span class="journal-dot journal-dot-edited" /> 편집됨
            </span>
            <span>
              <span class="journal-dot journal-dot-none" /> 미생성
            </span>
            <Show when={cal.loading}>
              <Spinner size={12} />
            </Show>
            <Show when={calErr() && !cal.loading}>
              <button type="button" class="journal-retry" title={`달력을 불러오지 못했어요 · ${calErr()}`} onClick={() => void refetchCal()}>
                다시 시도
              </button>
            </Show>
          </div>

          {/* 목록: 검색 중이면 전체 문서 검색 결과, 아니면 보이는 달의 일지 */}
          <Show
            when={results()}
            fallback={
              <div class="journal-list scroll" role="listbox" aria-label={`${monthTitle(month())} 일지`}>
                <For each={entries()} fallback={<div class="muted small journal-list-empty">이 달에는 일지가 없어요</div>}>
                  {(en) => (
                    <div
                      role="option"
                      tabIndex={0}
                      class="journal-entry"
                      classList={{
                        "is-selected": en.date === selected(),
                        "is-none": en.status === "none",
                        "is-today": en.date === today(),
                      }}
                      aria-selected={en.date === selected()}
                      onClick={() => select(en.date)}
                      onKeyDown={(e) => onEntryKey(e, en.date)}
                    >
                      <div class="journal-entry-row">
                        <b class="journal-entry-date">{fmtDateShort(en.date)}</b>
                        <EntryMeta date={en.date} status={en.status} doc={en.doc} />
                        <Show when={en.status === "none"}>
                          <Button
                            size="sm"
                            class="journal-entry-gen"
                            disabled={!!genDate()}
                            loading={genDate() === en.date}
                            onClick={(e) => {
                              e.stopPropagation();
                              void generate(en.date);
                            }}
                          >
                            {genDate() === en.date ? "생성 중" : "만들기"}
                          </Button>
                        </Show>
                      </div>
                      <EntryKpis date={en.date} />
                    </div>
                  )}
                </For>
              </div>
            }
          >
            {(hits) => (
              <>
                <div class="journal-results-head muted small">
                  <Switch fallback={<span>검색 결과 {hits().length}건</span>}>
                    <Match when={datesErr()}>
                      <Icon name="alert" size={12} />
                      <span>목록을 불러오지 못했어요</span>
                      <button type="button" class="journal-retry" title={datesErr() ?? undefined} onClick={() => void refetchDates()}>
                        다시 시도
                      </button>
                    </Match>
                    <Match when={searching() || allDates.loading}>
                      <Spinner size={12} />
                      <span>검색 중… {hits().length}건</span>
                    </Match>
                  </Switch>
                </div>
                <div class="journal-list scroll" role="listbox" aria-label="검색 결과">
                  <For each={hits()} fallback={<div class="muted small journal-list-empty">일치하는 일지가 없어요</div>}>
                    {(h) => (
                      <div
                        role="option"
                        tabIndex={0}
                        class="journal-entry"
                        classList={{ "is-selected": h.date === selected() }}
                        aria-selected={h.date === selected()}
                        onClick={() => select(h.date)}
                        onKeyDown={(e) => onEntryKey(e, h.date)}
                      >
                        <div class="journal-entry-row">
                          <b class="journal-entry-date">{fmtDateShort(h.date)}</b>
                          <span class="muted small journal-entry-status">{h.date.slice(0, 4)}년</span>
                          <Show when={h.doc}>{(d) => <span class="muted small journal-entry-status">{fmtTime(d().generated_at)} 생성</span>}</Show>
                          <Show when={h.doc?.edited_at}>
                            <Chip>편집됨</Chip>
                          </Show>
                        </div>
                        <Show when={h.snip}>
                          <div class="journal-entry-snip" title={h.snip ?? undefined}>
                            {h.snip}
                          </div>
                        </Show>
                        <EntryKpis date={h.date} />
                      </div>
                    )}
                  </For>
                </div>
              </>
            )}
          </Show>
        </aside>

        {/* ---- 오른쪽: 문서 ---- */}
        <section class="journal-main" aria-label="일지 문서">
          <header class="screen-header journal-head">
            <Show when={selected()} fallback={<span class="screen-title muted">일지</span>}>
              {(sel) => (
                <>
                  <span class="screen-title journal-title">{fmtDateLong(sel())}</span>
                  <Show when={cur()}>
                    {(d) => (
                      <Pill tone={d().edited_at ? "warn" : "default"} class="journal-pill">
                        {fmtTime(d().generated_at)} 생성
                        {d().edited_at ? ` · 편집됨 ${fmtTime(d().edited_at)}` : ""}
                      </Pill>
                    )}
                  </Show>
                  <Show when={templateName(tplList(), cur()?.template)}>
                    {(name) => (
                      <Chip class="journal-tplchip" title="이 일지를 만든 템플릿">
                        {name()}
                      </Chip>
                    )}
                  </Show>
                  <span class="grow" />
                  <Button
                    variant="ghost"
                    icon="today"
                    title="그날 활동 보기"
                    aria-label="그날 활동 보기"
                    onClick={() => gotoDay(sel())}
                  >
                    <span class="journal-btn-label">그날 활동 보기</span>
                  </Button>
                  <Show when={cur()}>
                    <Button
                      icon="edit"
                      title={editing() ? "편집 중" : hasDraft() ? "저장하지 않은 편집이 남아 있어요 — 이어서 편집" : "편집"}
                      aria-label={editing() ? "편집 중" : hasDraft() ? "이어서 편집" : "편집"}
                      aria-pressed={editing()}
                      classList={{ "journal-btn-on": editing(), "journal-btn-draft": !editing() && hasDraft() }}
                      onClick={() => (editing() ? leaveEdit() : startEdit())}
                    >
                      <span class="journal-btn-label">{editing() ? "편집 중" : hasDraft() ? "이어서 편집" : "편집"}</span>
                    </Button>
                    <Show
                      when={!genForSelected()}
                      fallback={
                        <Button icon="refresh" title="생성 중…" aria-label="생성 중" loading disabled>
                          <span class="journal-btn-label">생성 중…</span>
                        </Button>
                      }
                    >
                      <div class="journal-regen">
                        <Button
                          icon="refresh"
                          class="journal-regen-main"
                          title="다시 생성 · 설정의 기본 템플릿"
                          aria-label="다시 생성"
                          disabled={editing() || !!genDate()}
                          onClick={() => void generate(sel())}
                        >
                          <span class="journal-btn-label">다시 생성</span>
                        </Button>
                        <Button
                          icon="down"
                          class="journal-regen-caret"
                          title="템플릿 골라 다시 생성"
                          aria-label="템플릿 골라 다시 생성"
                          aria-haspopup="menu"
                          aria-expanded={tplOpen()}
                          disabled={editing() || !!genDate()}
                          onClick={() => setTplOpen((v) => !v)}
                        />
                        <Show when={tplOpen()}>
                          <div class="journal-tplmenu" role="menu">
                            <For each={tplList()}>
                              {(t) => (
                                <button
                                  type="button"
                                  role="menuitem"
                                  class="journal-tplitem"
                                  title={t.description || undefined}
                                  onClick={() => {
                                    setTplOpen(false);
                                    void generate(sel(), t.id);
                                  }}
                                >
                                  {t.name}
                                </button>
                              )}
                            </For>
                          </div>
                        </Show>
                      </div>
                    </Show>
                    <Button icon="copy" title="문서 전체를 마크다운으로 복사" aria-label="복사" onClick={() => void copyDoc()}>
                      <span class="journal-btn-label">복사</span>
                    </Button>
                    <Button
                      icon="external"
                      class="journal-share"
                      title="지표 · 타임라인 · 로컬 경로를 뺀 공유용으로 복사"
                      aria-label="공유용 복사"
                      onClick={() => void copyShare()}
                    >
                      <span class="journal-btn-label">공유용 복사</span>
                    </Button>
                  </Show>
                </>
              )}
            </Show>
          </header>

          <Show when={genForSelected()}>
            <div class="journal-progress" role="status" aria-live="polite">
              <Spinner size={13} />
              <span class="journal-progress-text">{genLabel()}</span>
              <span class="journal-bar" aria-hidden="true">
                <i style={{ width: `${Math.round(genProgressRatio(generating()) * 100)}%` }} />
              </span>
            </div>
          </Show>

          <div class="scroll grow journal-body" classList={{ "is-editing": editing() }}>
            <Show when={selected()}>
              {(sel) => (
                <Switch>
                  <Match when={docLoading()}>
                    <div class="journal-loading muted small">
                      <Spinner size={14} /> 불러오는 중…
                    </div>
                  </Match>
                  <Match when={curErr()}>
                    {(msg) => (
                      <div class="journal-error" role="alert">
                        <Icon name="alert" size={16} />
                        <span class="journal-error-text">일지를 불러오지 못했어요 · {msg()}</span>
                        <Button size="sm" icon="refresh" onClick={() => void refetchDoc()}>
                          다시 시도
                        </Button>
                      </div>
                    )}
                  </Match>
                  <Match when={editing() && cur()}>
                    <div class="journal-editor">
                      <textarea
                        ref={textareaEl}
                        class="textarea journal-textarea"
                        aria-label={`${fmtDateLong(sel())} 일지 편집`}
                        spellcheck={false}
                        value={draft()}
                        onInput={(e) => setDraft(e.currentTarget.value)}
                        onKeyDown={(e) => {
                          // 한글 조합 중 Esc 는 조합 취소 — 편집 종료로 취급하지 않는다.
                          if (e.key === "Escape" && !e.isComposing && e.keyCode !== 229) {
                            e.preventDefault();
                            leaveEdit();
                          }
                        }}
                      />
                      <div class="journal-editor-foot">
                        <Button variant="primary" icon="save" loading={saving()} onClick={() => void save()}>
                          저장
                        </Button>
                        <Button onClick={leaveEdit} disabled={saving()}>
                          취소
                        </Button>
                        <span class="muted small">Ctrl+S 저장 · Esc 취소 · 저장하면 설정한 곳(로컬 md · Obsidian)에도 다시 씁니다</span>
                      </div>
                    </div>
                  </Match>
                  <Match when={cur()}>
                    <div class="doc journal-doc" innerHTML={html()} onClick={onDocClick} onAuxClick={onDocClick} />
                  </Match>
                  <Match when={true}>
                    <Empty
                      icon="journal"
                      title="이 날의 일지는 아직 없어요"
                      hint={sel() === today() ? "지금까지 모인 활동으로 일지를 만듭니다." : `${fmtDateShort(sel())}의 활동을 모아 일지를 만듭니다.`}
                    >
                      <Show
                        when={!genForSelected()}
                        fallback={
                          <div class="row muted small journal-empty-gen">
                            <Spinner size={14} /> 만드는 중 · {genLabel()}
                          </div>
                        }
                      >
                        <Button variant="primary" icon="bolt" class="journal-empty-btn" disabled={!!genDate()} onClick={() => void generate(sel())}>
                          {sel() === today() ? "지금 만들기" : "이 날짜로 만들기"}
                        </Button>
                      </Show>
                    </Empty>
                  </Match>
                </Switch>
              )}
            </Show>
          </div>

          <footer class="journal-foot">
            <span class="muted">저장</span>
            <For each={sinkRows()}>
              {(r) => (
                <span
                  class={`journal-sink journal-sink-${r.state}`}
                  title={r.state === "failed" ? (r.error ?? "실패") : r.state === "off" ? "설정 · 저장 대상에서 켤 수 있어요" : undefined}
                >
                  <Switch>
                    <Match when={r.state === "ok"}>
                      <Icon name="check" size={13} class="ok" />
                    </Match>
                    <Match when={r.state === "failed"}>
                      <Icon name="alert" size={13} />
                    </Match>
                  </Switch>
                  <span>{r.label}</span>
                  <Show when={r.state === "ok" && r.time}>
                    <span class="muted">{r.time}</span>
                  </Show>
                  <Show when={r.state === "failed"}>
                    <span>실패</span>
                  </Show>
                </span>
              )}
            </For>
            <span class="grow" />
            <Button size="sm" icon="folder" onClick={() => void openFolder()}>
              폴더 열기
            </Button>
          </footer>
        </section>
      </div>
    </section>
  );
}

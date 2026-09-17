// 화면 공통 유틸 — 날짜/시간 표기, 디바운스, 마크다운 렌더.
import { marked } from "marked";

const WEEKDAYS_KO = ["일", "월", "화", "수", "목", "금", "토"];

/** "2026-09-14" → Date(로컬 자정). */
export function parseDate(s: string): Date {
  const [y, m, d] = s.split("-").map(Number);
  return new Date(y, m - 1, d);
}

/** Date → "YYYY-MM-DD"(로컬). */
export function toDateStr(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

export function todayStr(): string {
  return toDateStr(new Date());
}

/** "2026-09-14" → "9월 14일 월요일". */
export function fmtDateLong(s: string): string {
  const d = parseDate(s);
  return `${d.getMonth() + 1}월 ${d.getDate()}일 ${WEEKDAYS_KO[d.getDay()]}요일`;
}

/** "2026-09-14" → "9/14 월". */
export function fmtDateShort(s: string): string {
  const d = parseDate(s);
  return `${d.getMonth() + 1}/${d.getDate()} ${WEEKDAYS_KO[d.getDay()]}`;
}

/** ISO 시각(UTC) → "HH:MM"(로컬). */
export function fmtTime(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 밀리초 → "2분 14초". */
export function fmtDuration(ms: number | null | undefined): string {
  if (ms == null) return "";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}초`;
  const m = Math.floor(s / 60);
  const r = s % 60;
  return r ? `${m}분 ${String(r).padStart(2, "0")}초` : `${m}분`;
}

/** 오늘 기준 상대 표기: "방금", "3분 전", "14:26". */
export function fmtAgo(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "";
  const t = new Date(iso).getTime();
  const diff = Math.max(0, now - t);
  if (diff < 60_000) return "방금";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}분 전`;
  return fmtTime(iso);
}

export function isWeekend(s: string): boolean {
  const d = parseDate(s).getDay();
  return d === 0 || d === 6;
}

/** 시간대 구분 라벨(피드 구분선). */
export function daypart(hhmm: string): "오전" | "오후" | "저녁" {
  const h = Number(hhmm.slice(0, 2));
  if (h < 12) return "오전";
  if (h < 18) return "오후";
  return "저녁";
}

export function debounce<A extends unknown[]>(f: (...a: A) => void, ms: number): (...a: A) => void {
  let t: ReturnType<typeof setTimeout> | undefined;
  return (...a: A) => {
    if (t) clearTimeout(t);
    t = setTimeout(() => f(...a), ms);
  };
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

marked.use({
  gfm: true,
  breaks: false,
  renderer: {
    // 원문 HTML 은 그대로 넣지 않는다(요약은 LLM 출력이라 태그가 섞여 들어올 수 있음).
    html({ text }) {
      return escapeHtml(text);
    },
  },
});

/** 마크다운 → HTML(원문 HTML 은 이스케이프). */
export function renderMarkdown(md: string): string {
  return marked.parse(md, { async: false }) as string;
}

export function clamp(n: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, n));
}

// ---- 공유용 마크다운 ----------------------------------------------------------- //
//
// 일지 원문에서 남 앞에 두기 곤란한 것(집계 지표 · 접어 둔 타임라인 · 수집 데이터 원본 ·
// 내 PC 의 절대경로)만 덜어 내고, LLM 이 쓴 성과 · 결정 · 프로젝트 문장은 그대로 남긴다.
// 순수 함수 — 같은 입력이면 늘 같은 출력.

/** 이 제목을 만나면 그 줄부터 문서 끝까지 잘라 낸다(지표 아래는 전부 결정론적 집계). */
const SHARE_CUT_HEADINGS = ["지표", "타임라인"];

/** "## 📊 지표" → "지표", "## 오늘의 성과" → "오늘의성과". 제목 줄이 아니면 null. */
function headingKey(line: string): string | null {
  const m = /^#{1,6}\s+(.*)$/.exec(line);
  if (!m) return null;
  // 이모지 · 공백 · 구분 기호를 털어 낸 한글/영숫자만 남긴다.
  return m[1].replace(/[^0-9A-Za-z\uac00-\ud7a3]+/g, "");
}

/** `<details>…</details>` 블록 제거(닫히지 않았으면 문서 끝까지). */
function stripDetails(md: string): string {
  return md.replace(/<details\b[\s\S]*?<\/details>[^\S\n]*\n?/gi, "").replace(/<details\b[\s\S]*$/i, "");
}

/** `## 지표`(또는 `## 타임라인`) 줄부터 끝까지 잘라 낸다. */
function cutAtMetrics(md: string): string {
  const lines = md.split("\n");
  const at = lines.findIndex((l) => {
    const k = headingKey(l);
    return k !== null && SHARE_CUT_HEADINGS.includes(k);
  });
  return at < 0 ? md : lines.slice(0, at).join("\n");
}

/** "D:\study\Daily Work Log\src\util.ts" → "util.ts". */
function baseName(p: string): string {
  const parts = p.split(/[\\/]+/).filter((s) => s.length > 0);
  return parts.length ? parts[parts.length - 1] : p;
}

// 경로 앞에는 줄머리이거나 공백·따옴표·괄호 같은 구분자가 와야 한다(URL 안의 "/home/…" 오인 방지).
// 경로에 쓸 수 없는 문자(따옴표 · 꺾쇠 · 파이프 · 와일드카드)에서 멈춘다. 중간 마디는 공백을 허용하고
// ("Daily Work Log" 처럼 폴더 이름에 공백이 들어가므로), 마지막 마디는 공백에서 끊는다.
/** `D:\a\b.ts` · `C:/Users/me/b.ts` */
const WIN_PATH = /(^|[\s(\[{<"'`|])([A-Za-z]:[\\/](?:[^\\/\n\r"'`<>|*?]+[\\/])*[^\\/\s"'`<>|*?]*)/gm;
/** `/home/me/b.ts` · `/Users/me/b.ts` */
const NIX_PATH = /(^|[\s(\[{<"'`|])(\/(?:home|Users|root|mnt|media)\/(?:[^/\n\r"'`<>|*?]+\/)*[^/\s"'`<>|*?]*)/gm;

/** 내 PC 의 절대경로를 파일명만 남기고 지운다. */
function stripLocalPaths(md: string): string {
  const cut = (_m: string, lead: string, path: string) => `${lead}${baseName(path)}`;
  return md.replace(WIN_PATH, cut).replace(NIX_PATH, cut);
}

/**
 * 일지 원문(full_md) → 공유용 마크다운.
 *
 * 남기는 것: 제목 줄, `> 한 줄 요약`, LLM 이 쓴 모든 섹션(성과 · 결정 · 프로젝트별 진행 · 흐름 등).
 * 빼는 것: `## 지표` 이후 전부(지표 줄 · 프로젝트별 집중 표 · 타임라인), `<details>` 블록,
 *          로컬 절대경로(파일명만 남김).
 */
export function shareableMarkdown(fullMd: string): string {
  const body = stripLocalPaths(cutAtMetrics(stripDetails(fullMd)))
    .replace(/\n{3,}/g, "\n\n")
    .trimEnd()
    // 부록 앞에 있던 구분선(---)이 꼬리로 남지 않게.
    .replace(/(?:\n[^\S\n]*(?:-{3,}|\*{3,}|_{3,})[^\S\n]*)+$/, "")
    .trimEnd();
  return body ? `${body}\n` : "";
}

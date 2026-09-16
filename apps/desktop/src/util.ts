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

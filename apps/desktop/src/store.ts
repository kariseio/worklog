// 앱 전역 상태 — 피드·생성 진행·설정·알림·토스트. 화면들은 여기 시그널만 읽고 액션 함수를 부른다.
import { createSignal } from "solid-js";
import type { UnlistenFn } from "@tauri-apps/api/event";
import {
  EDITED_PREFIX,
  api,
  on,
  type AppInfo,
  type Config,
  type Feed,
  type GenDone,
  type GenProgress,
  type GenStatus,
  type Reminder,
  type SettingsView,
  type UpdateInfo,
} from "./ipc";

export type Screen = "today" | "journal" | "settings";

// ---- 시그널 ------------------------------------------------------------------ //

export const [screen, setScreen] = createSignal<Screen>("today");
/** 일지 화면에서 열어야 할 날짜(오늘 화면의 '일지 보기', 생성 완료 후 이동 등). */
export const [journalDate, setJournalDate] = createSignal<string | null>(null);
/** 설정 화면의 하위 탭(정보 탭의 업데이트 안내 등으로 바로 이동). */
export const [settingsTab, setSettingsTab] = createSignal<string | null>(null);

export const [feed, setFeed] = createSignal<Feed | null>(null);
export const [refreshing, setRefreshing] = createSignal(false);
/** 마지막 피드 갱신 사유(디버그·상태 표시). */
export const [feedReason, setFeedReason] = createSignal("");

/** 진행 중인 생성. null 이면 없음. step 은 "수집" | "요약" | "저장" | "취소 중" | "준비". */
export const [generating, setGenerating] = createSignal<GenStatus | null>(null);
export const [lastDone, setLastDone] = createSignal<GenDone | null>(null);

export const [reminder, setReminder] = createSignal<Reminder | null>(null);
export const [update, setUpdate] = createSignal<UpdateInfo | null>(null);
export const [info, setInfo] = createSignal<AppInfo | null>(null);
export const [settings, setSettings] = createSignal<SettingsView | null>(null);

export interface Toast {
  id: number;
  kind: "info" | "ok" | "error";
  text: string;
  /** 눌렀을 때 동작(선택). */
  action?: { label: string; run: () => void };
}
export const [toasts, setToasts] = createSignal<Toast[]>([]);
let toastSeq = 0;

export function toast(text: string, kind: Toast["kind"] = "info", opts: { ms?: number; action?: Toast["action"] } = {}) {
  const id = ++toastSeq;
  setToasts((t) => [...t, { id, kind, text, action: opts.action }]);
  const ms = opts.ms ?? (kind === "error" ? 8000 : 4000);
  if (ms > 0) setTimeout(() => dismissToast(id), ms);
  return id;
}

export function dismissToast(id: number) {
  setToasts((t) => t.filter((x) => x.id !== id));
}

/** 오류를 문자열로(백엔드는 한국어 문자열을 던진다). */
export function errText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) return String((e as { message: unknown }).message);
  return String(e);
}

// ---- 액션 -------------------------------------------------------------------- //

export function gotoJournal(date: string) {
  setJournalDate(date);
  setScreen("journal");
}

export function gotoSettings(tab?: string) {
  if (tab) setSettingsTab(tab);
  setScreen("settings");
}

export async function refreshNow() {
  try {
    await api.refreshNow();
  } catch (e) {
    toast(errText(e), "error");
  }
}

/**
 * 일지 생성. 편집된 일지가 있으면 확인을 받고 덮어쓴다.
 * 돌려주는 값: 시작했으면 run_id, 취소·실패면 null.
 */
export async function startGenerate(date?: string, confirmFn: (msg: string) => boolean = (m) => window.confirm(m)): Promise<number | null> {
  try {
    return await api.generateStart(date);
  } catch (e) {
    const msg = errText(e);
    if (msg.includes(EDITED_PREFIX)) {
      if (!confirmFn(`${msg}\n\n그래도 다시 만들까요?`)) return null;
      try {
        return await api.generateStart(date, true);
      } catch (e2) {
        toast(errText(e2), "error");
        return null;
      }
    }
    toast(msg, "error");
    return null;
  }
}

export async function cancelGenerate() {
  try {
    await api.generateCancel();
  } catch (e) {
    toast(errText(e), "error");
  }
}

export async function loadInfo(): Promise<AppInfo | null> {
  try {
    const i = await api.appInfo();
    setInfo(i);
    return i;
  } catch (e) {
    toast(errText(e), "error");
    return null;
  }
}

export async function loadSettings(): Promise<SettingsView | null> {
  try {
    const s = await api.settingsGet();
    setSettings(s);
    return s;
  } catch (e) {
    toast(errText(e), "error");
    return null;
  }
}

/** 설정 저장(변경 즉시). 실패하면 토스트 후 이전 값을 다시 읽는다. */
export async function saveSettings(cfg: Config): Promise<SettingsView | null> {
  try {
    const s = await api.settingsSet(cfg);
    setSettings(s);
    return s;
  } catch (e) {
    toast(errText(e), "error");
    await loadSettings();
    return null;
  }
}

// ---- 초기화(이벤트 구독 → 스냅샷) ------------------------------------------------ //

/**
 * 메인 창에서 한 번 호출. 리스너를 먼저 걸고 나서 스냅샷을 받는다(사이에 온 이벤트를 잃지 않게).
 * 돌려주는 함수는 정리용.
 */
export function initStore(): () => void {
  let uns: UnlistenFn[] = [];
  let disposed = false;
  void (async () => {
    const got = await Promise.all([
      on("feed:changed", (p) => {
        setFeed(p.feed);
        setFeedReason(p.reason);
      }),
      on("feed:refreshing", setRefreshing),
      on("generate:progress", (p) => {
        const g = generating();
        if (g && g.run_id === p.run_id) {
          setGenerating({ ...g, step: p.step, detail: p.detail });
          return;
        }
        // 모르는 실행(다른 창·정해진 시각 자동 생성): 날짜 등 실제 상태를 백엔드에서 받아 온다.
        void api.generateStatus().then((s) => {
          if (s && s.run_id === p.run_id) setGenerating({ ...s, step: p.step, detail: p.detail });
        });
      }),
      on("generate:done", (d) => {
        setGenerating(null);
        setLastDone(d);
        if (d.status === "ok") {
          toast(`${d.date} 일지를 만들었습니다${d.error ? ` · ${d.error}` : ""}`, d.error ? "info" : "ok", {
            action: { label: "일지 보기", run: () => gotoJournal(d.date) },
          });
        } else if (d.status === "failed") {
          toast(`일지 생성 실패 · ${d.error ?? "알 수 없는 오류"}`, "error");
        } else {
          toast("일지 생성을 취소했습니다", "info");
        }
      }),
      on("reminder:fired", setReminder),
      on("update:available", setUpdate),
      on("engine:error", (e) => toast(e, "error")),
    ]);
    if (disposed) {
      got.forEach((u) => u());
      return;
    }
    uns = got;
    try {
      const snap = await api.feedToday();
      if (snap && !feed()) setFeed(snap);
      const i = await loadInfo();
      if (i) {
        setRefreshing(i.refreshing);
        if (i.generating && !generating()) setGenerating(i.generating);
        if (i.last_reminder && !reminder()) setReminder(i.last_reminder);
      }
      await loadSettings();
    } catch (e) {
      toast(errText(e), "error");
    }
  })();
  return () => {
    disposed = true;
    uns.forEach((u) => u());
  };
}

/** 생성 진행률(0~1) — 단계 순서 기준의 대략값. */
export function genProgressRatio(g: GenStatus | GenProgress | null): number {
  if (!g) return 0;
  switch (g.step) {
    case "준비":
      return 0.05;
    case "수집":
      return 0.2;
    case "요약": {
      const m = /(\d+)\s*\/\s*(\d+)/.exec(g.detail);
      if (m) return 0.3 + 0.5 * (Number(m[1]) / Math.max(1, Number(m[2])));
      return g.detail === "종합" ? 0.85 : 0.3;
    }
    case "저장":
      return 0.95;
    case "취소 중":
      return 1;
    default:
      return 0.1;
  }
}

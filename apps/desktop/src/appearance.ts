// 화면 모양(테마 · 글꼴 · 글자 크기)을 <html> 의 data-* 로 옮긴다. 실제 색·글꼴·크기는 styles.css 가 정한다.
//   data-theme : "light" | "dark" (없으면 시스템 설정을 따름)
//   data-font  : "sketch" | "plain"
//   data-size  : "small" | "normal" | "large"
// 설정을 읽어 오기 전 첫 그림에서 깜빡이지 않게 마지막 값을 localStorage 에 남겨 둔다.
import type { Config } from "./ipc";

export type Appearance = Config["appearance"];

export const DEFAULT_APPEARANCE: Appearance = { theme: "system", font: "sketch", text_size: "normal" };

/** 마지막으로 적용한 모양(창을 새로 열 때 먼저 입히는 값). */
const STORAGE_KEY = "worklog.appearance";

const THEMES: Appearance["theme"][] = ["system", "light", "dark"];
const FONTS: Appearance["font"][] = ["sketch", "plain"];
const SIZES: Appearance["text_size"][] = ["small", "normal", "large"];

/** 저장소·백엔드에서 온 값을 훑어 아는 값만 남긴다(모르는 값은 기본값). */
function normalize(v: unknown): Appearance {
  if (!v || typeof v !== "object") return { ...DEFAULT_APPEARANCE };
  const o = v as Partial<Appearance>;
  const pick = <T extends string>(all: T[], got: unknown, fallback: T): T =>
    all.includes(got as T) ? (got as T) : fallback;
  return {
    theme: pick(THEMES, o.theme, DEFAULT_APPEARANCE.theme),
    font: pick(FONTS, o.font, DEFAULT_APPEARANCE.font),
    text_size: pick(SIZES, o.text_size, DEFAULT_APPEARANCE.text_size),
  };
}

/** 모양을 지금 창에 입히고 다음 실행을 위해 남겨 둔다. */
export function applyAppearance(a: Appearance) {
  const cur = normalize(a);
  const root = document.documentElement;
  // "시스템"은 특성을 아예 지운다 — 그래야 prefers-color-scheme 규칙이 그대로 산다.
  if (cur.theme === "system") delete root.dataset.theme;
  else root.dataset.theme = cur.theme;
  root.dataset.font = cur.font;
  root.dataset.size = cur.text_size;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(cur));
  } catch {
    // 저장소를 못 쓰는 상황(사생활 보호 모드 등)에서도 화면은 이미 바뀌었다.
  }
}

/** 첫 그림 전에 마지막으로 쓰던 모양을 입힌다(설정을 읽어 오는 사이의 깜빡임 방지). */
export function applyCachedAppearance() {
  let cached: unknown = null;
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) cached = JSON.parse(raw);
  } catch {
    // 읽지 못하거나 깨진 값이면 기본 모양으로 시작한다.
  }
  applyAppearance(normalize(cached));
}

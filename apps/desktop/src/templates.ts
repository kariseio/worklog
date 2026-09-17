// 일지 템플릿 목록 — 앱이 도는 동안 바뀌지 않으므로 한 번만 읽어 함께 쓴다.
// (설정 화면의 '일지 템플릿' 칩, 일지 화면의 '다시 생성' 메뉴)
import { api, type TemplateInfo } from "./ipc";

/** 백엔드에서 못 읽었을 때 쓰는 이름 — 고르는 칸은 늘 세 개가 보여야 한다. */
export const TEMPLATE_FALLBACK: TemplateInfo[] = [
  { id: "standard", name: "표준", description: "", sections: [] },
  { id: "report", name: "보고용", description: "", sections: [] },
  { id: "retro", name: "회고용", description: "", sections: [] },
];

let cache: Promise<TemplateInfo[]> | null = null;

/** 템플릿 목록(한 번만 부르고 나눠 쓴다). 실패하면 기본 이름들로 채우고 다음에 다시 시도한다. */
export function loadTemplates(): Promise<TemplateInfo[]> {
  cache ??= api.templates().then(
    (list) => (list.length ? list : TEMPLATE_FALLBACK),
    (e) => {
      cache = null; // 다음 화면에서 다시 시도
      console.warn("일지 템플릿 목록을 읽지 못했습니다", e);
      return TEMPLATE_FALLBACK;
    },
  );
  return cache;
}

/** 템플릿 id → 사람이 읽는 이름. 모르는 id 는 id 그대로, 없으면 null. */
export function templateName(list: TemplateInfo[], id: string | null | undefined): string | null {
  if (!id) return null;
  return list.find((t) => t.id === id)?.name ?? id;
}

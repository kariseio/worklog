import { Show, createSignal, onCleanup, onMount } from "solid-js";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { applyAppearance } from "./appearance";
import { Chip, Icon } from "./components/ui";
import { api, on } from "./ipc";
import { errText } from "./store";
import { fmtTime } from "./util";

const TAGS = ["#요청", "#결정", "#할일", "#회의"];

// 빠른 메모 팝업(전역 단축키 · 트레이 메뉴). Enter 저장 → 닫힘, Esc 닫기, 포커스 잃으면 닫힘(Rust).
// 방금 저장한 메모 한 줄을 남겨 '들어갔다'는 확신을 준다.
export default function Quick() {
  let input!: HTMLInputElement;
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [last, setLast] = createSignal<{ text: string; time: string } | null>(null);
  const [saving, setSaving] = createSignal(false);

  const focus = () => {
    setError(null);
    queueMicrotask(() => {
      input.focus();
      input.select();
    });
  };

  onMount(() => {
    // onCleanup 은 await 전에(동기적으로) 걸어야 Solid 가 소유자에 붙인다.
    const uns: UnlistenFn[] = [];
    let disposed = false;
    onCleanup(() => {
      disposed = true;
      uns.forEach((u) => u());
    });
    const keep = (u: UnlistenFn) => {
      if (disposed) u();
      else uns.push(u);
    };
    focus();
    void on("quick:show", focus).then(keep);
    // 이 창은 자체 설정을 읽지 않는다 — 모양만 받아 와 메인 창과 같은 테마·글꼴·크기로 그린다.
    void api.settingsGet().then((sv) => applyAppearance(sv.config.appearance)).catch(() => {});
    void on("settings:changed", (cfg) => applyAppearance(cfg.appearance)).then(keep);
  });

  const submit = async () => {
    const t = text().trim();
    if (!t || saving()) return;
    setSaving(true);
    try {
      const n = await api.noteAdd(t, "quick");
      setText("");
      setLast({ text: n.text, time: fmtTime(n.ts) });
      await api.quickHide();
    } catch (e) {
      setError(errText(e));
    } finally {
      setSaving(false);
    }
  };

  const addTag = (tag: string) => {
    const cur = text();
    setText(cur.includes(tag) ? cur : `${cur.trimEnd()} ${tag} `.trimStart());
    input.focus();
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault();
      void submit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      void api.quickHide();
    }
  };

  return (
    <main class="quick">
      <div class="quick-bar">
        <Icon name="note" size={15} />
        <span>빠른 메모</span>
        <span class="muted">Enter 저장 · Esc 닫기</span>
        <span class="grow" />
        <div class="quick-chips">
          {TAGS.map((t) => (
            <Chip onClick={() => addTag(t)} memo>
              {t}
            </Chip>
          ))}
        </div>
      </div>
      <input
        ref={input}
        class="quick-input"
        placeholder="구두 요청 · 결정 · 할 일 — #태그 @이름"
        value={text()}
        onInput={(e) => setText(e.currentTarget.value)}
        onKeyDown={onKey}
        autofocus
      />
      <div class="quick-foot">
        <Show when={error()}>
          <Icon name="alert" size={14} />
          <span>{error()}</span>
        </Show>
        <Show when={!error() && last()}>
          {(l) => (
            <>
              <Icon name="check" size={14} />
              <span class="muted">{l().time} 저장됨 · </span>
              <span class="grow" style={{ overflow: "hidden", "text-overflow": "ellipsis", "white-space": "nowrap" }}>
                {l().text}
              </span>
            </>
          )}
        </Show>
        <Show when={!error() && !last()}>
          <span class="muted">오늘 피드에 바로 들어갑니다 · 트레이 메뉴 또는 단축키로 어디서든</span>
        </Show>
      </div>
    </main>
  );
}

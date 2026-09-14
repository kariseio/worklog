import { createSignal, onCleanup, onMount } from "solid-js";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { api, on } from "./ipc";

// 빠른 메모 팝업(전역 단축키 · 트레이 메뉴). Enter 저장, Esc 닫기, 포커스 잃으면 닫힘(Rust).
export default function Quick() {
  let input!: HTMLInputElement;
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [saved, setSaved] = createSignal<string | null>(null);

  const focus = () => {
    setError(null);
    setSaved(null);
    queueMicrotask(() => {
      input.focus();
      input.select();
    });
  };

  onMount(() => {
    // onCleanup 은 await 전에(동기적으로) 걸어야 Solid 가 소유자에 붙인다.
    let un: UnlistenFn | null = null;
    let disposed = false;
    onCleanup(() => {
      disposed = true;
      un?.();
    });
    focus();
    void on("quick:show", focus).then((u) => {
      if (disposed) u();
      else un = u;
    });
  });

  const submit = async () => {
    const t = text().trim();
    if (!t) return;
    try {
      const n = await api.noteAdd(t, "quick");
      setText("");
      setSaved(`저장됨 · ${n.ts.slice(11, 16)}`);
      await api.quickHide();
    } catch (e) {
      setError(String(e));
    }
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
        <span>빠른 메모</span>
        <span class="muted">#태그 @이름 · Enter 저장 · Esc 닫기</span>
      </div>
      <input
        ref={input}
        class="quick-input"
        placeholder="예) #요청 @김팀장 결제 API 타임아웃 늘려달라"
        value={text()}
        onInput={(e) => setText(e.currentTarget.value)}
        onKeyDown={onKey}
        autofocus
      />
      <div class="quick-foot">
        {error() ? <span class="err">{error()}</span> : <span class="muted">{saved() ?? ""}</span>}
      </div>
    </main>
  );
}

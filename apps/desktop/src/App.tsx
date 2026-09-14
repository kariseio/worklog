import { For, Show, createResource, createSignal, onCleanup, onMount } from "solid-js";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { EDITED_PREFIX, api, on, type Feed, type GenDone, type GenProgress } from "./ipc";

// 5단계 점검용 임시 화면 — 피드·생성·설정 IPC 가 이어졌는지 본다.
// 6단계(UI)에서 오늘 · 일지 · 설정 화면(와이어프레임 A)으로 교체된다.
export default function App() {
  const [info] = createResource(() => api.appInfo());
  const [feed, setFeed] = createSignal<Feed | null>(null);
  const [reason, setReason] = createSignal("");
  const [refreshing, setRefreshing] = createSignal(false);
  const [progress, setProgress] = createSignal<GenProgress | null>(null);
  const [done, setDone] = createSignal<GenDone | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [note, setNote] = createSignal("");

  onMount(() => {
    // 순서가 중요하다: 리스너를 먼저 걸고 나서 스냅샷을 받는다(사이에 온 이벤트를 놓치지 않게).
    // onCleanup 은 await 전에 동기적으로 걸어야 Solid 소유자에 붙는다.
    let uns: UnlistenFn[] = [];
    let disposed = false;
    onCleanup(() => {
      disposed = true;
      uns.forEach((u) => u());
    });
    void (async () => {
      const got = await Promise.all([
        on("feed:changed", (p) => {
          setFeed(p.feed);
          setReason(
            `${p.reason} · +${p.delta.added.length} ~${p.delta.updated.length} -${p.delta.removed.length}`,
          );
        }),
        on("feed:refreshing", setRefreshing),
        on("generate:progress", setProgress),
        on("generate:done", (d) => {
          setDone(d);
          setProgress(null);
        }),
        on("reminder:fired", (r) =>
          setReason(`알림 발동 · ${r.date} (${r.mode}${r.missed ? ", 놓침" : ""})`),
        ),
        on("update:available", (u) => setReason(`새 버전 ${u.version}`)),
        on("engine:error", (e) => setError(e)),
      ]);
      if (disposed) {
        got.forEach((u) => u());
        return;
      }
      uns = got;
      const snapshot = await api.feedToday();
      if (snapshot && !feed()) setFeed(snapshot);
      const i = await api.appInfo();
      setRefreshing(i.refreshing);
      if (i.generating && !progress()) {
        setProgress({ run_id: i.generating.run_id, step: i.generating.step, detail: i.generating.detail });
      }
    })();
  });

  const run = (f: () => Promise<unknown>) => async () => {
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(String(e));
    }
  };

  const generate = run(async () => {
    try {
      await api.generateStart();
    } catch (e) {
      const msg = String(e);
      if (msg.includes(EDITED_PREFIX) && confirm(`${msg}\n덮어쓸까요?`)) {
        await api.generateStart(undefined, true);
      } else {
        throw e;
      }
    }
  });

  const addNote = run(async () => {
    const t = note().trim();
    if (!t) return;
    await api.noteAdd(t);
    setNote("");
  });

  return (
    <main class="shell">
      <h1>업무일지</h1>
      <p class="muted">
        v2 셸 점검 · 코어 {info()?.version ?? "…"} · 저장소 {info()?.store_ok ? "OK" : info()?.store_error}
        {info()?.shortcut_error ? ` · 단축키 오류: ${info()?.shortcut_error}` : ""}
      </p>

      <div class="row">
        <button onClick={run(() => api.refreshNow())} disabled={refreshing()}>
          {refreshing() ? "수집 중…" : "지금 갱신"}
        </button>
        <button onClick={run(() => api.rescanRepos())} disabled={refreshing()}>
          저장소 다시 찾기
        </button>
        <button onClick={generate} disabled={!!progress()}>
          지금 일지 만들기
        </button>
        <button onClick={run(() => api.generateCancel())} disabled={!progress()}>
          취소
        </button>
        <button onClick={run(() => api.quickShow())}>빠른 메모</button>
        <span class="muted">{reason()}</span>
      </div>

      <div class="row">
        <input
          placeholder="메모 · #태그 @이름"
          value={note()}
          onInput={(e) => setNote(e.currentTarget.value)}
          onKeyDown={(e) => e.key === "Enter" && !e.isComposing && addNote()}
        />
        <button onClick={addNote}>메모 추가</button>
      </div>

      <Show when={progress()}>
        {(p) => (
          <p>
            생성 중 · {p().step} {p().detail}
          </p>
        )}
      </Show>
      <Show when={done()}>
        {(d) => (
          <p>
            생성 {d().status} · {d().date} · 요약 {d().has_summary ? "있음" : "없음"} · {d().error ?? ""}{" "}
            {d().sinks.map((s) => `${s.name}:${s.ok ? "ok" : s.error}`).join(", ")}
          </p>
        )}
      </Show>
      <Show when={error()}>
        <p class="err">{error()}</p>
      </Show>

      <Show when={feed()} fallback={<p class="muted">첫 수집 대기 중…</p>}>
        {(f) => (
          <>
            <p>
              {f().date} · 커밋 {f().kpis.commits} · 세션 {f().kpis.sessions} · 회의 {f().kpis.meetings} · 메모{" "}
              {f().kpis.notes} · +{f().kpis.insertions}/−{f().kpis.deletions}
            </p>
            <p class="muted">
              {f().statuses.map((s) => `${s.name}:${s.state}(${s.count})`).join("  ")}
            </p>
            <ul class="feed">
              <For each={f().items}>
                {(it) => (
                  <li classList={{ active: it.active }}>
                    <span class="time">{it.time}</span>
                    <span class="kind">{it.kind}</span>
                    <span class="proj">{it.project ?? ""}</span>
                    <span>{it.label}</span>
                    <span class="muted">{it.detail ?? ""}</span>
                  </li>
                )}
              </For>
            </ul>
            <Show when={f().warnings.length}>
              <ul class="muted">
                <For each={f().warnings}>{(w) => <li>{w}</li>}</For>
              </ul>
            </Show>
          </>
        )}
      </Show>
    </main>
  );
}

import { createResource } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

// 0단계 뼈대: 코어 크레이트와 IPC 가 이어졌는지만 확인한다.
// 6단계(UI)에서 오늘 · 일지 · 설정 화면으로 교체된다.
export default function App() {
  const [version] = createResource(() => invoke<string>("app_version"));

  return (
    <main class="shell">
      <h1>업무일지</h1>
      <p class="muted">v2 뼈대 · 코어 {version.loading ? "…" : version()}</p>
    </main>
  );
}

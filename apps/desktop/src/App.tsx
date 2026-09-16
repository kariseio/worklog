import { Show, Switch, Match, onCleanup, onMount } from "solid-js";
import { Icon, Toasts } from "./components/ui";
import { api } from "./ipc";
import Today from "./screens/Today";
import Journal from "./screens/Journal";
import Settings from "./screens/Settings";
import {
  errText,
  gotoSettings,
  initStore,
  reminder,
  screen,
  setReminder,
  setScreen,
  setUpdate,
  settings,
  toast,
  update,
  type Screen,
} from "./store";
import { fmtDateShort, todayStr } from "./util";

const NAV: { id: Screen; label: string; icon: string }[] = [
  { id: "today", label: "오늘", icon: "today" },
  { id: "journal", label: "일지", icon: "journal" },
  { id: "settings", label: "설정", icon: "settings" },
];

// 메인 창: 왼쪽 레일(오늘 · 일지 · 설정) + 화면. 상태 구독은 store.initStore 에서.
export default function App() {
  onMount(() => onCleanup(initStore()));

  const schedule = () => settings()?.config.automation.schedule;
  const scheduleLabel = () => {
    const s = schedule();
    if (!s || !s.enabled) return null;
    return { mode: s.mode === "generate" ? "자동 생성" : "알림", time: s.time };
  };

  const installUpdate = async () => {
    try {
      toast("업데이트를 내려받는 중… 설치가 시작되면 앱이 다시 실행됩니다.", "info", { ms: 0 });
      await api.updateInstall();
    } catch (e) {
      toast(errText(e), "error");
    }
  };

  return (
    <div class="app">
      <nav class="rail" aria-label="화면">
        <div class="rail-logo" title="업무일지">
          <Icon name="bolt" size={18} />
        </div>
        {NAV.map((n) => (
          <button
            type="button"
            class={`rail-item ${screen() === n.id ? "is-active" : ""}`}
            onClick={() => setScreen(n.id)}
            aria-current={screen() === n.id ? "page" : undefined}
          >
            <Icon name={n.icon} size={20} />
            <span>{n.label}</span>
          </button>
        ))}
        <span class="rail-spacer" />
        <Show when={scheduleLabel()} fallback={<div class="rail-sched muted">시각 동작<br />꺼짐</div>}>
          {(s) => (
            <button type="button" class="rail-sched" onClick={() => gotoSettings("automation")} title="설정 · 자동화">
              <span class="dot dot-live" />
              <span>{s().mode}</span>
              <span>{s().time}</span>
            </button>
          )}
        </Show>
      </nav>

      <div class="screen-wrap">
        <Show when={update()}>
          {(u) => (
            <div class="banner banner-update">
              <Icon name="bolt" size={16} />
              <span>
                새 버전 {u().version}이 나왔습니다 (현재 {u().current}).
              </span>
              <button type="button" class="btn btn-primary btn-sm" onClick={installUpdate}>
                지금 업데이트
              </button>
              <button type="button" class="btn btn-ghost btn-sm" onClick={() => setUpdate(null)}>
                나중에
              </button>
            </div>
          )}
        </Show>
        <Show when={reminder() && reminder()!.mode === "notify" && reminder()!.date === todayStr()}>
          {(_) => (
            <div class="banner banner-reminder">
              <Icon name="bell" size={16} />
              <span>
                {fmtDateShort(reminder()!.date)} 일지를 만들 시간입니다{reminder()!.missed ? " (꺼져 있던 동안 지난 알림)" : ""}.
              </span>
              <button type="button" class="btn btn-ghost btn-sm" onClick={() => setReminder(null)}>
                닫기
              </button>
            </div>
          )}
        </Show>
        <Switch>
          <Match when={screen() === "today"}>
            <Today />
          </Match>
          <Match when={screen() === "journal"}>
            <Journal />
          </Match>
          <Match when={screen() === "settings"}>
            <Settings />
          </Match>
        </Switch>
      </div>
      <Toasts />
    </div>
  );
}

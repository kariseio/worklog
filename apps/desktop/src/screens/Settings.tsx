// 설정 화면 — 왼쪽 하위 탭(자동화 · 모양 · 수집 소스 · 저장 대상 · AI 요약 · 정보) + 오른쪽 폼.
// 저장 버튼 없음: 토글·선택·칩·찾기는 즉시, 글자·숫자 칸은 blur·Enter 때 settings_set 으로 저장하고 머리글에 결과를 알린다.
// (settings_set 마다 엔진이 감시를 다시 걸고 재수집하므로 타이핑마다 저장하지 않고, 마지막으로 보낸 값과 같으면 건너뛴다.)
import {
  For,
  Index,
  Match,
  Show,
  Switch,
  createEffect,
  createResource,
  createSignal,
  on,
  onCleanup,
  type JSX,
  type ParentProps,
} from "solid-js";
import { createStore, produce, reconcile, unwrap, type SetStoreFunction } from "solid-js/store";
import type { Appearance } from "../appearance";
import { Button, Chip, Field, Icon, Pill, Select, Spinner, TextInput, Toggle } from "../components/ui";
import { api, type CalendarInfo, type Check, type Config, type DriveInfo, type Run, type SettingsView, type UpdateInfo } from "../ipc";
import { TEMPLATE_FALLBACK, loadTemplates } from "../templates";
import {
  errText,
  generating,
  info,
  lastDone,
  loadSettings,
  setSettings,
  setSettingsTab,
  settings,
  settingsTab,
  startGenerate,
  toast,
  update,
} from "../store";
import { clamp, fmtDateShort, fmtDuration, fmtTime } from "../util";
import "./settings.css";

// ---- 탭 ------------------------------------------------------------------- //

type TabId = "automation" | "appearance" | "sources" | "outputs" | "summary" | "about";

const TABS: { id: TabId; label: string; sub: string }[] = [
  { id: "automation", label: "자동화", sub: "수집은 자동으로, 일지 작성은 원할 때" },
  { id: "appearance", label: "모양", sub: "테마 · 글꼴 · 글자 크기" },
  { id: "sources", label: "수집 소스", sub: "어디서 하루를 모을지" },
  { id: "outputs", label: "저장 대상", sub: "만든 일지를 어디에 둘지" },
  { id: "summary", label: "AI 요약", sub: "요약 문구를 어떻게 만들지" },
  { id: "about", label: "정보", sub: "버전 · 파일 위치 · 업데이트" },
];

const isTab = (s: string): s is TabId => TABS.some((t) => t.id === s);

type SaveState = { kind: "idle" } | { kind: "saving" } | { kind: "ok"; at: string } | { kind: "err"; text: string };

/** 폼 한 벌이 탭들에 넘겨주는 것: 초안 스토어 + 저장. */
interface Ctx {
  draft: Config;
  set: SetStoreFunction<Config>;
  /** 초안을 저장한다(마지막으로 맞춘 값과 같으면 건너뜀). 토글·칩·셀렉트·찾기는 바로, 글자·숫자 칸은 blur·Enter 때 부른다. */
  now: () => void;
  /** 연결 확인 등에 넘길 현재 초안 사본(빈 비밀 칸은 백엔드가 저장값으로 채움). */
  snapshot: () => Config;
  /** 비밀 칸이 초점을 잃을 때 — 저장 뒤 미뤄 둔 비우기를 실행. */
  secretBlur: (key: SecretKey) => void;
}

const SECRET_HINT = "비우면 현재 값 유지";

/** 서버가 돌려주지 않는 비밀 칸. 저장 성공 뒤 '설정됨' 자리표로 되돌리기 위해 이름을 붙여 둔다. */
type SecretKey = "nw_secret" | "nw_key" | "notion_token";
const SECRET_KEYS: SecretKey[] = ["nw_secret", "nw_key", "notion_token"];
const readSecret = (c: Config, k: SecretKey): string =>
  k === "nw_secret" ? c.sources.naverworks.client_secret : k === "nw_key" ? c.sources.naverworks.private_key : c.outputs.notion.token;
const writeSecret = (c: Config, k: SecretKey, v: string) => {
  if (k === "nw_secret") c.sources.naverworks.client_secret = v;
  else if (k === "nw_key") c.sources.naverworks.private_key = v;
  else c.outputs.notion.token = v;
};

/** 시작 때 못 읽던 설정 파일을 백엔드가 저장 시점에 다시 읽어 왔을 때 돌려주는 오류의 표식(commands.rs settings_set). */
const RELOAD_MARK = "설정 파일을 다시 읽었습니다";

export default function Settings() {
  // 다른 화면에서 gotoSettings("automation") 처럼 탭을 지정해 들어올 수 있다. 한 번 읽으면 비운다.
  const takeRequestedTab = (): TabId | null => {
    const t = settingsTab();
    if (!t) return null;
    setSettingsTab(null);
    return isTab(t) ? t : null;
  };
  const [tab, setTab] = createSignal<TabId>(takeRequestedTab() ?? "automation");
  createEffect(
    on(
      settingsTab,
      () => {
        const t = takeRequestedTab();
        if (t) setTab(t);
      },
      { defer: true },
    ),
  );

  const [saveState, setSaveState] = createSignal<SaveState>({ kind: "idle" });
  const cur = () => TABS.find((t) => t.id === tab()) ?? TABS[0];
  const path = () => info()?.settings_path ?? settings()?.path ?? "";

  return (
    <section class="screen">
      <div class="settings-body">
        <aside class="settings-aside">
          <nav class="settings-nav-list" aria-label="설정 항목">
            <For each={TABS}>
              {(t) => (
                <button
                  type="button"
                  class={`settings-nav ${tab() === t.id ? "is-active" : ""}`}
                  aria-current={tab() === t.id ? "page" : undefined}
                  onClick={() => setTab(t.id)}
                >
                  <span class="settings-nav-dot" aria-hidden="true" />
                  {t.label}
                </button>
              )}
            </For>
          </nav>
          <div class="settings-aside-foot muted small">
            v{info()?.version ?? "?"} · 설정 파일
            <span class="settings-path mono" title={path()}>
              {path()}
            </span>
          </div>
        </aside>

        <div class="settings-main">
          <header class="screen-header">
            <span class="screen-title">{cur().label}</span>
            <span class="muted small settings-header-sub">{cur().sub}</span>
            <span class="grow" />
            <SaveIndicator state={saveState()} />
          </header>
          <div class="scroll grow settings-content">
            <Show
              when={settings()}
              fallback={
                <div class="settings-loading muted small row">
                  <Spinner size={16} />
                  설정을 읽는 중…
                </div>
              }
            >
              {(sv) => <Form initial={sv().config} tab={tab()} onState={setSaveState} />}
            </Show>
          </div>
        </div>
      </div>
    </section>
  );
}

// ---- 머리글 저장 표시 ----------------------------------------------------------- //

function SaveIndicator(props: { state: SaveState }) {
  const sv = settings;
  const okAt = () => (props.state.kind === "ok" ? props.state.at : "");
  const errT = () => (props.state.kind === "err" ? props.state.text : "");
  return (
    <span class="settings-header-state small">
      <Show when={sv()?.status === "corrupted"}>
        <Pill tone="warn" title={sv()?.status_detail ?? undefined}>
          <Icon name="alert" size={13} />
          손상된 설정 파일을 백업하고 기본값으로 시작했습니다
        </Pill>
      </Show>
      <Show when={sv() && !sv()!.safe_to_save}>
        <Pill tone="warn" title={sv()?.status_detail ?? undefined}>
          <Icon name="alert" size={13} />
          설정 파일을 읽지 못해 저장이 막혀 있습니다
        </Pill>
      </Show>
      <Switch fallback={<span class="muted">변경 즉시 저장</span>}>
        <Match when={props.state.kind === "saving"}>
          <span class="muted row" role="status">
            <Spinner size={14} />
            저장 중…
          </span>
        </Match>
        <Match when={props.state.kind === "ok"}>
          <span class="muted row" role="status">
            <Icon name="check" size={14} />
            변경 즉시 저장됨 · {okAt()}
          </span>
        </Match>
        <Match when={props.state.kind === "err"}>
          <span class="err row" role="status">
            <Icon name="alert" size={14} />
            {errT()}
          </span>
        </Match>
      </Switch>
    </span>
  );
}

// ---- 폼(초안 + 저장 예약) ------------------------------------------------------- //

function Form(props: { initial: Config; tab: TabId; onState: (s: SaveState) => void }) {
  // 초안은 서버 설정의 사본. 저장에 실패해도 고친 값은 그대로 둔다(백엔드가 설정 파일을 뒤늦게 읽어 온 경우만 다시 채운다).
  const [draft, setDraft] = createStore<Config>(structuredClone(props.initial));
  // 초안이 마지막으로 맞춰진 설정 — 성공적으로 보낸 값, 또는 실패 뒤 되돌린 서버 값. 같은 값이면 저장하지 않는다.
  let base: Config = structuredClone(props.initial);
  let inflight = false;
  let again = false;
  // 저장 성공 뒤 비우려 했지만 아직 입력 중(초점)이던 비밀 칸 — 초점을 잃을 때 비운다.
  const pendingClear = new Set<SecretKey>();

  const snapshot = (): Config => structuredClone(unwrap(draft));
  const same = (a: Config, b: Config) => JSON.stringify(a) === JSON.stringify(b);

  const clearSecret = (k: SecretKey) => {
    setDraft(produce((d) => writeSecret(d, k, "")));
    writeSecret(base, k, "");
  };
  // 비밀 값은 서버가 돌려주지 않는다 — 방금 보낸 값이 아직 그대로면 칸을 비워 '설정됨' 자리표로 돌린다(빈 칸 = 유지).
  // 그 칸에 초점이 있으면 글자를 지우지 않고 blur 때 비운다.
  const forgetSentSecrets = (sent: Config) => {
    for (const k of SECRET_KEYS) {
      const v = readSecret(sent, k);
      if (!v) continue;
      const d = readSecret(draft, k);
      if (d === "") writeSecret(base, k, "");
      if (d !== v) continue;
      const el = document.activeElement;
      if (el instanceof HTMLElement && el.dataset.secret === k) pendingClear.add(k);
      else clearSecret(k);
    }
  };
  const secretBlur = (k: SecretKey) => {
    if (!pendingClear.delete(k)) return;
    // 저장 뒤 더 입력했다면 그 값은 change 로 곧 저장된다 — 여기서 지우지 않는다(저장 성공 뒤 비워짐).
    if (readSecret(draft, k) !== readSecret(base, k)) return;
    clearSecret(k);
  };

  const doSave = async () => {
    if (inflight) {
      again = true;
      return;
    }
    const sent = snapshot();
    if (same(sent, base)) return;
    inflight = true;
    props.onState({ kind: "saving" });
    // 실패했을 때 백엔드가 설정 파일을 뒤늦게 읽어 왔는지 알아보려고 보내기 전 설정을 붙잡아 둔다.
    const before = settings();
    // store.saveSettings 와 같은 흐름이지만 어떤 오류인지 알아야 해서 직접 부른다.
    let res: SettingsView | null = null;
    let err = "";
    try {
      res = await api.settingsSet(sent);
      setSettings(res);
    } catch (e) {
      err = errText(e);
      toast(err, "error");
      await loadSettings();
    }
    inflight = false;
    if (res) {
      base = sent;
      props.onState({ kind: "ok", at: fmtTime(new Date().toISOString()) });
      forgetSentSecrets(sent);
    } else {
      const after = settings();
      // 백엔드가 시작 때 못 읽던 설정 파일을 이제 읽어 왔다면(설정이 달라졌다) 화면의 초안은 기본값에서 출발한 것이라
      // 그대로 저장하면 남의 설정을 지운다 — 읽어 온 값으로 초안을 다시 채우고 사용자가 고쳐 다시 저장하게 한다.
      if (after && (err.includes(RELOAD_MARK) || !same(after.config, before?.config ?? after.config))) {
        const fresh = structuredClone(after.config);
        setDraft(reconcile(fresh));
        base = fresh;
        props.onState({ kind: "err", text: "설정을 다시 읽었습니다 · 다시 저장하세요" });
      } else {
        // 그 밖의 실패는 고친 값을 그대로 둔다 — base 도 그대로라 다음 커밋 때 다시 보낸다.
        props.onState({ kind: "err", text: "저장 실패 · 고친 값은 그대로입니다 · 다시 저장하세요" });
      }
    }
    if (again) {
      again = false;
      void doSave();
    }
  };
  const now = () => void doSave();
  // 화면을 떠날 때 아직 보내지 않은 변경(입력 중이던 글자)이 있으면 바로 보낸다.
  onCleanup(() => {
    if (!same(snapshot(), base)) void doSave();
  });

  const ctx: Ctx = { draft, set: setDraft, now, snapshot, secretBlur };

  return (
    <Switch>
      <Match when={props.tab === "automation"}>
        <AutomationTab f={ctx} />
      </Match>
      <Match when={props.tab === "appearance"}>
        <AppearanceTab f={ctx} />
      </Match>
      <Match when={props.tab === "sources"}>
        <SourcesTab f={ctx} />
      </Match>
      <Match when={props.tab === "outputs"}>
        <OutputsTab f={ctx} />
      </Match>
      <Match when={props.tab === "summary"}>
        <SummaryTab f={ctx} />
      </Match>
      <Match when={props.tab === "about"}>
        <AboutTab />
      </Match>
    </Switch>
  );
}

/** IANA 시간대 이름이면 표준 표기("asia/seoul" → "Asia/Seoul")로, 아니면 null. */
function canonicalTimeZone(tz: string): string | null {
  try {
    return new Intl.DateTimeFormat("en-US", { timeZone: tz }).resolvedOptions().timeZone;
  } catch {
    return null;
  }
}

/** Enter 키인지 — 한글 IME 조합을 끝내는 Enter(isComposing · keyCode 229)는 아니다. */
const isEnter = (e: KeyboardEvent) => e.key === "Enter" && !e.isComposing && e.keyCode !== 229;

// ---- 공용 조각 ---------------------------------------------------------------- //

/** Field 의 라벨 글자는 입력과 연결되지 않으므로, 셀렉트는 보이지 않는 라벨로 감싸 접근성 이름을 준다. */
function Labeled(props: ParentProps<{ label: string }>) {
  return (
    <label class="settings-labelwrap">
      <span class="settings-sr">{props.label}</span>
      {props.children}
    </label>
  );
}

/**
 * 여럿 중 하나를 고르는 칩 한 줄. 고른 값은 잉크로 채우고 aria-pressed 로도 알린다
 * (공용 Chip 은 눌림 상태를 보조 기술에 알리지 않아 여기서 버튼을 직접 그린다).
 */
function PickChips<T extends string>(props: { value: T; options: { value: T; label: string }[]; onPick: (v: T) => void }) {
  return (
    <div class="row">
      <For each={props.options}>
        {(o) => (
          <button
            type="button"
            class={`chip chip-click ${props.value === o.value ? "chip-on" : ""}`}
            aria-pressed={props.value === o.value}
            onClick={() => props.onPick(o.value)}
          >
            {o.label}
          </button>
        )}
      </For>
    </div>
  );
}

/** 섹션 머리: 굵은 제목(켜기/끄기 토글이 있으면 그 라벨로) + 설명 + 오른쪽 내용. */
function SecHead(props: { title: string; checked?: boolean; onToggle?: (v: boolean) => void; sub?: string; right?: JSX.Element }) {
  return (
    <div class="row settings-sec-head">
      <Show when={props.onToggle} fallback={<span class="settings-sec-title">{props.title}</span>}>
        {(toggle) => (
          <Toggle checked={!!props.checked} onChange={(v) => toggle()(v)} label={<span class="settings-sec-title">{props.title}</span>} />
        )}
      </Show>
      <Show when={props.sub}>
        <span class="muted small">{props.sub}</span>
      </Show>
      <Show when={props.right}>
        <span class="grow" />
        {props.right}
      </Show>
    </div>
  );
}

/** 경로 한 칸: 입력 + '찾기'(폴더/파일 대화상자). 글자는 초안만 바꾸고 blur·Enter(onCommit)·찾기(onPick) 때 저장. */
function PathInput(props: {
  kind: "folder" | "file";
  value: string;
  label: string;
  placeholder?: string;
  onInput: (v: string) => void;
  onCommit: () => void;
  onPick: (v: string) => void;
}) {
  const pick = async () => {
    try {
      const p = await api.pickPath(props.kind, props.value || undefined);
      if (p) props.onPick(p);
    } catch (e) {
      toast(errText(e), "error");
    }
  };
  return (
    <div class="row settings-pathrow">
      <TextInput
        class="grow"
        mono
        value={props.value}
        placeholder={props.placeholder}
        aria-label={props.label}
        onInput={(e) => props.onInput(e.currentTarget.value)}
        onChange={() => props.onCommit()}
      />
      <Button size="sm" icon="folder" onClick={() => void pick()}>
        찾기
      </Button>
    </div>
  );
}

/**
 * 비밀 값 입력. 서버는 값을 돌려주지 않으므로 '설정됨' 여부만 자리표로 알린다. 빈 칸 = 현재 값 유지.
 * 글자는 초안만 바꾸고 blur·Enter(onCommit) 때 저장. data-secret 으로 Form 이 '초점이 있는 비밀 칸'을 알아본다.
 */
function SecretInput(props: {
  value: string;
  present: boolean;
  label: string;
  secretKey: SecretKey;
  textarea?: boolean;
  onInput: (v: string) => void;
  onCommit: () => void;
  onBlur: (key: SecretKey) => void;
}) {
  const ph = () => (props.present ? "설정됨 · 바꾸려면 입력" : "입력");
  return (
    <Show
      when={props.textarea}
      fallback={
        <input
          type="password"
          class="input settings-secret settings-short"
          autocomplete="off"
          aria-label={props.label}
          data-secret={props.secretKey}
          placeholder={ph()}
          value={props.value}
          onInput={(e) => props.onInput(e.currentTarget.value)}
          onChange={() => props.onCommit()}
          onBlur={() => props.onBlur(props.secretKey)}
        />
      }
    >
      <textarea
        class="textarea settings-secret settings-secret-area"
        rows={4}
        spellcheck={false}
        autocomplete="off"
        aria-label={props.label}
        data-secret={props.secretKey}
        placeholder={ph()}
        value={props.value}
        onInput={(e) => props.onInput(e.currentTarget.value)}
        onChange={() => props.onCommit()}
        onBlur={() => props.onBlur(props.secretKey)}
      />
    </Show>
  );
}

/** 숫자 칸. blur·Enter 때 반올림해 범위 안으로 맞춰 넘긴다(비거나 숫자가 아니면 이전 값으로 되돌림). */
function NumInput(props: { value: number; label: string; min?: number; max?: number; step?: number; onChange: (n: number) => void }) {
  return (
    <input
      type="number"
      class="input settings-num"
      aria-label={props.label}
      value={props.value}
      min={props.min}
      max={props.max}
      step={props.step}
      onChange={(e) => {
        const v = Math.round(e.currentTarget.valueAsNumber);
        if (!Number.isFinite(v)) {
          e.currentTarget.value = String(props.value);
          return;
        }
        const n = clamp(v, props.min ?? 0, props.max ?? Number.MAX_SAFE_INTEGER);
        e.currentTarget.value = String(n);
        props.onChange(n);
      }}
    />
  );
}

/** 문자열 목록 편집(폴더·저장소). 행마다 입력 + 찾기 + 삭제, 아래에 추가 버튼. blur·Enter·찾기·삭제는 저장(save=true). */
function ListEditor(props: {
  items: string[];
  name: string;
  addLabel: string;
  placeholder?: string;
  pick?: "folder" | "file";
  onChange: (items: string[], save: boolean) => void;
}) {
  const update = (i: number, v: string, save: boolean) =>
    props.onChange(
      props.items.map((x, j) => (j === i ? v : x)),
      save,
    );
  const remove = (i: number) =>
    props.onChange(
      props.items.filter((_, j) => j !== i),
      true,
    );
  const add = () => props.onChange([...props.items, ""], false);
  const save = () => props.onChange([...props.items], true);
  const pickFor = async (i: number) => {
    try {
      const p = await api.pickPath(props.pick ?? "folder", props.items[i] || undefined);
      if (p) update(i, p, true);
    } catch (e) {
      toast(errText(e), "error");
    }
  };
  return (
    <div class="settings-list">
      <Index each={props.items}>
        {(item, i) => (
          <div class="row settings-list-row">
            <TextInput
              class="grow"
              mono
              value={item()}
              placeholder={props.placeholder}
              aria-label={`${props.name} ${i + 1}`}
              onInput={(e) => update(i, e.currentTarget.value, false)}
              onChange={save}
              onKeyDown={(e) => {
                if (!isEnter(e)) return;
                e.preventDefault();
                save();
              }}
            />
            <Show when={props.pick}>
              <Button size="sm" icon="folder" onClick={() => void pickFor(i)}>
                찾기
              </Button>
            </Show>
            <button type="button" class="settings-x" aria-label={`${props.name} ${i + 1} 삭제`} onClick={() => remove(i)}>
              <Icon name="x" size={14} />
            </button>
          </div>
        )}
      </Index>
      <div>
        <Button size="sm" variant="ghost" icon="plus" onClick={add}>
          {props.addLabel}
        </Button>
      </div>
    </div>
  );
}

/** 짧은 문자열 태그 목록(작성자). 칩 + 삭제, 입력 후 Enter 로 추가, Esc 로 입력 비움. */
function TagsEditor(props: { items: string[]; label: string; placeholder: string; onChange: (items: string[]) => void }) {
  const [text, setText] = createSignal("");
  const add = () => {
    const t = text().trim();
    if (!t) return;
    if (!props.items.includes(t)) props.onChange([...props.items, t]);
    setText("");
  };
  return (
    <div class="row settings-tags">
      <For each={props.items}>
        {(a) => (
          <span class="chip settings-tag-chip">
            {a}
            <button
              type="button"
              class="settings-chip-x"
              aria-label={`${a} 삭제`}
              onClick={() => props.onChange(props.items.filter((x) => x !== a))}
            >
              <Icon name="x" size={12} />
            </button>
          </span>
        )}
      </For>
      <TextInput
        class="settings-tag-input"
        value={text()}
        placeholder={props.placeholder}
        aria-label={props.label}
        onInput={(e) => setText(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            if (!isEnter(e)) return;
            e.preventDefault();
            add();
          } else if (e.key === "Escape") {
            setText("");
          }
        }}
        onBlur={add}
      />
    </div>
  );
}

/** 연결 확인 버튼 + 결과 한 줄. */
function CheckButton(props: { kind: string; label: string; cfg: () => Config; onDone?: (c: Check) => void; icon?: string }) {
  const [busy, setBusy] = createSignal(false);
  const [res, setRes] = createSignal<Check | null>(null);
  const run = async () => {
    setBusy(true);
    setRes(null);
    try {
      const c = await api.testConnection(props.kind, props.cfg());
      setRes(c);
      props.onDone?.(c);
    } catch (e) {
      setRes({ ok: false, message: errText(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div class="row settings-check">
      <Button size="sm" icon={props.icon} loading={busy()} onClick={() => void run()}>
        {props.label}
      </Button>
      <Show when={res()}>
        {(c) => (
          <span class={`small row ${c().ok ? "ok" : "err"}`} role="status">
            <Icon name={c().ok ? "check" : "x"} size={14} />
            <span class="settings-kv">{c().message}</span>
          </span>
        )}
      </Show>
    </div>
  );
}

/** 셀렉트 보기에 현재 값이 없으면 끼워 넣는다(설정 파일을 손으로 고친 경우). */
function withValue(opts: { value: number; label: string }[], v: number, unit: string) {
  return opts.some((o) => o.value === v) ? opts : [...opts, { value: v, label: `${v}${unit}` }].sort((a, b) => a.value - b.value);
}

// ---- 자동화 ------------------------------------------------------------------- //

const DAYS = [
  [1, "월"],
  [2, "화"],
  [3, "수"],
  [4, "목"],
  [5, "금"],
  [6, "토"],
  [7, "일"],
] as const;
const MODE_HINT = '알림만: "오늘 일지 만들까요?" Windows 알림 → 누르면 앱이 열리고 직접 생성 · 자동 생성: 그 시각에 만들고 저장 (AI 호출 발생)';
const POLL_MIN = [5, 10, 15, 30, 60].map((v) => ({ value: v, label: `${v}분` }));
const RESCAN_MIN = [15, 30, 60, 120].map((v) => ({ value: v, label: `${v}분` }));

function AutomationTab(props: { f: Ctx }) {
  const f = props.f;
  const a = () => f.draft.automation;
  const sch = () => a().schedule;
  const rt = () => a().realtime;

  const toggleDay = (d: number) => {
    const cur = sch().weekdays;
    const next = cur.includes(d) ? cur.filter((x) => x !== d) : [...cur, d].sort((x, y) => x - y);
    f.set("automation", "schedule", "weekdays", next);
    f.now();
  };
  const setMode = (m: "notify" | "generate") => {
    f.set("automation", "schedule", "mode", m);
    f.now();
  };
  // 저장된 설정은 켜져 있는데 Windows 등록이 안 된 경우만 알린다(저장 중 잠깐 어긋나는 건 무시).
  const autostartMismatch = () => {
    const s = settings();
    return !!s && s.config.automation.autostart && !s.autostart_enabled;
  };

  return (
    <>
      <section class={`settings-sec ${sch().enabled ? "" : "is-off"}`}>
        <SecHead
          title="정해진 시각에 일지 챙기기"
          checked={sch().enabled}
          onToggle={(v) => {
            f.set("automation", "schedule", "enabled", v);
            f.now();
          }}
        />
        <Field label="시각">
          <input
            type="time"
            class="input settings-time"
            aria-label="시각"
            value={sch().time}
            onChange={(e) => {
              const v = e.currentTarget.value;
              if (!v) return;
              f.set("automation", "schedule", "time", v);
              f.now();
            }}
          />
        </Field>
        <Field label="요일">
          <div class="row">
            <For each={DAYS}>
              {([n, label]) => (
                <Chip on={sch().weekdays.includes(n)} onClick={() => toggleDay(n)} title={sch().weekdays.includes(n) ? "켜짐" : "꺼짐"}>
                  {label}
                </Chip>
              )}
            </For>
          </div>
        </Field>
        <Field label="동작" top hint={MODE_HINT}>
          <div class="row">
            <Chip on={sch().mode === "notify"} onClick={() => setMode("notify")}>
              알림만
            </Chip>
            <Chip on={sch().mode === "generate"} onClick={() => setMode("generate")}>
              자동 생성
            </Chip>
          </div>
        </Field>
      </section>

      <section class="settings-sec">
        <SecHead
          title="실시간 수집"
          checked={rt().enabled}
          onToggle={(v) => {
            f.set("automation", "realtime", "enabled", v);
            f.now();
          }}
          sub="Claude · Codex 세션과 git 커밋을 파일 감시로 즉시 반영"
        />
        <Field label="회의 확인 주기" hint="NaverWorks 는 푸시가 없어 주기 확인">
          <Labeled label="회의 확인 주기">
            <Select
              value={rt().meeting_poll_min}
              options={withValue(POLL_MIN, rt().meeting_poll_min, "분")}
              onChange={(v) => {
                f.set("automation", "realtime", "meeting_poll_min", v);
                f.now();
              }}
            />
          </Labeled>
        </Field>
        <Field label="전체 재수집" hint="놓친 이벤트 보정 · '지금 갱신'으로 즉시">
          <Labeled label="전체 재수집 주기">
            <Select
              value={rt().full_rescan_min}
              options={withValue(RESCAN_MIN, rt().full_rescan_min, "분")}
              onChange={(v) => {
                f.set("automation", "realtime", "full_rescan_min", v);
                f.now();
              }}
            />
          </Labeled>
        </Field>
        <div class="muted small settings-note">
          <Icon name="info" size={13} />
          회의 확인·전체 재수집 주기는 실시간 수집을 꺼도 계속 돕니다
        </div>
        <Toggle
          checked={a().autostart}
          onChange={(v) => {
            f.set("automation", "autostart", v);
            f.now();
          }}
          label="Windows 시작 시 자동 실행"
          hint="창 없이 트레이에만"
        />
        <Show when={autostartMismatch()}>
          <div class="warn small settings-note">
            <Icon name="alert" size={13} />
            Windows 시작 프로그램에 아직 등록되지 않았습니다 — 설정을 바꿔 저장하면 다시 시도합니다
          </div>
        </Show>
        <Toggle
          checked={a().notify}
          onChange={(v) => {
            f.set("automation", "notify", v);
            f.now();
          }}
          label="Windows 알림"
          hint="생성 완료·실패·정해진 시각 알림"
        />
        <Field label="빠른 메모 단축키" hint="예: Ctrl+Shift+Space · 비우면 사용 안 함">
          <TextInput
            class="settings-shortcut"
            mono
            value={a().global_shortcut}
            placeholder="사용 안 함"
            aria-label="빠른 메모 단축키"
            onInput={(e) => f.set("automation", "global_shortcut", e.currentTarget.value)}
            onChange={f.now}
            onKeyDown={(e) => {
              if (!isEnter(e)) return;
              e.preventDefault();
              f.now();
            }}
          />
          <Show when={settings()?.shortcut_error}>{(err) => <div class="err small">{err()}</div>}</Show>
        </Field>
      </section>

      <RecentRuns />
    </>
  );
}

const KIND_LABEL: Record<Run["kind"], string> = { manual: "직접", auto: "자동", retry: "다시" };

function RecentRuns() {
  // 생성이 끝나거나 새로 시작하면 다시 읽는다(진행 이벤트마다 읽지 않게 run_id 만 본다).
  const key = () => `${lastDone()?.run_id ?? 0}:${generating()?.run_id ?? 0}`;
  const [runs, { refetch }] = createResource(key, () => api.runsRecent(10));
  const retry = async (r: Run) => {
    const id = await startGenerate(r.date);
    if (id != null) void refetch();
  };
  const canRetry = (r: Run) => r.status === "failed" || r.status === "cancelled" || r.status === "abandoned";

  return (
    <section class="settings-sec">
      <SecHead title="최근 생성" right={<span class="muted small">최근 10건</span>} />
      <Show when={!runs.error} fallback={<div class="err small">{errText(runs.error)}</div>}>
        <Show
          when={(runs() ?? []).length > 0}
          fallback={<div class="muted small">{runs.loading ? "불러오는 중…" : "아직 만든 일지가 없습니다"}</div>}
        >
          <table class="settings-table">
            <thead>
              <tr>
                <th>날짜</th>
                <th>시각</th>
                <th>종류</th>
                <th>결과</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={runs() ?? []}>
                {(r) => (
                  <tr>
                    <td class="settings-td-nowrap">{fmtDateShort(r.date)}</td>
                    <td class="settings-td-nowrap">{fmtTime(r.started)}</td>
                    <td class="settings-td-nowrap">{KIND_LABEL[r.kind]}</td>
                    <td>
                      <RunResult run={r} />
                    </td>
                    <td class="settings-td-action">
                      <Show when={canRetry(r)}>
                        <Button size="sm" disabled={!!generating()} onClick={() => void retry(r)}>
                          다시 시도
                        </Button>
                      </Show>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Show>
      </Show>
    </section>
  );
}

function RunResult(props: { run: Run }) {
  const r = () => props.run;
  return (
    <Switch>
      <Match when={r().status === "ok"}>
        <span class="settings-result">
          <Icon name="check" size={14} class="ok" />
          성공 · {fmtDuration(r().duration_ms)}
        </span>
        <Show when={r().error}>
          <div class="settings-result warn small">
            <Icon name="alert" size={13} />
            {r().error}
          </div>
        </Show>
      </Match>
      <Match when={r().status === "failed"}>
        <span class="settings-result err">
          <Icon name="x" size={14} />
          실패 · {r().error ?? "알 수 없는 오류"}
        </span>
      </Match>
      <Match when={r().status === "cancelled"}>
        <span class="muted">취소</span>
      </Match>
      <Match when={r().status === "abandoned"}>
        <span class="muted">중단(앱 종료)</span>
      </Match>
      <Match when={r().status === "running"}>
        <span class="settings-result">
          <Spinner size={14} />
          진행 중
        </span>
      </Match>
    </Switch>
  );
}

// ---- 모양 -------------------------------------------------------------------- //

const THEME_OPTS: { value: Appearance["theme"]; label: string }[] = [
  { value: "system", label: "시스템" },
  { value: "light", label: "라이트" },
  { value: "dark", label: "다크" },
];
const FONT_OPTS: { value: Appearance["font"]; label: string }[] = [
  { value: "rounded", label: "둥근 고딕 (기본)" },
  { value: "sketch", label: "손글씨" },
  { value: "plain", label: "기본 고딕" },
];
const SIZE_OPTS: { value: Appearance["text_size"]; label: string }[] = [
  { value: "small", label: "작게" },
  { value: "normal", label: "보통" },
  { value: "large", label: "크게" },
];

/** 미리보기 줄에만 쓰는 본문 글꼴 묶음 — styles.css 의 --font 와 같은 값이어야 한다(저장 전에도 보여 주려고 여기서 직접 지정). */
const FONT_STACK: Record<Appearance["font"], string> = {
  rounded: '"NanumSquareRound", "Pretendard", "Malgun Gothic", "Apple SD Gothic Neo", sans-serif',
  sketch: '"Gaegu", "Malgun Gothic", "Apple SD Gothic Neo", sans-serif',
  plain: '"Pretendard Variable", "Pretendard", "Segoe UI", "Malgun Gothic", "Apple SD Gothic Neo", system-ui, sans-serif',
};

/** 같은 줄에서 보여 줄 제목 글꼴 — styles.css 의 --font-display · --display-weight 와 같은 값. */
const DISPLAY_STACK: Record<Appearance["font"], string> = {
  rounded: '"Jua", ' + FONT_STACK.rounded,
  sketch: FONT_STACK.sketch,
  plain: FONT_STACK.plain,
};

const DISPLAY_WEIGHT: Record<Appearance["font"], string> = { rounded: "400", sketch: "700", plain: "700" };

const PREVIEW_TITLE = "업무일지";
const PREVIEW_TEXT = "오늘 한 일을 적어두세요 · 가나다라 ABC 123";

function AppearanceTab(props: { f: Ctx }) {
  const f = props.f;
  const a = () => f.draft.appearance;
  // 칩은 누르는 즉시 저장한다. 저장이 끝나면 백엔드의 settings:changed 로 두 창 모두 새 모양을 입는다.
  const setTheme = (v: Appearance["theme"]) => {
    f.set("appearance", "theme", v);
    f.now();
  };
  const setFont = (v: Appearance["font"]) => {
    f.set("appearance", "font", v);
    f.now();
  };
  const setSize = (v: Appearance["text_size"]) => {
    f.set("appearance", "text_size", v);
    f.now();
  };

  return (
    <section class="settings-sec">
      <Field label="테마" hint="시스템은 Windows 설정을 따릅니다">
        <PickChips value={a().theme} options={THEME_OPTS} onPick={setTheme} />
      </Field>
      <Field label="글꼴" top>
        <PickChips value={a().font} options={FONT_OPTS} onPick={setFont} />
        {/* 저장이 끝나기 전에도 고른 글꼴을 바로 보여 준다 — 초안 값으로 직접 글꼴을 준다.
            앞의 '업무일지'는 제목 글꼴, 뒤 문장은 본문 글꼴이다. */}
        <div class="box settings-preview" style={{ "font-family": FONT_STACK[a().font] }}>
          <span
            class="settings-preview-title"
            style={{ "font-family": DISPLAY_STACK[a().font], "font-weight": DISPLAY_WEIGHT[a().font] }}
          >
            {PREVIEW_TITLE}
          </span>
          {PREVIEW_TEXT}
        </div>
      </Field>
      <Field label="글자 크기">
        <PickChips value={a().text_size} options={SIZE_OPTS} onPick={setSize} />
      </Field>
      <div class="muted small settings-note">
        <Icon name="info" size={13} />
        빠른 메모 창에도 같이 적용됩니다
      </div>
    </section>
  );
}

// ---- 수집 소스 ------------------------------------------------------------------ //

const DEPTHS = Array.from({ length: 12 }, (_, i) => ({ value: i + 1, label: `${i + 1}단계` }));

function SourcesTab(props: { f: Ctx }) {
  const f = props.f;
  const g = () => f.draft.sources.git;
  const cl = () => f.draft.sources.claude;
  const cx = () => f.draft.sources.codex;
  const nw = () => f.draft.sources.naverworks;
  const secrets = () => settings()?.secrets;
  // 오류는 fetcher 안에서 잡는다 — 실패한 리소스를 읽으면 던지므로(solid 1.9) 빈 목록을 돌려주고 칸 안에 알린다.
  const [drivesErr, setDrivesErr] = createSignal<string | null>(null);
  const [drives] = createResource(async (): Promise<DriveInfo[]> => {
    try {
      return await api.drives();
    } catch (e) {
      setDrivesErr(errText(e));
      return [];
    }
  });

  const rescan = async () => {
    try {
      await api.rescanRepos();
      toast("저장소를 다시 찾는 중…");
    } catch (e) {
      toast(errText(e), "error");
    }
  };

  // NaverWorks 캘린더: 불러온 목록이 있으면 그것, 없으면 저장된 이름 목록.
  const [cals, setCals] = createSignal<CalendarInfo[] | null>(null);
  const [calBusy, setCalBusy] = createSignal(false);
  const [calErr, setCalErr] = createSignal<string | null>(null);
  const shownCals = () => cals() ?? nw().calendars;
  const loadCals = async () => {
    setCalBusy(true);
    setCalErr(null);
    try {
      setCals(await api.naverworksCalendars(f.snapshot()));
    } catch (e) {
      setCalErr(errText(e));
    } finally {
      setCalBusy(false);
    }
  };
  const toggleCal = (c: CalendarInfo) => {
    const ids = nw().calendar_ids;
    const next = ids.includes(c.calendar_id) ? ids.filter((x) => x !== c.calendar_id) : [...ids, c.calendar_id];
    const names = new Map<string, string>();
    for (const k of nw().calendars) names.set(k.calendar_id, k.name);
    for (const k of shownCals()) names.set(k.calendar_id, k.name);
    f.set("sources", "naverworks", "calendar_ids", next);
    f.set(
      "sources",
      "naverworks",
      "calendars",
      next.map((id) => ({ calendar_id: id, name: names.get(id) ?? id })),
    );
    f.now();
  };

  return (
    <>
      <section class={`settings-sec ${g().enabled ? "" : "is-off"}`}>
        <SecHead
          title="git 커밋"
          checked={g().enabled}
          onToggle={(v) => {
            f.set("sources", "git", "enabled", v);
            f.now();
          }}
        />
        <Field label="저장소 찾기" top>
          <div class="row">
            <Chip
              on={g().scan_all_drives}
              onClick={() => {
                f.set("sources", "git", "scan_all_drives", true);
                f.now();
              }}
            >
              모든 고정 디스크
            </Chip>
            <Chip
              on={!g().scan_all_drives}
              onClick={() => {
                f.set("sources", "git", "scan_all_drives", false);
                f.now();
              }}
            >
              지정한 폴더
            </Chip>
          </div>
          <Show
            when={g().scan_all_drives}
            fallback={
              <ListEditor
                items={g().scan_roots}
                name="폴더"
                addLabel="폴더 추가"
                pick="folder"
                placeholder="D:\work"
                onChange={(items, save) => {
                  f.set("sources", "git", "scan_roots", items);
                  if (save) f.now();
                }}
              />
            }
          >
            <div class="row">
              <Show when={drives.loading}>
                <Spinner size={14} />
              </Show>
              <For each={drives() ?? []}>
                {(d) => (
                  <Pill tone="muted">
                    <span class="mono">{d.path}</span> {d.label}
                  </Pill>
                )}
              </For>
              <Show when={drivesErr()}>{(m) => <span class="err small">{m()}</span>}</Show>
            </div>
          </Show>
        </Field>
        <Field label="탐색 깊이" hint="루트 아래 몇 단계까지">
          <Labeled label="탐색 깊이">
            <Select
              value={g().scan_depth}
              options={withValue(DEPTHS, g().scan_depth, "단계")}
              onChange={(v) => {
                f.set("sources", "git", "scan_depth", v);
                f.now();
              }}
            />
          </Labeled>
        </Field>
        <Field label="직접 지정 저장소" top hint="찾기 결과와 상관없이 항상 수집">
          <ListEditor
            items={g().repos}
            name="저장소"
            addLabel="저장소 추가"
            pick="folder"
            placeholder="D:\work\my-repo"
            onChange={(items, save) => {
              f.set("sources", "git", "repos", items);
              if (save) f.now();
            }}
          />
        </Field>
        <Field label="작성자" top hint="비우면 내 git 신원으로 자동 · 다른 이름·이메일로 남긴 커밋도 세려면 아래에 추가">
          <TextInput
            class="settings-shortcut"
            value={g().author}
            placeholder="이름 또는 이메일"
            aria-label="작성자"
            onInput={(e) => f.set("sources", "git", "author", e.currentTarget.value)}
            onChange={f.now}
          />
          <TagsEditor
            items={g().authors}
            label="추가 작성자"
            placeholder="이름·이메일 입력 후 Enter"
            onChange={(items) => {
              f.set("sources", "git", "authors", items);
              f.now();
            }}
          />
        </Field>
        <Toggle
          checked={g().include_claude_cwds}
          onChange={(v) => {
            f.set("sources", "git", "include_claude_cwds", v);
            f.now();
          }}
          label="세션 작업 폴더도 포함"
          hint="Claude · Codex 세션이 열렸던 폴더가 git 저장소면 함께 수집"
        />
        <div>
          <Button icon="refresh" onClick={() => void rescan()}>
            저장소 다시 찾기
          </Button>
        </div>
      </section>

      <section class={`settings-sec ${cl().enabled ? "" : "is-off"}`}>
        <SecHead
          title="Claude Code 세션"
          checked={cl().enabled}
          onToggle={(v) => {
            f.set("sources", "claude", "enabled", v);
            f.now();
          }}
        />
        <Field label="로그 폴더" hint="비우면 ~/.claude/projects">
          <PathInput
            kind="folder"
            value={cl().projects_dir}
            label="Claude 로그 폴더"
            placeholder="~/.claude/projects"
            onInput={(v) => f.set("sources", "claude", "projects_dir", v)}
            onCommit={f.now}
            onPick={(v) => {
              f.set("sources", "claude", "projects_dir", v);
              f.now();
            }}
          />
        </Field>
        <Field label="질답 상한" hint="일지에 담을 세션당 질답 수와 답변 한 개의 글자수">
          <div class="row">
            <NumInput
              value={cl().max_qa_turns}
              min={1}
              max={1000}
              label="Claude 세션당 질답 수"
              onChange={(n) => {
                f.set("sources", "claude", "max_qa_turns", n);
                f.now();
              }}
            />
            <span class="muted small">턴</span>
            <NumInput
              value={cl().max_answer_len}
              min={20}
              max={2000}
              step={10}
              label="Claude 답변 글자수"
              onChange={(n) => {
                f.set("sources", "claude", "max_answer_len", n);
                f.now();
              }}
            />
            <span class="muted small">자</span>
          </div>
        </Field>
      </section>

      <section class={`settings-sec ${cx().enabled ? "" : "is-off"}`}>
        <SecHead
          title="Codex 세션"
          checked={cx().enabled}
          onToggle={(v) => {
            f.set("sources", "codex", "enabled", v);
            f.now();
          }}
        />
        <Field label="로그 폴더" hint="비우면 ~/.codex/sessions">
          <PathInput
            kind="folder"
            value={cx().sessions_dir}
            label="Codex 로그 폴더"
            placeholder="~/.codex/sessions"
            onInput={(v) => f.set("sources", "codex", "sessions_dir", v)}
            onCommit={f.now}
            onPick={(v) => {
              f.set("sources", "codex", "sessions_dir", v);
              f.now();
            }}
          />
        </Field>
        <Field label="질답 상한" hint="일지에 담을 세션당 질답 수와 답변 한 개의 글자수">
          <div class="row">
            <NumInput
              value={cx().max_qa_turns}
              min={1}
              max={1000}
              label="Codex 세션당 질답 수"
              onChange={(n) => {
                f.set("sources", "codex", "max_qa_turns", n);
                f.now();
              }}
            />
            <span class="muted small">턴</span>
            <NumInput
              value={cx().max_answer_len}
              min={20}
              max={2000}
              step={10}
              label="Codex 답변 글자수"
              onChange={(n) => {
                f.set("sources", "codex", "max_answer_len", n);
                f.now();
              }}
            />
            <span class="muted small">자</span>
          </div>
        </Field>
        <Field label="파일 행 상한" hint="아주 큰 세션 파일은 이 행 수까지만 읽음">
          <div class="row">
            <NumInput
              value={cx().max_lines}
              min={1000}
              max={5_000_000}
              step={1000}
              label="Codex 세션 파일 행 상한"
              onChange={(n) => {
                f.set("sources", "codex", "max_lines", n);
                f.now();
              }}
            />
            <span class="muted small">행</span>
          </div>
        </Field>
      </section>

      <section class={`settings-sec ${nw().enabled ? "" : "is-off"}`}>
        <SecHead
          title="NaverWorks 회의"
          checked={nw().enabled}
          onToggle={(v) => {
            f.set("sources", "naverworks", "enabled", v);
            f.now();
          }}
          sub="캘린더 일정을 회의로 수집"
        />
        <Field label="사용자 ID">
          <TextInput
            class="settings-shortcut"
            value={nw().user_id}
            placeholder="이메일"
            aria-label="NaverWorks 사용자 ID"
            onInput={(e) => f.set("sources", "naverworks", "user_id", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <Field label="Client ID">
          <TextInput
            class="settings-shortcut"
            mono
            value={nw().client_id}
            aria-label="Client ID"
            onInput={(e) => f.set("sources", "naverworks", "client_id", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <Field label="Client Secret" hint={SECRET_HINT}>
          <SecretInput
            value={nw().client_secret}
            present={!!secrets()?.naverworks_client_secret}
            label="Client Secret"
            secretKey="nw_secret"
            onInput={(v) => f.set("sources", "naverworks", "client_secret", v)}
            onCommit={f.now}
            onBlur={f.secretBlur}
          />
        </Field>
        <Field label="서비스 계정">
          <TextInput
            class="settings-shortcut"
            mono
            value={nw().service_account}
            aria-label="서비스 계정"
            onInput={(e) => f.set("sources", "naverworks", "service_account", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <Field label="Private Key" top hint={`PEM 내용을 붙여넣거나 아래 '키 파일'로 경로 지정 · ${SECRET_HINT}`}>
          <SecretInput
            textarea
            value={nw().private_key}
            present={!!secrets()?.naverworks_private_key}
            label="Private Key"
            secretKey="nw_key"
            onInput={(v) => f.set("sources", "naverworks", "private_key", v)}
            onCommit={f.now}
            onBlur={f.secretBlur}
          />
        </Field>
        <Field label="키 파일" hint="Private Key 를 직접 넣는 대신 .pem / .key 파일 경로">
          <PathInput
            kind="file"
            value={nw().private_key_path}
            label="Private Key 파일"
            placeholder="C:\keys\naverworks.pem"
            onInput={(v) => f.set("sources", "naverworks", "private_key_path", v)}
            onCommit={f.now}
            onPick={(v) => {
              f.set("sources", "naverworks", "private_key_path", v);
              f.now();
            }}
          />
        </Field>
        <Field label="Scope">
          <TextInput
            class="settings-shortcut"
            mono
            value={nw().scope}
            placeholder="calendar.read"
            aria-label="Scope"
            onInput={(e) => f.set("sources", "naverworks", "scope", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <Field label="캘린더" top hint="고른 캘린더의 일정만 회의로 가져옵니다">
          <div class="row">
            <Button size="sm" icon="cal" loading={calBusy()} onClick={() => void loadCals()}>
              캘린더 불러오기
            </Button>
            <Show when={calErr()}>
              <span class="err small settings-kv">{calErr()}</span>
            </Show>
          </div>
          <Show when={shownCals().length > 0} fallback={<div class="muted small">불러온 캘린더가 없습니다</div>}>
            <div class="settings-cals">
              <For each={shownCals()}>
                {(c) => (
                  <label class="settings-checkrow small">
                    <input type="checkbox" checked={nw().calendar_ids.includes(c.calendar_id)} onChange={() => toggleCal(c)} />
                    <span>{c.name}</span>
                    <span class="muted mono">{c.calendar_id}</span>
                  </label>
                )}
              </For>
            </div>
          </Show>
        </Field>
        <CheckButton kind="naverworks" label="연결 확인" cfg={f.snapshot} />
      </section>
    </>
  );
}

// ---- 저장 대상 ------------------------------------------------------------------ //

function OutputsTab(props: { f: Ctx }) {
  const f = props.f;
  const md = () => f.draft.outputs.markdown;
  const ob = () => f.draft.outputs.obsidian;
  const no = () => f.draft.outputs.notion;
  const secrets = () => settings()?.secrets;
  const mdDefault = () => info()?.markdown_dir ?? "";

  const open = async (p: string) => {
    try {
      await api.openPath(p);
    } catch (e) {
      toast(errText(e), "error");
    }
  };

  return (
    <>
      <section class={`settings-sec ${md().enabled ? "" : "is-off"}`}>
        <SecHead
          title="로컬 마크다운"
          checked={md().enabled}
          onToggle={(v) => {
            f.set("outputs", "markdown", "enabled", v);
            f.now();
          }}
          sub="하루 한 파일 · YYYY-MM-DD.md"
        />
        <Field label="폴더" hint={"비우면 문서\\업무일지"}>
          <PathInput
            kind="folder"
            value={md().dir ?? ""}
            label="마크다운 폴더"
            placeholder={mdDefault()}
            onInput={(v) => f.set("outputs", "markdown", "dir", v)}
            onCommit={f.now}
            onPick={(v) => {
              f.set("outputs", "markdown", "dir", v);
              f.now();
            }}
          />
          <div class="row">
            <Button size="sm" icon="folder" onClick={() => void open(md().dir || mdDefault())}>
              폴더 열기
            </Button>
            <CheckButton kind="markdown" label="쓰기 확인" cfg={f.snapshot} />
          </div>
        </Field>
      </section>

      <section class={`settings-sec ${ob().enabled ? "" : "is-off"}`}>
        <SecHead
          title="Obsidian"
          checked={ob().enabled}
          onToggle={(v) => {
            f.set("outputs", "obsidian", "enabled", v);
            f.now();
          }}
          sub="vault 안 폴더에 같은 마크다운을 둔다"
        />
        <Field label="vault 폴더">
          <PathInput
            kind="folder"
            value={ob().vault_dir}
            label="Obsidian vault 폴더"
            placeholder="D:\notes\vault"
            onInput={(v) => f.set("outputs", "obsidian", "vault_dir", v)}
            onCommit={f.now}
            onPick={(v) => {
              f.set("outputs", "obsidian", "vault_dir", v);
              f.now();
            }}
          />
        </Field>
        <Field label="하위 폴더" hint="vault 안에서 일지를 둘 폴더">
          <TextInput
            class="settings-shortcut"
            value={ob().subdir}
            placeholder="업무일지"
            aria-label="Obsidian 하위 폴더"
            onInput={(e) => f.set("outputs", "obsidian", "subdir", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <CheckButton kind="obsidian" label="연결 확인" cfg={f.snapshot} />
      </section>

      <section class={`settings-sec ${no().enabled ? "" : "is-off"}`}>
        <SecHead
          title="Notion"
          checked={no().enabled}
          onToggle={(v) => {
            f.set("outputs", "notion", "enabled", v);
            f.now();
          }}
          sub="페이지 아래 하위 페이지로, 또는 데이터베이스 행으로"
        />
        <Field label="대상">
          <div class="row">
            <Chip
              on={no().parent_type === "page"}
              onClick={() => {
                f.set("outputs", "notion", "parent_type", "page");
                f.now();
              }}
            >
              페이지
            </Chip>
            <Chip
              on={no().parent_type === "database"}
              onClick={() => {
                f.set("outputs", "notion", "parent_type", "database");
                f.now();
              }}
            >
              데이터베이스
            </Chip>
          </div>
        </Field>
        <Field label="ID" hint={no().parent_type === "database" ? "데이터베이스 ID" : "부모 페이지 ID"}>
          <TextInput
            class="settings-shortcut"
            mono
            value={no().parent_id}
            aria-label="Notion 대상 ID"
            onInput={(e) => f.set("outputs", "notion", "parent_id", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <Show when={no().parent_type === "database"}>
          <Field label="제목 속성" hint="데이터베이스의 제목(title) 속성 이름">
            <TextInput
              class="settings-short"
              value={no().title_prop}
              placeholder="Name"
              aria-label="Notion 제목 속성"
              onInput={(e) => f.set("outputs", "notion", "title_prop", e.currentTarget.value)}
              onChange={f.now}
            />
          </Field>
        </Show>
        <Field label="토큰" hint={`통합(Integration) 시크릿 · ${SECRET_HINT}`}>
          <SecretInput
            value={no().token}
            present={!!secrets()?.notion_token}
            label="Notion 토큰"
            secretKey="notion_token"
            onInput={(v) => f.set("outputs", "notion", "token", v)}
            onCommit={f.now}
            onBlur={f.secretBlur}
          />
        </Field>
        <Field label="API 버전">
          <TextInput
            class="settings-short"
            mono
            value={no().version}
            placeholder="2022-06-28"
            aria-label="Notion API 버전"
            onInput={(e) => f.set("outputs", "notion", "version", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <CheckButton kind="notion" label="연결 확인" cfg={f.snapshot} />
      </section>
    </>
  );
}

// ---- AI 요약 -------------------------------------------------------------------- //

const PROVIDERS = [
  { value: "auto", label: "자동 (claude CLI 있으면 사용)" },
  { value: "claude_cli", label: "claude CLI" },
  { value: "anthropic_api", label: "Anthropic API (ANTHROPIC_API_KEY)" },
  { value: "none", label: "사용 안 함 (데이터만 정리)" },
];

function SummaryTab(props: { f: Ctx }) {
  const f = props.f;
  const s = () => f.draft.summarizer;
  const cli = () => settings()?.claude_cli ?? null;
  const usesCli = () => s().provider === "auto" || s().provider === "claude_cli";
  const providerOpts = () => (PROVIDERS.some((p) => p.value === s().provider) ? PROVIDERS : [...PROVIDERS, { value: s().provider, label: s().provider }]);

  // 템플릿 목록은 한 번만 읽어 둔다(일지 화면의 '다시 생성' 메뉴와 같은 자료).
  const [tpls] = createResource(loadTemplates);
  const tplList = () => tpls() ?? TEMPLATE_FALLBACK;
  const tplOpts = () => tplList().map((t) => ({ value: t.id, label: t.name }));
  const tplDesc = () => {
    const t = tplList().find((x) => x.id === s().template);
    if (!t) return "";
    return t.sections.length ? `${t.description} — ${t.sections.join(" · ")}` : t.description;
  };

  // 시간대는 올바른 IANA 이름일 때만 초안에 넣고 저장한다 — 잘못된 글자는 칸에만 남기고 아래에 알린다.
  const [tzText, setTzText] = createSignal(f.draft.timezone);
  const [tzErr, setTzErr] = createSignal<string | null>(null);
  createEffect(
    on(
      () => f.draft.timezone,
      (v) => {
        setTzText(v);
        setTzErr(null);
      },
      { defer: true },
    ),
  );
  const commitTz = () => {
    const raw = tzText().trim();
    const tz = raw ? canonicalTimeZone(raw) : "";
    if (tz == null) {
      setTzErr(`알 수 없는 시간대 · '${raw}' · 예: Asia/Seoul`);
      return;
    }
    setTzErr(null);
    setTzText(tz);
    f.set("timezone", tz);
    f.now();
  };

  return (
    <>
      <section class="settings-sec">
        <SecHead title="요약 엔진" />
        <Field label="요약 방식" top>
          <Labeled label="요약 방식">
            <Select
              value={s().provider}
              options={providerOpts()}
              onChange={(v) => {
                f.set("summarizer", "provider", v);
                f.now();
              }}
            />
          </Labeled>
          <Show when={usesCli()}>
            <div class="row settings-status">
              <Show
                when={cli()}
                fallback={
                  <span class="warn small row">
                    <Icon name="alert" size={14} />
                    claude CLI 를 찾지 못했습니다
                  </span>
                }
              >
                {(p) => (
                  <span class="ok small row">
                    <Icon name="check" size={14} />
                    claude CLI 찾음 · <span class="mono settings-kv">{p()}</span>
                  </span>
                )}
              </Show>
              <CheckButton kind="claude_cli" label="다시 확인" icon="refresh" cfg={f.snapshot} onDone={() => void loadSettings()} />
            </div>
          </Show>
          <Show when={s().provider === "none"}>
            <div class="muted small">AI 를 부르지 않고 수집한 데이터만 정리해 일지를 만듭니다</div>
          </Show>
        </Field>
        <Field label="모델" hint="비우면 CLI · API 의 기본 모델">
          <TextInput
            class="settings-shortcut"
            mono
            value={s().model}
            placeholder="기본 모델"
            aria-label="모델"
            onInput={(e) => f.set("summarizer", "model", e.currentTarget.value)}
            onChange={f.now}
          />
        </Field>
        <Field label="최대 토큰" hint="요약 응답 상한">
          <NumInput
            value={s().max_tokens}
            min={256}
            max={32000}
            step={256}
            label="최대 토큰"
            onChange={(n) => {
              f.set("summarizer", "max_tokens", n);
              f.now();
            }}
          />
        </Field>
        <Field label="긴 날 처리" top hint="세션 질답 총량이 이 글자수를 넘으면 세션별로 먼저 압축한 뒤 종합 · 병렬은 동시에 압축할 세션 수">
          <div class="row">
            <NumInput
              value={s().map_reduce_chars}
              min={2000}
              max={200000}
              step={1000}
              label="세션별 압축 기준 글자수"
              onChange={(n) => {
                f.set("summarizer", "map_reduce_chars", n);
                f.now();
              }}
            />
            <span class="muted small">자 이상이면 세션별 압축</span>
            <NumInput
              value={s().map_workers}
              min={1}
              max={16}
              label="동시 압축 세션 수"
              onChange={(n) => {
                f.set("summarizer", "map_workers", n);
                f.now();
              }}
            />
            <span class="muted small">병렬</span>
          </div>
        </Field>
      </section>

      <section class="settings-sec">
        <SecHead title="문서" />
        <Field label="일지 템플릿" top hint="일지 화면의 '다시 생성' 옆 메뉴로 이번 한 번만 다른 템플릿을 쓸 수도 있습니다">
          <PickChips
            value={s().template}
            options={tplOpts()}
            onPick={(v) => {
              f.set("summarizer", "template", v);
              f.now();
            }}
          />
          <Show when={tplDesc()}>{(d) => <div class="muted small">{d()}</div>}</Show>
        </Field>
        <Field label="시간대" hint="예: Asia/Seoul · 피드와 일지의 하루 경계">
          <TextInput
            class="settings-shortcut"
            mono
            value={tzText()}
            placeholder="Asia/Seoul"
            aria-label="시간대"
            aria-invalid={tzErr() ? "true" : undefined}
            onInput={(e) => setTzText(e.currentTarget.value)}
            onChange={commitTz}
            onKeyDown={(e) => {
              if (!isEnter(e)) return;
              e.preventDefault();
              commitTz();
            }}
          />
          <Show when={tzErr()}>{(m) => <div class="err small">{m()}</div>}</Show>
        </Field>
        <Toggle
          checked={f.draft.include_raw_data}
          onChange={(v) => {
            f.set("include_raw_data", v);
            f.now();
          }}
          label="문서 끝에 수집 원본 부록 포함"
          hint="세션 · 커밋 · 회의 원본 목록을 그대로 덧붙임"
        />
      </section>
    </>
  );
}

// ---- 정보 ---------------------------------------------------------------------- //

type UpdState = { kind: "idle" } | { kind: "busy" } | { kind: "found"; u: UpdateInfo } | { kind: "err"; text: string };

function AboutTab() {
  const i = () => info();
  const settingsPath = () => i()?.settings_path ?? settings()?.path ?? "";
  const parentDir = (p: string) => p.replace(/[\\/][^\\/]*$/, "") || p;

  const initial = (): UpdState => {
    const u = update();
    return u ? { kind: "found", u } : { kind: "idle" };
  };
  const [upd, setUpd] = createSignal<UpdState>(initial());
  const [installing, setInstalling] = createSignal(false);

  const open = async (p: string) => {
    try {
      await api.openPath(p);
    } catch (e) {
      toast(errText(e), "error");
    }
  };
  const check = async () => {
    setUpd({ kind: "busy" });
    try {
      const u = await api.updateCheck();
      if (u) setUpd({ kind: "found", u });
      else {
        setUpd({ kind: "idle" });
        toast("최신 버전입니다", "ok");
      }
    } catch (e) {
      setUpd({ kind: "err", text: errText(e) });
    }
  };
  const install = async () => {
    setInstalling(true);
    try {
      toast("업데이트를 내려받는 중… 설치가 시작되면 앱이 다시 실행됩니다.", "info", { ms: 0 });
      await api.updateInstall();
    } catch (e) {
      toast(errText(e), "error");
    } finally {
      setInstalling(false);
    }
  };
  const quit = async () => {
    if (!window.confirm("앱을 종료할까요? 종료하면 실시간 수집과 정해진 시각 알림도 멈춥니다.")) return;
    try {
      await api.appQuit();
    } catch (e) {
      toast(errText(e), "error");
    }
  };

  return (
    <>
      <section class="settings-sec">
        <SecHead title="앱" />
        <Field label="버전">
          <span class="settings-kv">v{i()?.version ?? "?"}</span>
        </Field>
        <Field label="설정 파일">
          <div class="row">
            <span class="mono settings-kv">{settingsPath()}</span>
            <Button size="sm" icon="folder" onClick={() => void open(parentDir(settingsPath()))}>
              폴더 열기
            </Button>
          </div>
        </Field>
        <Field label="데이터">
          <div class="row">
            <span class="mono settings-kv">{i()?.db_path ?? ""}</span>
            <Show when={i()?.db_path}>
              <Button size="sm" icon="folder" onClick={() => void open(parentDir(i()!.db_path))}>
                폴더 열기
              </Button>
            </Show>
          </div>
          <Show when={i() && !i()!.store_ok}>
            <div class="err small settings-kv">
              <Icon name="alert" size={13} /> 데이터 저장소를 열지 못했습니다{i()!.store_error ? ` · ${i()!.store_error}` : ""}
            </div>
          </Show>
        </Field>
        <Field label="일지 폴더">
          <div class="row">
            <span class="mono settings-kv">{i()?.markdown_dir ?? ""}</span>
            <Show when={i()?.markdown_dir}>
              <Button size="sm" icon="folder" onClick={() => void open(i()!.markdown_dir)}>
                열기
              </Button>
            </Show>
          </div>
        </Field>
      </section>

      <section class="settings-sec">
        <SecHead title="업데이트" />
        <div class="row">
          <Button icon="refresh" loading={upd().kind === "busy"} onClick={() => void check()}>
            업데이트 확인
          </Button>
          <Show when={upd().kind === "err"}>
            <span class="err small settings-kv">{(upd() as { kind: "err"; text: string }).text}</span>
          </Show>
        </div>
        <Show when={upd().kind === "found" ? (upd() as { kind: "found"; u: UpdateInfo }).u : null}>
          {(u) => (
            <div class="box settings-update-box">
              <div class="row">
                <Icon name="bolt" size={16} />
                <b>새 버전 {u().version}</b>
                <span class="muted small">현재 {u().current}{u().date ? ` · ${u().date}` : ""}</span>
              </div>
              <Show when={u().notes}>
                <div class="small muted settings-update-notes">{u().notes}</div>
              </Show>
              <div>
                <Button variant="primary" loading={installing()} onClick={() => void install()}>
                  지금 설치
                </Button>
              </div>
            </div>
          )}
        </Show>
      </section>

      <section class="settings-sec">
        <SecHead title="종료" />
        <div class="row">
          <Button variant="danger" onClick={() => void quit()}>
            앱 종료
          </Button>
          <span class="muted small">창을 닫아도 트레이에서 계속 수집합니다 — 완전히 멈추려면 여기서 종료</span>
        </div>
      </section>
    </>
  );
}

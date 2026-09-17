// 오늘 화면 — 실시간 피드(자동 수집 + 메모 한 줄기) · 지표 · 생성 진행 · 메모 입력. 와이어프레임 A(피드형).
import { For, Show, createEffect, createMemo, createSignal, on, onCleanup, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import "./today.css";
import { Button, Chip, Empty, Icon, Pill, Spinner } from "../components/ui";
import { api, type Config, type FeedItem, type GenStatus, type SourceStatus } from "../ipc";
import {
  cancelGenerate,
  errText,
  feed,
  genProgressRatio,
  generating,
  gotoJournal,
  gotoSettings,
  lastDone,
  refreshNow,
  refreshing,
  settings,
  startGenerate,
  toast,
} from "../store";
import { daypart, fmtDateLong, fmtTime, todayStr } from "../util";

const KIND_LABEL: Record<string, string> = { session: "세션", commit: "커밋", meeting: "회의", note: "메모" };
const SOURCE_LABEL: Record<string, string> = { git: "git", claude: "Claude", codex: "Codex", naverworks: "회의" };
const STATE_LABEL: Record<string, string> = { skipped: "건너뜀", error: "오류" };
const AGENT_LABEL: Record<string, string> = { claude: "Claude", codex: "Codex" };
const STEPS = ["수집", "요약", "저장"] as const;
const TAGS = ["요청", "결정", "할일", "회의"];
/** 저장 단계 라벨에 쓰는 출력 이름(설정 순서대로). */
const OUTPUT_LABEL: [keyof Config["outputs"], string][] = [
  ["markdown", "로컬 md"],
  ["obsidian", "Obsidian"],
  ["notion", "Notion"],
];

/** 현재 로컬 시각 "HH:MM". */
function nowHHMM(): string {
  return fmtTime(new Date().toISOString());
}

/** "HH:MM" 꼴인지. 종일 회의("종일")·시각 없는 항목("--:--")은 아니다. */
function isHHMM(t: string): boolean {
  return /^\d{2}:\d{2}$/.test(t);
}

/** 구분선 라벨. 시각이 없는 항목은 "종일"(맨 앞 묶음). daypart 는 HH:MM 일 때만 부른다. */
function partOf(it: FeedItem): string {
  return isHHMM(it.time) ? daypart(it.time) : "종일";
}

/** 텍스트 영역 높이를 내용에 맞춘다(최대 높이는 CSS max-height 가 막는다). */
function autoGrow(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${el.scrollHeight + 2}px`;
}

/** 한글 IME 조합 중의 Enter 는 무시한다(조합 확정용 Enter). */
function isComposingEnter(e: KeyboardEvent): boolean {
  return e.isComposing || e.keyCode === 229;
}

// ---- 화면 -------------------------------------------------------------------- //

export default function Today() {
  const date = () => feed()?.date ?? todayStr();
  const realtime = () => settings()?.config.automation.realtime.enabled ?? false;
  const shortcut = () => settings()?.config.automation.global_shortcut ?? "";

  // 시각 동작(알림 / 자동 생성) 상태 필
  const sched = () => {
    const s = settings()?.config.automation.schedule;
    if (!s || !s.enabled) return { text: "시각 동작 꺼짐", on: false };
    return { text: s.mode === "generate" ? `${s.time} 자동 생성` : `${s.time} 일지 알림 켜짐`, on: true };
  };

  // 메모 입력창 내용 — 생성 완료 후 자동 이동 여부 판단에 쓰므로 화면 수준에 둔다.
  const [composerText, setComposerText] = createSignal("");

  // 이 화면에서 사용자가 직접 시작한 생성(run_id)만 끝나면 '일지'로 자동 이동한다.
  // 정해진 시각 자동 생성·다른 창에서 시작한 실행은 토스트로만 알린다(읽는 도중 화면이 바뀌지 않게).
  const startedHere = new Set<number>();
  const generateHere = async () => {
    const id = await startGenerate();
    if (id != null) startedHere.add(id);
  };

  // 생성 완료: 여기서 시작한 오늘 일지이고 입력 중인 메모가 없으면 '일지'로 이동, 아니면 10초 안내 띠.
  const [doneLine, setDoneLine] = createSignal<{ time: string; date: string } | null>(null);
  let doneTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(
    on(
      lastDone,
      (d) => {
        if (!d || d.status !== "ok" || d.date !== date()) return;
        if (startedHere.has(d.run_id) && composerText().trim().length === 0) {
          startedHere.delete(d.run_id);
          gotoJournal(d.date);
          return;
        }
        setDoneLine({ time: nowHHMM(), date: d.date });
        if (doneTimer) clearTimeout(doneTimer);
        doneTimer = setTimeout(() => setDoneLine(null), 10_000);
      },
      { defer: true },
    ),
  );
  onCleanup(() => doneTimer && clearTimeout(doneTimer));

  // "지금 HH:MM" 구분선용 시계
  const [now, setNow] = createSignal(nowHHMM());
  const clock = setInterval(() => setNow(nowHHMM()), 30_000);
  onCleanup(() => clearInterval(clock));

  // 필터 · 경고
  const [onlyMemo, setOnlyMemo] = createSignal(false);
  const [showWarn, setShowWarn] = createSignal(false);
  const warnings = () => feed()?.warnings ?? [];

  const badStatuses = createMemo<SourceStatus[]>(() =>
    (feed()?.statuses ?? []).filter((s) => s.state === "skipped" || s.state === "error"),
  );

  // 메모 인라인 수정 상태 — feed:changed 마다 행이 새로 그려져도 잃지 않도록 화면 수준에 둔다.
  const [editId, setEditId] = createSignal<number | null>(null);
  const [draft, setDraft] = createSignal("");
  const [editBusy, setEditBusy] = createSignal(false);

  // 피드 항목은 id 로 맞춰 넣어(reconcile) 바뀌지 않은 행의 객체를 그대로 쓴다 → <For> 가 행을 재사용한다.
  const [items, setItems] = createStore<{ list: FeedItem[] }>({ list: [] });
  createEffect(() => {
    const list = feed()?.items ?? [];
    setItems("list", reconcile(list, { key: "id" }));
    // 수정 중이던 메모가 사라졌으면(다른 창에서 삭제 등) 수정 상태를 접는다.
    const id = untrack(editId);
    if (id != null && !list.some((i) => i.note_id === id)) setEditId(null);
  });

  // 백엔드 순서(has_start, start, kind)를 그대로 쓴다. 시각 없는 항목(종일 회의 등)만 맨 앞 묶음으로.
  const visibleItems = createMemo<FeedItem[]>(() => {
    const list = items.list;
    const filtered = onlyMemo() ? list.filter((i) => i.kind === "note") : list.slice();
    const untimed = filtered.filter((i) => !isHHMM(i.time));
    if (untimed.length === 0) return filtered;
    return [...untimed, ...filtered.filter((i) => isHHMM(i.time))];
  });

  const beginEdit = (it: FeedItem) => {
    if (it.note_id == null) return;
    setDraft(it.label);
    setEditId(it.note_id);
  };
  const cancelEdit = () => setEditId(null);
  const saveEdit = async (it: FeedItem) => {
    const id = it.note_id;
    if (id == null || editBusy()) return;
    const text = draft().trim();
    if (!text || text === it.label) {
      setEditId(null);
      return;
    }
    setEditBusy(true);
    try {
      await api.noteEdit(id, text);
      setEditId(null);
    } catch (e) {
      toast(errText(e), "error");
    } finally {
      setEditBusy(false);
    }
  };
  const removeNote = async (it: FeedItem) => {
    const id = it.note_id;
    if (id == null) return;
    if (!window.confirm("이 메모를 삭제할까요?")) return;
    try {
      await api.noteDelete(id);
      if (editId() === id) setEditId(null);
    } catch (e) {
      toast(errText(e), "error");
    }
  };

  // 자동 스크롤: 바닥 근처에 있었으면 새 항목이 와도 바닥을 유지. 처음엔 최신(바닥)으로.
  let bodyEl!: HTMLDivElement;
  let stick = true;
  const onScroll = () => {
    stick = bodyEl.scrollHeight - bodyEl.scrollTop - bodyEl.clientHeight < 48;
  };
  const scrollToBottom = () => {
    requestAnimationFrame(() => {
      if (bodyEl) bodyEl.scrollTop = bodyEl.scrollHeight;
    });
  };
  // 항목이 바뀌거나 생성 카드가 나타나/사라져 피드 높이가 바뀔 때 바닥을 유지한다.
  createEffect(
    on([visibleItems, () => generating() !== null], () => {
      if (stick) scrollToBottom();
    }),
  );
  const setFilter = (memoOnly: boolean) => {
    stick = true;
    setOnlyMemo(memoOnly);
  };

  const liveText = () => {
    const base = realtime() ? "실시간 감시 중" : "실시간 감시 꺼짐";
    const last = feed()?.last_event_at;
    const t = last ? fmtTime(last) : "";
    return t ? `${base} · 마지막 이벤트 ${t}` : base;
  };

  const emptyHint = () =>
    onlyMemo()
      ? "아래 입력창에 구두 요청 · 결정 · 할 일을 적어두세요"
      : realtime()
        ? "실시간 감시가 켜져 있어요 — 세션·커밋은 저절로 나타납니다. '지금 갱신'으로 바로 확인할 수도 있어요"
        : "실시간 감시가 꺼져 있어요 — 설정 · 자동화에서 켜거나 '지금 갱신'을 눌러 수집하세요";

  return (
    <section class="screen today">
      <header class="screen-header">
        <span class="screen-title">오늘</span>
        <span class="today-header-date">{fmtDateLong(date())}</span>
        <span class="grow" />
        <Show when={settings()}>
          <button type="button" class="today-sched" onClick={() => gotoSettings("automation")} title="설정 · 자동화">
            <Pill tone={sched().on ? "default" : "muted"}>
              <Show when={sched().on}>
                <span class="dot dot-live" />
              </Show>
              <span class="today-sched-text">{sched().text}</span>
            </Pill>
          </button>
        </Show>
        <Show when={generating()} fallback={<Button variant="primary" icon="bolt" onClick={() => void generateHere()}>지금 일지 만들기</Button>}>
          <Button loading disabled>
            생성 중…
          </Button>
        </Show>
      </header>

      <Show when={doneLine()}>
        {(d) => (
          <div class="today-done small" role="status">
            <Icon name="check" size={14} />
            <span>{d().time} 일지 생성됨 ·</span>
            <button type="button" class="today-link" onClick={() => gotoJournal(d().date)}>
              일지 보기
            </button>
          </div>
        )}
      </Show>

      <div class="today-kpis">
        <span class="kpi">
          <b>{feed()?.kpis.commits ?? 0}</b>
          <span class="small muted">커밋</span>
        </span>
        <span class="kpi">
          <b>{feed()?.kpis.sessions ?? 0}</b>
          <span class="small muted">AI 세션</span>
        </span>
        <span class="kpi">
          <b>{feed()?.kpis.meetings ?? 0}</b>
          <span class="small muted">회의</span>
        </span>
        <span class="kpi kpi-memo">
          <b>{feed()?.kpis.notes ?? 0}</b>
          <span class="small">메모</span>
        </span>
        <For each={badStatuses()}>
          {(s) => (
            <Chip dashed title={s.note ?? undefined}>
              {SOURCE_LABEL[s.name] ?? s.name} {STATE_LABEL[s.state] ?? s.state}
            </Chip>
          )}
        </For>
        <span class="grow" />
        <span class="today-live small">
          <span class={`dot ${realtime() ? "dot-live" : "dot-idle"}`} />
          <span class="today-live-text">{liveText()}</span>
        </span>
        <Button icon="refresh" loading={refreshing()} onClick={() => void refreshNow()}>
          지금 갱신
        </Button>
      </div>

      <Show when={generating()}>
        {(g) => (
          <div class="today-genwrap">
            <GenCard g={g()} />
          </div>
        )}
      </Show>

      <div class="today-feedbar">
        <div class="today-feedbar-row">
          <button type="button" class={`chip chip-click ${onlyMemo() ? "" : "chip-on"}`} aria-pressed={!onlyMemo()} onClick={() => setFilter(false)}>
            전체
          </button>
          <button type="button" class={`chip chip-click ${onlyMemo() ? "chip-on" : ""}`} aria-pressed={onlyMemo()} onClick={() => setFilter(true)}>
            메모만
          </button>
          <span class="grow" />
          <Show when={warnings().length > 0}>
            <button type="button" class="today-warn-btn" aria-expanded={showWarn()} onClick={() => setShowWarn((v) => !v)}>
              <Icon name="alert" size={13} />
              경고 {warnings().length}건
              <Icon name={showWarn() ? "down" : "right"} size={12} />
            </button>
          </Show>
        </div>
        <Show when={showWarn() && warnings().length > 0}>
          <ul class="today-warn-list">
            <For each={warnings()}>{(w) => <li>{w}</li>}</For>
          </ul>
        </Show>
      </div>

      <div class="scroll grow today-body" ref={bodyEl} onScroll={onScroll}>
        <Show when={feed()} fallback={<div class="today-loading"><Spinner size={20} /></div>}>
          <div class={`today-feed ${generating() ? "is-dim" : ""}`}>
            <Show
              when={visibleItems().length > 0}
              fallback={<Empty icon={onlyMemo() ? "note" : "today"} title={onlyMemo() ? "오늘 적은 메모가 없어요" : "아직 수집된 활동이 없어요"} hint={emptyHint()} />}
            >
              {/* 행 단위 <For>(항목 객체가 안정적이라 재사용됨). 구분선은 앞 항목과 시간대가 다를 때 행 앞에 붙인다. */}
              <For each={visibleItems()}>
                {(it, i) => {
                  const part = () => partOf(it);
                  const sep = () => {
                    const idx = i();
                    if (idx === 0) return true;
                    const prev = visibleItems()[idx - 1];
                    return !prev || partOf(prev) !== part();
                  };
                  return (
                    <>
                      <Show when={sep()}>
                        <div class="today-sep">
                          <span>{part()}</span>
                          <span class="today-sep-line" />
                        </div>
                      </Show>
                      <Show when={it.kind === "note"} fallback={<EventRow item={it} />}>
                        <MemoRow
                          item={it}
                          editing={editId() != null && editId() === it.note_id}
                          draft={draft()}
                          busy={editBusy()}
                          onDraft={setDraft}
                          onBegin={() => beginEdit(it)}
                          onSave={() => void saveEdit(it)}
                          onCancel={cancelEdit}
                          onRemove={() => void removeNote(it)}
                        />
                      </Show>
                    </>
                  );
                }}
              </For>
              <div class="today-sep">
                <span class="today-sep-line" />
                <span>지금 {now()}</span>
                <span class="today-sep-line" />
              </div>
            </Show>
          </div>
        </Show>
      </div>

      <Composer shortcut={shortcut()} text={composerText()} onText={setComposerText} />
    </section>
  );
}

// ---- 생성 카드 ------------------------------------------------------------------ //

function GenCard(props: { g: GenStatus }) {
  const cancelling = () => props.g.step === "취소 중";
  const idx = () => (STEPS as readonly string[]).indexOf(props.g.step);
  const state = (i: number): "done" | "now" | "todo" => {
    const cur = idx();
    if (cur < 0) return "todo";
    return i < cur ? "done" : i === cur ? "now" : "todo";
  };
  const pct = () => Math.round(genProgressRatio(props.g) * 100);

  // 저장 단계는 켜진 출력 목록을 같이 보여준다: "저장 · 로컬 md, Obsidian, Notion". 하나도 없으면 "저장".
  const saveLabel = () => {
    const o = settings()?.config.outputs;
    if (!o) return "저장";
    const on = OUTPUT_LABEL.filter(([k]) => o[k].enabled).map(([, l]) => l);
    return on.length > 0 ? `저장 · ${on.join(", ")}` : "저장";
  };
  const label = (i: number) => (STEPS[i] === "저장" ? saveLabel() : STEPS[i]);

  return (
    <div class={`box today-gen ${cancelling() ? "is-cancel" : ""}`} role="status" aria-live="polite">
      <div class="today-gen-head">
        <Spinner size={18} />
        <b class="today-gen-title">일지 생성 중</b>
        <span class="small muted">{fmtTime(props.g.started)} 시작 · 보통 2–3분</span>
        <span class="grow" />
        <Button size="sm" disabled={cancelling()} onClick={() => void cancelGenerate()}>
          {cancelling() ? "취소 중…" : "취소"}
        </Button>
      </div>
      <div class="today-gen-steps">
        <For each={STEPS}>
          {(_s, i) => (
            <>
              <Show when={i() > 0}>
                <span class="today-gen-link" aria-hidden="true" />
              </Show>
              <span class={`today-step is-${state(i())}`}>
                <Show when={state(i()) === "done"}>
                  <Icon name="check" size={13} />
                </Show>
                {label(i())}
                {state(i()) === "now" && props.g.detail ? ` · ${props.g.detail}` : ""}
              </span>
            </>
          )}
        </For>
        <Show when={!cancelling() && idx() < 0}>
          <span class="small muted">준비 중…</span>
        </Show>
      </div>
      <div class="today-prog" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct()} aria-label="생성 진행률">
        <div class="today-prog-fill" style={{ width: `${pct()}%` }} />
      </div>
      <div class="small muted">창을 닫아도 트레이에서 계속됩니다 · 끝나면 알림 후 '일지' 화면으로 이동</div>
    </div>
  );
}

// ---- 피드 항목 ------------------------------------------------------------------ //

function eventDetail(it: FeedItem): string {
  const parts: string[] = [];
  if (it.kind === "session") {
    parts.push(it.active ? `${it.time}– · 진행 중 (마지막 활동 ${it.end_time ?? it.time})` : `${it.time}–${it.end_time ?? ""}`);
    parts.push(`${it.files}파일`);
    if (it.agent) parts.push(AGENT_LABEL[it.agent] ?? it.agent);
    if (it.detail) parts.push(it.detail);
  } else if (it.kind === "commit") {
    parts.push(`+${it.insertions} / −${it.deletions}`);
    if (it.files > 0) parts.push(`${it.files}파일`);
    if (it.detail) parts.push(it.detail);
  } else {
    // 회의: files 에 참석자 수가 들어온다.
    parts.push(it.end_time ? `${it.time}–${it.end_time}` : it.time);
    if (it.files > 0) parts.push(`참석 ${it.files}명`);
    if (it.detail) parts.push(it.detail);
  }
  return parts.filter(Boolean).join(" · ");
}

function EventRow(props: { item: FeedItem }) {
  return (
    <div class="today-row">
      <span class="today-tm">{props.item.time}</span>
      <div class={`today-ev ${props.item.active ? "is-active" : ""}`}>
        <Icon name={props.item.kind} size={18} />
        <div class="today-ev-body">
          <div class="today-ev-title">
            <b>{KIND_LABEL[props.item.kind] ?? props.item.kind}</b>
            <Show when={props.item.project}>{(p) => <> · <b>{p()}</b></>}</Show>
            {" — "}
            {props.item.label}
          </div>
          <div class="today-ev-detail small muted">
            <Show when={props.item.active}>
              <span class="dot dot-live today-ev-live" aria-hidden="true" />
            </Show>
            <span>{eventDetail(props.item)}</span>
          </div>
        </div>
      </div>
    </div>
  );
}

/** 메모 행. 수정 상태(editing/draft/busy)는 부모(Today)가 들고 있어 피드가 갱신돼도 유지된다. */
function MemoRow(props: {
  item: FeedItem;
  editing: boolean;
  draft: string;
  busy: boolean;
  onDraft: (v: string) => void;
  onBegin: () => void;
  onSave: () => void;
  onCancel: () => void;
  onRemove: () => void;
}) {
  const noteId = () => props.item.note_id;
  let editEl: HTMLTextAreaElement | undefined;

  // 수정 모드로 들어오면(또는 행이 다시 그려지면) 텍스트 영역에 포커스를 돌려준다.
  createEffect(() => {
    if (!props.editing) return;
    const el = editEl;
    if (!el) return;
    requestAnimationFrame(() => {
      autoGrow(el);
      if (document.activeElement !== el) {
        el.focus();
        el.setSelectionRange(el.value.length, el.value.length);
      }
    });
  });

  return (
    <div class="today-row today-row-memo">
      <div class={`today-memo ${props.editing ? "is-editing" : ""}`}>
        <Show when={props.editing} fallback={<div class="today-memo-text">{props.item.label}</div>}>
          <textarea
            class="textarea today-memo-edit"
            aria-label="메모 수정"
            rows={1}
            value={props.draft}
            disabled={props.busy}
            ref={(el) => (editEl = el)}
            onInput={(e) => {
              props.onDraft(e.currentTarget.value);
              autoGrow(e.currentTarget);
            }}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                props.onCancel();
              } else if (e.key === "Enter" && !e.shiftKey) {
                if (isComposingEnter(e)) return;
                e.preventDefault();
                props.onSave();
              }
            }}
          />
        </Show>
        <div class="today-memo-foot">
          <For each={props.item.tags}>{(t) => <Chip memo>#{t}</Chip>}</For>
          <For each={props.item.mentions}>{(m) => <Chip memo>@{m}</Chip>}</For>
          <span class="grow" />
          <Show
            when={props.editing}
            fallback={
              <Show when={noteId() != null}>
                <Button variant="ghost" size="sm" onClick={props.onBegin} aria-label="메모 수정">
                  수정
                </Button>
                <Button variant="ghost" size="sm" onClick={props.onRemove} aria-label="메모 삭제">
                  삭제
                </Button>
              </Show>
            }
          >
            <span class="today-memo-hint">Enter 저장 · Esc 취소</span>
            <Button variant="ghost" size="sm" onClick={props.onCancel} disabled={props.busy}>
              취소
            </Button>
            <Button size="sm" onClick={props.onSave} loading={props.busy}>
              저장
            </Button>
          </Show>
        </div>
      </div>
      <span class="today-tm today-tm-r">{props.item.time}</span>
    </div>
  );
}

// ---- 메모 입력 ------------------------------------------------------------------ //

/** 메모 입력. 내용(text)은 부모가 들고 있다(생성 완료 후 자동 이동 판단에 씀). */
function Composer(props: { shortcut: string; text: string; onText: (v: string) => void }) {
  const [saving, setSaving] = createSignal(false);
  let inputEl!: HTMLTextAreaElement;

  const canSend = () => props.text.trim().length > 0 && !saving();

  const setValue = (v: string) => {
    props.onText(v);
    requestAnimationFrame(() => autoGrow(inputEl));
  };

  const addTag = (t: string) => {
    const tag = `#${t}`;
    const cur = props.text;
    if (!cur.includes(tag)) setValue((cur && !/\s$/.test(cur) ? `${cur} ` : cur) + `${tag} `);
    inputEl.focus();
  };

  const save = async () => {
    if (!canSend()) return;
    const t = props.text.trim();
    setSaving(true);
    try {
      await api.noteAdd(t);
      setValue("");
    } catch (e) {
      toast(errText(e), "error");
    } finally {
      setSaving(false);
      inputEl.focus();
    }
  };

  return (
    <div class="today-composer">
      <div class="today-composer-tags">
        <For each={TAGS}>{(t) => <Chip onClick={() => addTag(t)}>#{t}</Chip>}</For>
        <span class="small muted">태그를 누르면 입력에 붙습니다 · @이름 으로 요청자 표시</span>
      </div>
      <div class="today-composer-row">
        <textarea
          class="textarea today-input"
          ref={inputEl}
          rows={1}
          placeholder="구두 요청 · 결정 · 할 일을 적어두세요"
          aria-label="메모 입력"
          value={props.text}
          disabled={saving()}
          onInput={(e) => {
            props.onText(e.currentTarget.value);
            autoGrow(e.currentTarget);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              if (isComposingEnter(e)) return;
              e.preventDefault();
              void save();
            }
          }}
        />
        <Button variant="primary" icon="send" class="today-send" aria-label="메모 저장" disabled={!canSend()} loading={saving()} onClick={() => void save()} />
      </div>
      <div class="today-composer-hint small muted">
        <Show
          when={generating()}
          fallback={
            <span>
              Enter 저장 · Shift+Enter 줄바꿈 · 트레이 메뉴 '빠른 메모'로 어디서든
              {props.shortcut ? ` (단축키 ${props.shortcut})` : ""}
            </span>
          }
        >
          <span class="today-ydot" aria-hidden="true" />
          <span>생성 중에도 메모할 수 있어요 — 지금 추가한 메모는 다음 생성에 반영됩니다</span>
        </Show>
      </div>
    </div>
  );
}

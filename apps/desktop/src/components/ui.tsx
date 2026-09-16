// 공용 UI 조각 — 아이콘 · 버튼 · 칩 · 토글 · 셀렉트 · 필드 · 토스트. 스타일은 styles.css 의 클래스.
import { For, Show, splitProps, type JSX, type ParentProps } from "solid-js";
import { dismissToast, toasts } from "../store";

// ---- 아이콘 ------------------------------------------------------------------- //

const PATHS: Record<string, string> = {
  today: "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4",
  journal: "M4 5a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2v16H6a2 2 0 0 1-2-2zM8 3v18M12 8h4M12 12h4",
  settings: "M12 8.5a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7zM19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z",
  bolt: "M13 2L4 14h7l-1 8 9-12h-7z",
  refresh: "M20 12a8 8 0 1 1-2.3-5.7M20 4v5h-5",
  send: "M4 12l16-8-6 16-2-6z",
  session: "M3 4h18v16H3zM7 9l3 3-3 3M13 15h4",
  meeting: "M3 5h18v16H3zM3 10h18M8 3v4M16 3v4",
  commit: "M12 8.5a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7zM2 12h6.5M15.5 12H22",
  note: "M4 20l4-1 11-11-3-3L5 16zM13 7l3 3",
  check: "M5 12l5 5L20 7",
  x: "M6 6l12 12M18 6L6 18",
  edit: "M4 20l4-1 11-11-3-3L5 16zM13 7l3 3",
  copy: "M9 9h11v11H9zM5 15V5a2 2 0 0 1 2-2h10",
  folder: "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
  left: "M15 6l-6 6 6 6",
  right: "M9 6l6 6-6 6",
  down: "M6 9l6 6 6-6",
  search: "M11 5a6 6 0 1 0 0 12 6 6 0 0 0 0-12zM20 20l-4.5-4.5",
  alert: "M12 3l10 18H2zM12 10v5M12 18v.5",
  trash: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3",
  plus: "M12 5v14M5 12h14",
  external: "M14 4h6v6M20 4l-9 9M19 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1h5",
  bell: "M6 16V11a6 6 0 1 1 12 0v5l2 2H4zM10 20a2 2 0 0 0 4 0",
  clock: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 7v5l3 2",
  filter: "M3 5h18l-7 8v6l-4-2v-4z",
  spinner: "M12 3a9 9 0 0 1 9 9",
  info: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 11v5M12 8v.5",
  eye: "M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z",
  save: "M5 3h11l3 3v15H5zM8 3v6h7V3M8 21v-7h8v7",
  cal: "M3 5h18v16H3zM3 10h18M8 3v4M16 3v4",
  robot: "M5 8h14v11H5zM9 12h.5M14.5 12h.5M12 4v4M9 19v2M15 19v2",
  drive: "M3 6h18v12H3zM7 15h.5M11 15h6",
};

export function Icon(props: { name: keyof typeof PATHS | string; size?: number; class?: string; spin?: boolean }) {
  const d = () => PATHS[props.name] ?? PATHS.info;
  return (
    <svg
      class={`ic ${props.spin ? "spin" : ""} ${props.class ?? ""}`}
      width={props.size ?? 18}
      height={props.size ?? 18}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.75"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d={d()} />
    </svg>
  );
}

// ---- 버튼 · 칩 · 필 ------------------------------------------------------------- //

type BtnProps = ParentProps<
  JSX.ButtonHTMLAttributes<HTMLButtonElement> & {
    variant?: "primary" | "default" | "ghost" | "danger";
    size?: "sm" | "md";
    icon?: string;
    loading?: boolean;
  }
>;

export function Button(props: BtnProps) {
  const [local, rest] = splitProps(props, ["variant", "size", "icon", "loading", "children", "class", "disabled"]);
  return (
    <button
      type="button"
      class={`btn btn-${local.variant ?? "default"} btn-${local.size ?? "md"} ${local.class ?? ""}`}
      disabled={local.disabled || local.loading}
      {...rest}
    >
      <Show when={local.loading} fallback={local.icon ? <Icon name={local.icon} size={local.size === "sm" ? 14 : 16} /> : null}>
        <Icon name="spinner" size={local.size === "sm" ? 14 : 16} spin />
      </Show>
      {local.children}
    </button>
  );
}

export function Chip(
  props: ParentProps<{ on?: boolean; dashed?: boolean; memo?: boolean; onClick?: () => void; title?: string; class?: string }>,
) {
  return (
    <button
      type="button"
      class={`chip ${props.on ? "chip-on" : ""} ${props.dashed ? "chip-dashed" : ""} ${props.memo ? "chip-memo" : ""} ${props.onClick ? "chip-click" : ""} ${props.class ?? ""}`}
      onClick={props.onClick}
      title={props.title}
      tabIndex={props.onClick ? 0 : -1}
    >
      {props.children}
    </button>
  );
}

export function Pill(props: ParentProps<{ tone?: "default" | "ok" | "warn" | "muted"; class?: string; title?: string }>) {
  return (
    <span class={`pill pill-${props.tone ?? "default"} ${props.class ?? ""}`} title={props.title}>
      {props.children}
    </span>
  );
}

// ---- 폼 조각 ------------------------------------------------------------------ //

export function Toggle(props: { checked: boolean; onChange: (v: boolean) => void; label?: JSX.Element; hint?: string; disabled?: boolean }) {
  return (
    <label class={`toggle ${props.disabled ? "is-disabled" : ""}`}>
      <input
        type="checkbox"
        role="switch"
        checked={props.checked}
        disabled={props.disabled}
        onChange={(e) => props.onChange(e.currentTarget.checked)}
      />
      <span class="toggle-track" aria-hidden="true">
        <span class="toggle-knob" />
      </span>
      <Show when={props.label}>
        <span class="toggle-label">{props.label}</span>
      </Show>
      <Show when={props.hint}>
        <span class="muted small">{props.hint}</span>
      </Show>
    </label>
  );
}

export function Select<T extends string | number>(props: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
  disabled?: boolean;
  class?: string;
}) {
  return (
    <select
      class={`select ${props.class ?? ""}`}
      value={String(props.value)}
      disabled={props.disabled}
      onChange={(e) => {
        const raw = e.currentTarget.value;
        const opt = props.options.find((o) => String(o.value) === raw);
        if (opt) props.onChange(opt.value);
      }}
    >
      <For each={props.options}>{(o) => <option value={String(o.value)}>{o.label}</option>}</For>
    </select>
  );
}

/** 설정 행: 왼쪽 라벨(고정폭) + 오른쪽 내용. */
export function Field(props: ParentProps<{ label: string; hint?: string; top?: boolean }>) {
  return (
    <div class={`field ${props.top ? "field-top" : ""}`}>
      <span class="field-label">{props.label}</span>
      <div class="field-body">
        {props.children}
        <Show when={props.hint}>
          <div class="muted small field-hint">{props.hint}</div>
        </Show>
      </div>
    </div>
  );
}

/** 텍스트 입력(한 줄). */
export function TextInput(props: JSX.InputHTMLAttributes<HTMLInputElement> & { mono?: boolean }) {
  const [local, rest] = splitProps(props, ["class", "mono"]);
  return <input type="text" class={`input ${local.mono ? "mono" : ""} ${local.class ?? ""}`} spellcheck={false} {...rest} />;
}

export function Spinner(props: { size?: number }) {
  return <Icon name="spinner" size={props.size ?? 16} spin />;
}

/** 빈 상태 안내. */
export function Empty(props: ParentProps<{ icon?: string; title: string; hint?: string }>) {
  return (
    <div class="empty">
      <Icon name={props.icon ?? "info"} size={28} />
      <div class="empty-title">{props.title}</div>
      <Show when={props.hint}>
        <div class="muted small">{props.hint}</div>
      </Show>
      {props.children}
    </div>
  );
}

// ---- 토스트 ------------------------------------------------------------------ //

export function Toasts() {
  return (
    <div class="toasts" aria-live="polite">
      <For each={toasts()}>
        {(t) => (
          <div class={`toast toast-${t.kind}`} role="status">
            <Icon name={t.kind === "ok" ? "check" : t.kind === "error" ? "alert" : "info"} size={16} />
            <span class="toast-text">{t.text}</span>
            <Show when={t.action}>
              {(a) => (
                <button
                  type="button"
                  class="toast-action"
                  onClick={() => {
                    a().run();
                    dismissToast(t.id);
                  }}
                >
                  {a().label}
                </button>
              )}
            </Show>
            <button type="button" class="toast-close" aria-label="닫기" onClick={() => dismissToast(t.id)}>
              <Icon name="x" size={14} />
            </button>
          </div>
        )}
      </For>
    </div>
  );
}

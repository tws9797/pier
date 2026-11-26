import { Component, useEffect, useRef, useState, type ReactNode } from "react";

/** Keeps a crash in one view from blanking the whole window. */
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: string | null }> {
  state = { error: null as string | null };
  static getDerivedStateFromError(e: unknown) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
  render() {
    if (this.state.error === null) return this.props.children;
    return (
      <Empty title="This view hit an error" hint={this.state.error}>
        <button className="btn" onClick={() => this.setState({ error: null })}>
          Try again
        </button>
      </Empty>
    );
  }
}

const paths: Record<string, ReactNode> = {
  play: <path d="M6 4l14 8-14 8z" />,
  stop: <rect x="5" y="5" width="14" height="14" rx="1.5" />,
  restart: <path d="M3 12a9 9 0 1 0 3-6.7M3 4v5h5" />,
  logs: <path d="M4 6h16M4 12h16M4 18h10" />,
  terminal: <path d="M4 17l6-5-6-5M12 19h8" />,
  trash: <path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3" />,
  box: <path d="M21 8l-9-5-9 5v8l9 5 9-5zM3 8l9 5 9-5M12 13v8" />,
  layers: <path d="M12 3l9 5-9 5-9-5zM3 13l9 5 9-5" />,
  disk: <path d="M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3v12c0 1.7-3.6 3-8 3s-8-1.3-8-3zM4 6c0 1.7 3.6 3 8 3s8-1.3 8-3M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3" />,
  gear: (
    <>
      <circle cx="12" cy="12" r="3" />
      <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
    </>
  ),
  close: <path d="M6 6l12 12M18 6L6 18" />,
  "dock-right": <path d="M3 5h18v14H3zM14 5v14" />,
  "dock-bottom": <path d="M3 5h18v14H3zM3 13h18" />,
};

export function Icon({ name, size = 15 }: { name: keyof typeof paths | string; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      {paths[name]}
    </svg>
  );
}

export function IconButton(props: {
  icon: string;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      className={"icon-btn" + (props.danger ? " danger" : "")}
      title={props.label}
      aria-label={props.label}
      disabled={props.disabled}
      onClick={(e) => {
        e.stopPropagation();
        props.onClick();
      }}
    >
      <Icon name={props.icon} />
    </button>
  );
}

/**
 * Which rows of a list are ticked. Ids that are no longer in the list drop
 * out on their own, so the selection is empty again once its rows are deleted.
 */
export function useSelection(ids: string[]) {
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const anchor = useRef<string | null>(null);
  const selected = new Set(ids.filter((id) => picked.has(id)));

  /** Ticks or unticks one row; with `range`, every row from the last one touched to this one. */
  const toggle = (id: string, range = false) => {
    const next = new Set(selected);
    const on = !selected.has(id);
    const from = anchor.current === null ? -1 : ids.indexOf(anchor.current);
    const to = ids.indexOf(id);
    const span = range && from >= 0 ? ids.slice(Math.min(from, to), Math.max(from, to) + 1) : [id];
    for (const each of span) on ? next.add(each) : next.delete(each);
    anchor.current = id;
    setPicked(next);
  };
  const all = ids.length > 0 && selected.size === ids.length;
  return {
    selected,
    all,
    some: selected.size > 0 && !all,
    toggle,
    toggleAll: () => setPicked(all ? new Set() : new Set(ids)),
    clear: () => setPicked(new Set()),
  };
}

export function Check(props: {
  checked: boolean;
  /** Some but not all: shown as a dash. */
  partial?: boolean;
  label: string;
  onChange: (shiftKey: boolean) => void;
}) {
  const box = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (box.current) box.current.indeterminate = props.partial ?? false;
  }, [props.partial]);
  return (
    <input
      ref={box}
      type="checkbox"
      className="check"
      aria-label={props.label}
      checked={props.checked}
      // The click handler does the work: it knows whether Shift was held.
      onChange={() => {}}
      onClick={(e) => {
        e.stopPropagation();
        props.onChange(e.shiftKey);
      }}
    />
  );
}

/** State that survives restarts (kept in the webview's localStorage). */
export function useStored<T extends object>(key: string, fallback: T) {
  const [value, setValue] = useState<T>(() => {
    try {
      return { ...fallback, ...JSON.parse(localStorage.getItem(key) ?? "{}") };
    } catch {
      return fallback;
    }
  });
  useEffect(() => {
    try {
      localStorage.setItem(key, JSON.stringify(value));
    } catch {
      // Storage can be unavailable; the layout then just resets next time.
    }
  }, [key, value]);
  return [value, setValue] as const;
}

const caption = { width: 10, height: 10, viewBox: "0 0 10 10", fill: "none", stroke: "currentColor", strokeWidth: 1 };

/**
 * Pier has no native title bar: the top strip of the app is the drag area
 * and these are its minimise / maximise / close buttons.
 */
export function WindowChrome(props: {
  maximized: boolean;
  showControls: boolean;
  onMinimize: () => void;
  onToggleMaximize: () => void;
  onClose: () => void;
}) {
  return (
    <>
      <div className="drag-strip" data-tauri-drag-region />
      {props.showControls && (
        <div className="window-controls">
          <button title="Minimize" aria-label="Minimize" onClick={props.onMinimize}>
            <svg {...caption} aria-hidden>
              <path d="M0 5.5h10" />
            </svg>
          </button>
          <button
            title={props.maximized ? "Restore" : "Maximize"}
            aria-label={props.maximized ? "Restore" : "Maximize"}
            onClick={props.onToggleMaximize}
          >
            <svg {...caption} aria-hidden>
              {props.maximized ? <path d="M2.5 2.5v-2h7v7h-2M.5 2.5h7v7h-7z" /> : <path d="M.5.5h9v9h-9z" />}
            </svg>
          </button>
          <button className="close" title="Close to tray" aria-label="Close" onClick={props.onClose}>
            <svg {...caption} aria-hidden>
              <path d="M0 0l10 10M10 0L0 10" />
            </svg>
          </button>
        </div>
      )}
    </>
  );
}

/**
 * A text button for bulk deletes: the first click arms it, the second one
 * within 3 s runs it. While the action runs the button is disabled and shows
 * `busyText`, so it cannot be started twice and its label stays put.
 */
export function ConfirmTextButton(props: {
  text: string;
  confirmText: string;
  busyText: string;
  disabled?: boolean;
  onConfirm: () => Promise<unknown>;
}) {
  const [phase, setPhase] = useState<"idle" | "armed" | "busy">("idle");
  // The wording is frozen when armed: the list may refresh underneath it.
  const [frozen, setFrozen] = useState("");
  useEffect(() => {
    if (phase !== "armed") return;
    const t = setTimeout(() => setPhase("idle"), 3000);
    return () => clearTimeout(t);
  }, [phase]);

  if (phase === "busy") {
    return (
      <button className="btn" disabled>
        <span className="spinner" />
        {props.busyText}
      </button>
    );
  }
  return (
    <button
      className={"btn" + (phase === "armed" ? " danger" : "")}
      disabled={props.disabled}
      onClick={() => {
        if (phase === "idle") {
          setFrozen(props.confirmText);
          setPhase("armed");
          return;
        }
        setPhase("busy");
        props.onConfirm().finally(() => setPhase("idle"));
      }}
    >
      {phase === "armed" ? frozen : props.text}
    </button>
  );
}

/** A delete button that asks for a second click instead of a popup. */
export function ConfirmButton({ label, onConfirm }: { label: string; onConfirm: () => void }) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const t = setTimeout(() => setArmed(false), 2500);
    return () => clearTimeout(t);
  }, [armed]);

  if (armed) {
    return (
      <button
        className="btn small danger"
        onClick={(e) => {
          e.stopPropagation();
          setArmed(false);
          onConfirm();
        }}
      >
        Confirm
      </button>
    );
  }
  return <IconButton icon="trash" label={label} danger onClick={() => setArmed(true)} />;
}

export function formatBytes(n: number) {
  if (n >= 1e9) return (n / 1e9).toFixed(2) + " GB";
  if (n >= 1e6) return (n / 1e6).toFixed(0) + " MB";
  return (n / 1e3).toFixed(0) + " kB";
}

export function timeAgo(date: Date) {
  const s = (Date.now() - date.getTime()) / 1000;
  if (s < 3600) return Math.max(1, Math.round(s / 60)) + " min ago";
  if (s < 86400) return Math.round(s / 3600) + " h ago";
  const d = Math.round(s / 86400);
  return d < 60 ? d + (d === 1 ? " day ago" : " days ago") : Math.round(d / 30) + " months ago";
}

export function Empty({ title, hint, children }: { title: string; hint?: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty-title">{title}</div>
      {hint && <div className="empty-hint">{hint}</div>}
      {children}
    </div>
  );
}

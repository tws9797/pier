import { useEffect, useRef, useState, type PointerEvent } from "react";
import * as api from "./api";
import { Inspect } from "./Inspect";
import { IconButton } from "./ui";

// Colour codes are dropped rather than rendered, for now.
const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]/g;
const MAX_CHARS = 400_000;

/** Keeps the last MAX_CHARS, cutting at a line start when there is one in range. */
export function clip(text: string) {
  if (text.length <= MAX_CHARS) return text;
  const lineStart = text.indexOf("\n", text.length - MAX_CHARS);
  // A single line longer than the limit has no newline to cut at; cut hard instead.
  return text.slice(lineStart === -1 ? text.length - MAX_CHARS : lineStart + 1);
}

export type LogsDock = "bottom" | "right";
/** Where the log pane sits and how big it is in each position (px). Remembered between runs. */
export interface LogsLayout {
  dock: LogsDock;
  bottom: number;
  right: number;
}
export const DEFAULT_LOGS_LAYOUT: LogsLayout = { dock: "bottom", bottom: 300, right: 460 };

export function Logs(props: {
  /** The distro the container lives in. */
  distro: string;
  id: string;
  title: string;
  layout: LogsLayout;
  onLayout: (layout: LogsLayout) => void;
  onClose: () => void;
}) {
  const { distro, id, title, layout, onLayout, onClose } = props;
  const { dock } = layout;
  const root = useRef<HTMLDivElement>(null);

  // Dragging the edge resizes the pane; the CSS min/max keep the list usable.
  // While dragging, the size is written straight to the element: going through
  // React state on every mouse move re-rendered the whole container list and
  // made the drag stutter. The final size is committed (and saved) on release.
  const dragged = useRef<number | null>(null);
  const resize = (e: PointerEvent<HTMLDivElement>) => {
    const el = root.current;
    const area = el?.parentElement?.getBoundingClientRect();
    if (!el || !area || !e.currentTarget.hasPointerCapture(e.pointerId)) return;
    const size = dock === "bottom" ? area.bottom - e.clientY : area.right - e.clientX;
    const max = dock === "bottom" ? area.height - 120 : area.width - 320;
    dragged.current = Math.round(Math.max(dock === "bottom" ? 120 : 280, Math.min(size, max)));
    el.style[dock === "bottom" ? "height" : "width"] = dragged.current + "px";
  };
  const commitResize = () => {
    if (dragged.current !== null) onLayout({ ...layout, [dock]: dragged.current });
    dragged.current = null;
  };
  // What the pane shows. The log stream is only held while its tab is.
  const [tab, setTab] = useState<"logs" | "inspect">("logs");
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const box = useRef<HTMLPreElement>(null);
  const stick = useRef(true);

  // The engine ends a log stream when the container stops, so follow again each time it starts.
  const [run, setRun] = useState(0);
  useEffect(
    () => api.onDockerEvent((e) => e.type === "container" && e.action === "start" && e.id === id && setRun((n) => n + 1)),
    [id],
  );

  // No stream is held while the window is minimised; it is followed again on return.
  const [visible, setVisible] = useState(document.visibilityState === "visible");
  useEffect(() => api.onVisibility(setVisible), []);

  useEffect(() => {
    if (!visible || tab !== "logs") return;
    let stop: (() => void) | undefined;
    let cancelled = false;
    // Chunks are collected and painted at most once per frame, so a chatty
    // container cannot make the pane re-render for every chunk.
    let incoming = "";
    let frame = 0;
    // Batches received but not yet confirmed. Each is confirmed when it has
    // been painted, which is what lets the backend send the next one.
    let confirm: (() => void)[] = [];
    const paint = () => {
      frame = 0;
      const add = incoming;
      incoming = "";
      setText((t) => clip(t + add));
      const painted = confirm;
      confirm = [];
      painted.forEach((done) => done());
    };
    setText("");
    setError(null);
    api
      .followLogs(distro, id, (chunk, done) => {
        if (cancelled) return;
        confirm.push(done);
        incoming = clip(incoming + chunk.replace(ANSI, ""));
        frame ||= requestAnimationFrame(paint);
      })
      .then((s) => (cancelled ? s() : (stop = s)))
      .catch((e) => !cancelled && setError(api.errorText(e)));
    return () => {
      cancelled = true;
      cancelAnimationFrame(frame);
      stop?.();
    };
  }, [distro, id, run, visible, tab]);

  // Follow new output unless the user has scrolled up to read something.
  useEffect(() => {
    const el = box.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [text]);

  return (
    <div
      ref={root}
      className={"logs " + dock}
      style={dock === "bottom" ? { height: layout.bottom } : { width: layout.right }}
    >
      <div
        className="logs-resize"
        title="Drag to resize, double-click to reset"
        onPointerDown={(e) => e.currentTarget.setPointerCapture(e.pointerId)}
        onPointerMove={resize}
        onLostPointerCapture={commitResize}
        onDoubleClick={() => onLayout({ ...layout, [dock]: DEFAULT_LOGS_LAYOUT[dock] })}
      />
      <header className="logs-head">
        <span className="logs-title">{title}</span>
        <span className="tabs" role="tablist">
          {(["logs", "inspect"] as const).map((t) => (
            <button key={t} role="tab" aria-selected={tab === t} className={tab === t ? "active" : ""} onClick={() => setTab(t)}>
              {t === "logs" ? "Logs" : "Inspect"}
            </button>
          ))}
        </span>
        <span className="spacer" />
        {tab === "logs" && (
          <button className="btn small" onClick={() => setText("")}>
            Clear
          </button>
        )}
        <IconButton
          icon={dock === "bottom" ? "dock-right" : "dock-bottom"}
          label={dock === "bottom" ? "Move to the side" : "Move to the bottom"}
          onClick={() => onLayout({ ...layout, dock: dock === "bottom" ? "right" : "bottom" })}
        />
        <IconButton icon="close" label="Close panel" onClick={onClose} />
      </header>
      {tab === "inspect" && <Inspect distro={distro} id={id} />}
      <pre
        ref={box}
        hidden={tab !== "logs"}
        className="logs-body"
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
      >
        {error ?? (text || "No output yet.")}
      </pre>
    </div>
  );
}

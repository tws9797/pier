import { useState, useSyncExternalStore } from "react";
import * as api from "./api";
import { formatBytes } from "./ui";

/**
 * The pull in progress, if any. It lives outside the Images page so that
 * looking at another page and coming back finds it still there: the pull
 * itself goes on in the backend either way.
 */
interface Running {
  distro: string;
  image: string;
  progress: api.PullProgress | null;
  cancel: () => void;
}
let running: Running | null = null;
const watchers = new Set<() => void>();
const set = (next: Running | null) => {
  running = next;
  watchers.forEach((w) => w());
};
const watch = (w: () => void) => {
  watchers.add(w);
  return () => void watchers.delete(w);
};

type Run = (action: () => Promise<unknown>) => Promise<void>;

/** Starts a pull. `run` shows the reason if it fails and reloads the lists either way. */
function start(distro: string, image: string, run: Run) {
  // The backend hands back the means to cancel at once, before Docker or the
  // registry has answered. A Cancel pressed in the instant before that is
  // remembered and carried out when it arrives.
  let cancelled = false;
  set({ distro, image, progress: null, cancel: () => void (cancelled = true) });
  return run(async () => {
    try {
      const pull = await api.pullImage(distro, image, (progress) => running && set({ ...running, progress }));
      if (cancelled) pull.cancel();
      else if (running) set({ ...running, cancel: pull.cancel });
      await pull.finished;
    } catch (e) {
      // Stopping it was the user's doing; there is nothing to report.
      if (e !== "Cancelled") throw e;
    } finally {
      set(null);
    }
  });
}

function describe(p: api.PullProgress | null) {
  if (!p || p.layers === 0) return "Asking the registry…";
  const layers = `${p.done} of ${p.layers} layers`;
  return p.total > 0 ? `${layers}, ${formatBytes(p.received)} of ${formatBytes(p.total)}` : layers;
}

/**
 * The strip under the Images toolbar: a box to type an image name into, or
 * the progress of the pull that is running.
 */
export function PullBar({ distro, run, onClose }: { distro: string; run: Run; onClose: () => void }) {
  const pull = useSyncExternalStore(watch, () => running);
  const [image, setImage] = useState("");

  if (pull) {
    const p = pull.progress;
    const share = p && p.total > 0 ? p.received / p.total : 0;
    return (
      <div className="pull">
        <div className="pull-text">
          <span className="pull-name">Pulling {pull.image}</span>
          <span className="muted">{pull.distro === distro ? describe(p) : `into ${pull.distro}`}</span>
        </div>
        <div className="pull-track" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(share * 100)}>
          <div style={{ width: `${share * 100}%` }} />
        </div>
        <button className="btn small" onClick={pull.cancel}>
          Cancel
        </button>
      </div>
    );
  }

  return (
    <form
      className="pull"
      onSubmit={(e) => {
        e.preventDefault();
        if (image.trim()) void start(distro, image.trim(), run);
      }}
    >
      <input
        autoFocus
        spellCheck={false}
        placeholder="Image to pull, for example nginx:alpine"
        aria-label="Image to pull"
        value={image}
        onChange={(e) => setImage(e.target.value)}
        onKeyDown={(e) => e.key === "Escape" && onClose()}
      />
      <button className="btn small primary" disabled={!image.trim()}>
        Pull
      </button>
      <button type="button" className="btn small" onClick={onClose}>
        Close
      </button>
    </form>
  );
}

/** True while a pull is running, so the page can keep the strip open. */
export const usePulling = () => useSyncExternalStore(watch, () => running !== null);

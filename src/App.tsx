import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "./api";
import type { Container, Image, Status, Volume } from "./api";
import { Containers } from "./Containers";
import { Images, Volumes } from "./Lists";
import { Settings } from "./Settings";
import { Empty, ErrorBoundary, Icon, WindowChrome } from "./ui";

type View = "containers" | "images" | "volumes" | "settings";
const LISTS = ["containers", "images", "volumes"] as const;
type List = (typeof LISTS)[number];

/** The lists on screen, and the distro they were loaded from. */
interface Loaded {
  distro: string | null;
  containers: Container[];
  images: Image[];
  volumes: Volume[];
}
const NOTHING: Loaded = { distro: null, containers: [], images: [], volumes: [] };

export default function App() {
  const [view, setView] = useState<View>("containers");
  const [status, setStatus] = useState<Status | null>(null);
  const [loaded, setLoaded] = useState<Loaded>(NOTHING);
  const [busy, setBusy] = useState<"starting" | "stopping" | null>(null);
  const [error, setError] = useState<string | null>(null);

  const distro = status?.distro ?? null;
  const distroNow = useRef(distro);
  distroNow.current = distro;
  // Rows are only ever shown next to the distro they were loaded from: right
  // after a switch the lists are empty until the new distro's data is in, so
  // there is never a row on screen whose button would act somewhere else.
  const { containers, images, volumes } = loaded.distro === distro ? loaded : NOTHING;

  const up = status?.engineRunning ?? false;
  /** The engine was let go while idle and is being started again for this window. */
  const waking = !up && (status?.sleeping ?? false);

  // One status request at a time. While a sleeping engine wakes the window
  // asks every second, and a slow Docker would otherwise have a pile of
  // probes (each a wsl.exe or two) running at once, answering out of order.
  // A request made while one is running becomes a single follow-up, so the
  // last answer shown is always the newest one asked for.
  const statusRun = useRef<{ again: boolean; done: Promise<void> } | null>(null);
  const refreshStatus = useCallback((): Promise<void> => {
    if (statusRun.current) {
      statusRun.current.again = true;
      return statusRun.current.done;
    }
    const run = { again: false, done: Promise.resolve() };
    run.done = (async () => {
      try {
        do {
          run.again = false;
          await api.getStatus().then(setStatus, (e) => setError(api.errorText(e)));
        } while (run.again);
      } finally {
        statusRun.current = null;
      }
    })();
    statusRun.current = run;
    return run.done;
  }, []);

  // One request per list at a time. A refresh asked for while one is running
  // is folded into a single follow-up run, so slow answers cannot pile up.
  //
  // A list that fails to load is asked for again, twice, a little later each
  // time. It used to be dropped without a word, which left an empty list on
  // screen until the next Docker event or the one-minute refresh. If it still
  // fails while the engine is up, the reason is shown.
  const loading = useRef<Partial<Record<List, { again: boolean; done: Promise<void> }>>>({});
  const upNow = useRef(up);
  upNow.current = up;
  const load = useCallback(async (list: List, from: string) => {
    // Not worth another try once a different distro is selected or the engine
    // is known to be down: the status says so already.
    const wanted = () => distroNow.current === from && upNow.current;
    const ask = (): Promise<Container[] | Image[] | Volume[]> =>
      list === "containers" ? api.listContainers(from) : list === "images" ? api.listImages(from) : api.listVolumes(from);
    const result = await api.retrying(ask, wanted);
    if (result.ok) return result.value;
    if (wanted()) setError(`Could not load ${list}: ${api.errorText(result.error)}`);
    return null;
  }, []);
  const refresh = useCallback((list: List): Promise<void> => {
    const running = loading.current[list];
    if (running) {
      running.again = true;
      return running.done;
    }
    const state = { again: false, done: Promise.resolve() };
    state.done = (async () => {
      try {
        do {
          state.again = false;
          const from = distroNow.current;
          if (!from) return;
          const rows = await load(list, from);
          // An answer from a distro that is no longer selected is dropped.
          if (distroNow.current !== from) state.again = true;
          // No answer: what is on screen stays as it is, rather than going empty.
          else if (rows) setLoaded((prev) => ({ ...(prev.distro === from ? prev : NOTHING), distro: from, [list]: rows }));
        } while (state.again);
      } finally {
        delete loading.current[list];
      }
    })();
    loading.current[list] = state;
    return state.done;
  }, [load]);
  const refreshData = useCallback(() => Promise.all(LISTS.map(refresh)), [refresh]);

  // Nothing polls while the window is minimised or hidden to the tray.
  const [visible, setVisible] = useState(document.visibilityState === "visible");
  useEffect(() => api.onVisibility(setVisible), []);

  const [maximized, setMaximized] = useState(false);
  useEffect(() => api.onMaximizedChange(setMaximized), []);

  // The first status is fetched even if the window starts out hidden.
  useEffect(() => void refreshStatus(), [refreshStatus]);
  useEffect(() => {
    if (!visible) return;
    refreshStatus();
    // Poll faster while a sleeping engine is coming back, so the wait looks short.
    const t = setInterval(refreshStatus, waking ? 1000 : 5000);
    return () => clearInterval(t);
  }, [visible, waking, refreshStatus]);

  // Load data when the engine comes up, on Docker events, and on a slow timer as a fallback.
  // An event only reloads the lists it can have changed.
  const debounce = useRef<number>(undefined);
  const stale = useRef(new Set<List>());
  useEffect(() => {
    // Nothing is asked of an engine that is being stopped. The backend refuses
    // such requests anyway; this just avoids making them.
    if (!up || !visible || busy === "stopping") return;
    refreshData();
    const off = api.onDockerEvent((e) => {
      if (e.type === "container") {
        stale.current.add("containers");
        // Creating or removing a container changes which images are in use.
        if (e.action === "create" || e.action === "destroy") stale.current.add("images");
      } else if (e.type === "image") stale.current.add("images");
      else if (e.type === "volume") stale.current.add("volumes");
      else return;
      clearTimeout(debounce.current);
      debounce.current = window.setTimeout(() => {
        const lists = [...stale.current];
        stale.current.clear();
        lists.forEach(refresh);
      }, 250);
    });
    const t = setInterval(refreshData, 60000);
    return () => {
      off();
      clearInterval(t);
      // A refresh already waiting on the debounce timer goes with it.
      clearTimeout(debounce.current);
      stale.current.clear();
    };
    // `distro` is here so that selecting another one reloads everything at once.
  }, [up, visible, busy, distro, refreshData, refresh]);

  /** Runs an action, surfaces its error, then refreshes the lists. */
  const run = useCallback(
    async (action: () => Promise<unknown>) => {
      try {
        await action();
      } catch (e) {
        setError(api.errorText(e));
      }
      await refreshData();
    },
    [refreshData],
  );

  const engineAction = async (label: "starting" | "stopping", action: () => Promise<unknown>) => {
    setBusy(label);
    try {
      await action();
    } catch (e) {
      setError(api.errorText(e));
    }
    await refreshStatus();
    setBusy(null);
  };
  const toggleEngine = () => {
    if (!distro) return;
    return up ? engineAction("stopping", () => api.stopEngine(distro)) : engineAction("starting", () => api.startEngine(distro));
  };
  /** Always a shutdown, whether or not the engine is up: it must never start anything. */
  const shutdownDistro = () => distro && engineAction("stopping", () => api.stopEngine(distro, true));

  useEffect(() => {
    if (!error) return;
    const t = setTimeout(() => setError(null), 6000);
    return () => clearTimeout(t);
  }, [error]);

  const nav: { id: View; label: string; icon: string; count?: number }[] = [
    { id: "containers", label: "Containers", icon: "box", count: containers.filter((c) => c.State === "running").length },
    { id: "images", label: "Images", icon: "layers", count: images.length },
    { id: "volumes", label: "Volumes", icon: "disk", count: volumes.length },
    { id: "settings", label: "Settings", icon: "gear" },
  ];

  const engineLabel =
    busy === "starting" ? "Starting…" : busy === "stopping" ? "Stopping…" : up ? "Running" : waking ? "Waking…" : "Stopped";

  return (
    <div className={api.hasWindowControls ? "app desktop" : "app"}>
      <WindowChrome
        maximized={maximized}
        showControls={api.hasWindowControls}
        onMinimize={api.minimizeWindow}
        onToggleMaximize={api.toggleMaximizeWindow}
        onClose={api.closeWindow}
      />
      <aside className="sidebar">
        <div className="brand">Pier</div>
        <nav>
          {nav.map((n) => (
            <button key={n.id} className={"nav-item" + (view === n.id ? " active" : "")} onClick={() => setView(n.id)}>
              <Icon name={n.icon} />
              <span>{n.label}</span>
              {up && n.count !== undefined && <span className="count">{n.count}</span>}
            </button>
          ))}
        </nav>

        <div className="engine">
          <div className="engine-line">
            <span className={"dot " + (busy || waking ? "pending" : up ? "on" : "off")} />
            <span className="engine-state">{engineLabel}</span>
            {up && status?.version && <span className="muted">v{status.version}</span>}
          </div>
          <div className="muted engine-distro">{status?.distro ?? "No WSL distro found"}</div>
          <button className="btn block" disabled={!status?.distro || busy !== null || waking} onClick={() => toggleEngine()}>
            {up ? "Stop engine" : "Start engine"}
          </button>
        </div>
      </aside>

      <main className="main">
        <ErrorBoundary key={view}>
        {view === "settings" ? (
          <Settings status={status} onChanged={refreshStatus} onShutdown={shutdownDistro} onError={setError} />
        ) : !status ? null : !up ? (
          <Empty
            title={waking ? "Waking the Docker engine…" : status.distro ? "Docker engine is stopped" : "No WSL distro found"}
            hint={
              waking
                ? "Pier let it rest while nothing was running. It will be back in a few seconds."
                : status.distro
                  ? `Start it to manage containers in ${status.distro}.`
                  : "Install a WSL2 distro with Docker Engine, then reopen Pier."
            }
          >
            {status.distro && !waking && (
              <button className="btn primary" disabled={busy !== null} onClick={() => toggleEngine()}>
                {busy === "starting" ? "Starting…" : "Start engine"}
              </button>
            )}
          </Empty>
        ) : view === "containers" ? (
          <Containers distro={distro ?? ""} containers={containers} run={run} onError={setError} />
        ) : view === "images" ? (
          <Images distro={distro ?? ""} images={images} run={run} />
        ) : (
          <Volumes distro={distro ?? ""} volumes={volumes} containers={containers} run={run} />
        )}
        </ErrorBoundary>
      </main>

      {error && (
        <div className="toast" role="alert" onClick={() => setError(null)}>
          {error}
        </div>
      )}
    </div>
  );
}

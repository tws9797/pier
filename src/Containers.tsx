import { useEffect, useMemo, useState } from "react";
import * as api from "./api";
import type { Container } from "./api";
import { DEFAULT_LOGS_LAYOUT, Logs } from "./Logs";
import { ConfirmButton, Empty, formatBytes, IconButton, useStored } from "./ui";

type Run = (action: () => Promise<unknown>) => Promise<void>;

const name = (c: Container) => c.Names[0]?.replace(/^\//, "") ?? c.Id.slice(0, 12);
const project = (c: Container) => c.Labels["com.docker.compose.project"];
const isUp = (c: Container) => c.State === "running" || c.State === "restarting" || c.State === "paused";

function publishedPorts(c: Container) {
  const tcp = c.Ports.filter((p) => p.PublicPort && p.Type === "tcp").map((p) => p.PublicPort!);
  return [...new Set(tcp)].sort((a, b) => a - b);
}

interface Usage {
  /** Percent of one core; null until there are two samples to compare. */
  cpu: number | null;
  memory: number;
}

/**
 * CPU and memory of the running containers, refreshed every few seconds
 * while the window can be seen. One request (and one relay) per refresh,
 * whatever the number of containers; nothing at all while hidden.
 */
function useUsage(distro: string, runningIds: string[]) {
  const [usage, setUsage] = useState<Record<string, Usage>>({});
  const [visible, setVisible] = useState(document.visibilityState === "visible");
  useEffect(() => api.onVisibility(setVisible), []);
  const ids = runningIds.join(",");

  useEffect(() => {
    if (!visible || !ids) {
      setUsage({});
      return;
    }
    let before: Record<string, api.UsageSample> = {};
    let cancelled = false;
    let asking = false;
    const sample = async () => {
      if (asking) return; // the last one has not answered yet
      asking = true;
      try {
        const now = await api.sampleUsage(distro, ids.split(","));
        if (cancelled) return;
        const next: Record<string, Usage> = {};
        for (const [id, s] of Object.entries(now)) {
          next[id] = { cpu: before[id] ? api.cpuPercent(before[id], s) : null, memory: s.memory };
        }
        before = now;
        setUsage(next);
      } catch {
        // The engine went away, or another distro was selected: the list will follow.
      } finally {
        asking = false;
      }
    };
    sample();
    const second = setTimeout(sample, 1000); // a CPU figure needs two samples: get the second soon
    const every = setInterval(sample, 3000);
    return () => {
      cancelled = true;
      clearTimeout(second);
      clearInterval(every);
    };
  }, [distro, ids, visible]);

  return usage;
}

const percent = (n: number) => (n >= 100 ? n.toFixed(0) : n.toFixed(1)) + "%";

export function Containers(props: { distro: string; containers: Container[]; run: Run; onError: (e: string) => void }) {
  // `distro` is the one these rows were loaded from; every action names it.
  const { distro, containers, run } = props;
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [pending, setPending] = useState<Set<string>>(new Set());
  const [logsLayout, setLogsLayout] = useStored("pier.logs", DEFAULT_LOGS_LAYOUT);

  const groups = useMemo(() => {
    const q = query.trim().toLowerCase();
    const shown = containers
      .filter((c) => !q || name(c).toLowerCase().includes(q) || c.Image.toLowerCase().includes(q))
      .sort((a, b) => name(a).localeCompare(name(b)));
    const map = new Map<string, Container[]>();
    for (const c of shown) {
      const key = project(c) ?? "";
      map.set(key, [...(map.get(key) ?? []), c]);
    }
    // Compose projects first, alphabetically; standalone containers last.
    return [...map.entries()].sort(([a], [b]) => (a === "" ? 1 : b === "" ? -1 : a.localeCompare(b)));
  }, [containers, query]);

  const act = async (ids: string[], action: (id: string) => Promise<unknown>) => {
    setPending((p) => new Set([...p, ...ids]));
    // Four at a time: a big Compose project must not start one relay per container at once.
    await run(() => api.runEach(ids, action));
    setPending((p) => new Set([...p].filter((id) => !ids.includes(id))));
  };

  const selectedContainer = containers.find((c) => c.Id === selected);
  const usage = useUsage(
    distro,
    useMemo(() => containers.filter((c) => c.State === "running").map((c) => c.Id).sort(), [containers]),
  );

  return (
    <div className="pane">
      <header className="toolbar">
          <h1>Containers</h1>
          <input
            className="search"
            placeholder="Filter by name or image"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
      </header>

      <div className={"split " + logsLayout.dock}>
        <div className="scroll">
          {containers.length === 0 && (
            <Empty title="No containers yet" hint="Run docker compose up in your WSL distro and they will show up here." />
          )}
          {groups.map(([group, items]) => {
            const running = items.filter(isUp);
            const stopped = items.filter((c) => !isUp(c));
            return (
              <section key={group || "standalone"} className="group">
                <div className="group-head">
                  <span className="group-name">{group || "Standalone"}</span>
                  <span className="muted">
                    {running.length}/{items.length} running
                  </span>
                  {group && (
                    <span className="row-actions always">
                      {stopped.length > 0 && (
                        <IconButton
                          icon="play"
                          label={`Start all in ${group}`}
                          onClick={() => act(stopped.map((c) => c.Id), (id) => api.containerAction(distro, id, "start"))}
                        />
                      )}
                      {running.length > 0 && (
                        <IconButton
                          icon="stop"
                          label={`Stop all in ${group}`}
                          onClick={() => act(running.map((c) => c.Id), (id) => api.containerAction(distro, id, "stop"))}
                        />
                      )}
                    </span>
                  )}
                </div>

                {items.map((c) => {
                  const up = isUp(c);
                  const waiting = pending.has(c.Id);
                  return (
                    <div
                      key={c.Id}
                      className={"row" + (selected === c.Id ? " selected" : "")}
                      onClick={() => setSelected(selected === c.Id ? null : c.Id)}
                    >
                      <span className={"dot " + (waiting ? "pending" : up ? "on" : "off")} />
                      <div className="row-main">
                        <div className="row-title">{c.Labels["com.docker.compose.service"] ?? name(c)}</div>
                        <div className="row-sub">{c.Image}</div>
                      </div>
                      <div className="ports">
                        {up &&
                          publishedPorts(c).map((p) => (
                            <button
                              key={p}
                              className="port"
                              title={`Open http://localhost:${p}`}
                              onClick={(e) => {
                                e.stopPropagation();
                                api.openInBrowser(`http://localhost:${p}`);
                              }}
                            >
                              :{p}
                            </button>
                          ))}
                      </div>
                      {/* Always there, so rows line up whether or not a container is running. */}
                      <div className="usage">
                        {usage[c.Id] && (
                          <>
                            <span title="CPU, in percent of one core">
                              {usage[c.Id].cpu === null ? "" : percent(usage[c.Id].cpu!)}
                            </span>
                            <span title="Memory in use">{formatBytes(usage[c.Id].memory)}</span>
                          </>
                        )}
                      </div>
                      <div className="row-status muted">{c.Status}</div>
                      <div className="row-actions">
                        {up ? (
                          <>
                            <IconButton
                              icon="terminal"
                              label="Open shell"
                              onClick={() => api.openTerminal(distro, c.Id).catch((e) => props.onError(api.errorText(e)))}
                            />
                            <IconButton
                              icon="restart"
                              label="Restart"
                              disabled={waiting}
                              onClick={() => act([c.Id], (id) => api.containerAction(distro, id, "restart"))}
                            />
                            <IconButton
                              icon="stop"
                              label="Stop"
                              disabled={waiting}
                              onClick={() => act([c.Id], (id) => api.containerAction(distro, id, "stop"))}
                            />
                          </>
                        ) : (
                          <IconButton
                            icon="play"
                            label="Start"
                            disabled={waiting}
                            onClick={() => act([c.Id], (id) => api.containerAction(distro, id, "start"))}
                          />
                        )}
                        <ConfirmButton label="Delete container" onConfirm={() => act([c.Id], (id) => api.removeContainer(distro, id))} />
                      </div>
                    </div>
                  );
                })}
              </section>
            );
          })}
        </div>

        {selectedContainer && (
          <Logs
            key={selectedContainer.Id}
            distro={distro}
            id={selectedContainer.Id}
            title={name(selectedContainer)}
            layout={logsLayout}
            onLayout={setLogsLayout}
            onClose={() => setSelected(null)}
          />
        )}
      </div>
    </div>
  );
}

import { Channel, invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import * as demo from "./demo";

export interface Status {
  distros: string[];
  distro: string | null;
  distroRunning: boolean;
  engineRunning: boolean;
  /** Pier let go of an idle engine and is bringing it back. */
  sleeping: boolean;
  /** Never let go of an idle engine (the Settings switch). */
  keepRunning: boolean;
  /** The app starts, in the tray, when the user signs in to Windows. */
  startOnLogin: boolean;
  version: string | null;
  pipe: string;
}

export interface Port {
  PrivatePort: number;
  PublicPort?: number;
  Type: string;
}

export interface Container {
  Id: string;
  Names: string[];
  Image: string;
  State: string; // running | exited | paused | created | restarting | dead
  Status: string; // "Up 3 hours", "Exited (0) 2 days ago"
  Ports: Port[];
  Labels: Record<string, string>;
}

export interface Image {
  Id: string;
  RepoTags: string[] | null;
  Size: number;
  Created: number;
  Containers: number;
}

export interface Volume {
  Name: string;
  Driver: string;
  CreatedAt?: string;
  Labels: Record<string, string> | null;
}

/** Outside the Tauri shell (plain `npm run dev`) the UI runs on sample data. */
const live = isTauri();

/**
 * Which window this page is in. The backend sets it when it creates the
 * window, and it goes along with every request for something that keeps
 * running (logs, a pull): the backend ends those when their window closes,
 * and a request that arrives after that must not be taken for one of the
 * next window's.
 */
const windowNumber = (window as unknown as { __PIER_WINDOW__?: number }).__PIER_WINDOW__ ?? 0;

export const getStatus = () => (live ? invoke<Status>("status") : demo.status());
export const setDistro = (name: string) => (live ? invoke<void>("set_distro", { name }) : demo.setDistro(name));
export const setStartOnLogin = (enabled: boolean) =>
  live ? invoke<void>("set_start_on_login", { enabled }) : demo.setStartOnLogin(enabled);
export const setKeepRunning = (keep: boolean) =>
  live ? invoke<void>("set_keep_running", { keep }) : demo.setKeepRunning(keep);
/** Starts the engine in `distro`: the one on screen when Start was pressed, or nothing. */
export const startEngine = (distro: string) =>
  live ? invoke<void>("engine_start", { distro }) : demo.setEngine(true);
// Everything below that touches Docker takes the distro it is meant for as
// its first argument: the one whose rows the user is looking at. The backend
// refuses the call if another distro has been selected since, so a click on a
// row can never act on a different distro's container, image or volume.

export const stopEngine = (distro: string, shutdownDistro = false) =>
  live ? invoke<void>("engine_stop", { distro, shutdownDistro }) : demo.setEngine(false);

export function docker<T = unknown>(distro: string, method: string, path: string, body?: unknown): Promise<T> {
  return live ? invoke<T>("docker_api", { distro, method, path, body }) : (demo.docker(distro, method, path) as Promise<T>);
}

/** The engine sends null instead of an empty list or map (e.g. `Ports` of a container that publishes none). */
export const listContainers = (distro: string) =>
  docker<Container[]>(distro, "GET", "/containers/json?all=true").then((list) =>
    list.map((c) => ({ ...c, Names: c.Names ?? [], Ports: c.Ports ?? [], Labels: c.Labels ?? {} })),
  );
export const listImages = (distro: string) => docker<Image[]>(distro, "GET", "/images/json");
export const listVolumes = (distro: string) =>
  docker<{ Volumes: Volume[] | null }>(distro, "GET", "/volumes").then((r) => r.Volumes ?? []);

/** A container's counters at one moment. Two of them give a CPU percentage. */
export interface UsageSample {
  /** CPU time the container has used so far, ns. */
  cpu: number;
  /** CPU time the whole machine has used so far, ns (0 if the engine does not say). */
  system: number;
  cpus: number;
  /** Memory in use, bytes, without the file cache the kernel can drop. */
  memory: number;
  /** When the sample was taken, ms. */
  at: number;
}

interface RawStats {
  read?: string;
  cpu_stats?: { cpu_usage?: { total_usage?: number }; system_cpu_usage?: number; online_cpus?: number };
  memory_stats?: { usage?: number; stats?: { inactive_file?: number } };
}

/**
 * One sample for each of `ids` (those that could not be read are left out).
 * All of them come over a single connection, so this costs one relay however
 * many containers there are.
 */
export async function sampleUsage(distro: string, ids: string[]): Promise<Record<string, UsageSample>> {
  const raw = live
    ? await invoke<(RawStats | null)[]>("container_stats", { distro, ids })
    : ((await demo.containerStats(distro, ids)) as (RawStats | null)[]);
  const samples: Record<string, UsageSample> = {};
  raw.forEach((s, i) => {
    if (!s?.cpu_stats?.cpu_usage) return;
    samples[ids[i]] = {
      cpu: s.cpu_stats.cpu_usage.total_usage ?? 0,
      system: s.cpu_stats.system_cpu_usage ?? 0,
      cpus: s.cpu_stats.online_cpus || 1,
      memory: Math.max(0, (s.memory_stats?.usage ?? 0) - (s.memory_stats?.stats?.inactive_file ?? 0)),
      at: s.read ? Date.parse(s.read) : Date.now(),
    };
  });
  return samples;
}

/**
 * CPU use between two samples, in percent of one core (so 200 is two cores
 * flat out), the way `docker stats` shows it.
 */
export function cpuPercent(before: UsageSample, now: UsageSample) {
  const used = now.cpu - before.cpu;
  if (used <= 0) return 0;
  const machine = now.system - before.system;
  if (machine > 0) return (used / machine) * now.cpus * 100;
  // No machine-wide counter: fall back to the clock.
  const elapsed = (now.at - before.at) * 1e6;
  return elapsed > 0 ? (used / elapsed) * 100 : 0;
}

/** Everything the engine knows about one container (`docker inspect`). */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type Inspect = Record<string, any>;
export const inspectContainer = (distro: string, id: string) => docker<Inspect>(distro, "GET", `/containers/${id}/json`);

export const containerAction = (distro: string, id: string, action: "start" | "stop" | "restart") =>
  docker(distro, "POST", `/containers/${id}/${action}`);
export const removeContainer = (distro: string, id: string) => docker(distro, "DELETE", `/containers/${id}?force=true`);
export const removeImage = (distro: string, id: string) => docker(distro, "DELETE", `/images/${encodeURIComponent(id)}`);
export const removeVolume = (distro: string, name: string) =>
  docker(distro, "DELETE", `/volumes/${encodeURIComponent(name)}`);

/**
 * Does the same thing to several items, four at a time, so a big Compose
 * project or a long selection does not start one relay process per item all
 * at once. Every item is tried; if any fail the promise rejects with how many
 * and the first reason (an image still in use, for example).
 */
export async function runEach(ids: string[], action: (id: string) => Promise<unknown>, failed = "failed") {
  const failures: unknown[] = [];
  let next = 0;
  const worker = async () => {
    while (next < ids.length) {
      const id = ids[next++];
      await action(id).catch((e) => failures.push(e));
    }
  };
  await Promise.all(Array.from({ length: Math.min(4, ids.length) }, worker));
  if (failures.length > 0) {
    throw ids.length === 1
      ? errorText(failures[0])
      : `${failures.length} of ${ids.length} ${failed}: ${errorText(failures[0])}`;
  }
}

const filters = (f: object) => encodeURIComponent(JSON.stringify(f));
/** Removes every image not used by a container, not only dangling layers. */
export const pruneImages = (distro: string) =>
  docker(distro, "POST", `/images/prune?filters=${filters({ dangling: ["false"] })}`);
export const pruneVolumes = (distro: string) =>
  docker(distro, "POST", `/volumes/prune?filters=${filters({ all: ["true"] })}`);

/** Follows a container's logs. Returns a function that stops the stream. */
///
/// `onText` gets each batch together with `done`, which it must call once the
/// batch is on screen: the backend sends the next batch only after that, so a
/// window that draws slowly is never handed more than it has asked for.
export async function followLogs(
  distro: string,
  id: string,
  onText: (text: string, done: () => void) => void,
  tail = 500,
) {
  if (!live) return demo.followLogs((text) => onText(text, () => {}));
  // Each batch has a number; confirming means sending that number back. A
  // batch can arrive before the stream's id does, so its confirmation waits.
  let streamId: number | undefined;
  let owed: number | undefined;
  const confirm = (seq: number) => {
    if (streamId === undefined) owed = seq;
    else void invoke("ack_logs", { streamId, seq });
  };
  const onChunk = new Channel<{ seq: number; text: string }>();
  onChunk.onmessage = (batch) => onText(batch.text, () => confirm(batch.seq));
  streamId = await invoke<number>("stream_logs", { window: windowNumber, distro, id, tail, onChunk });
  if (owed !== undefined) confirm(owed);
  return () => void invoke("stop_stream", { streamId });
}

/** How far a pull has got, summed over its layers. */
export interface PullProgress {
  layers: number;
  /** Layers downloaded and unpacked, or there already. */
  done: number;
  /** Bytes downloaded, and the size of the layers whose size is known. */
  received: number;
  total: number;
}

type PullEvent =
  | ({ kind: "progress" } & PullProgress)
  | { kind: "done"; message: string }
  | { kind: "failed"; message: string };

export interface Pull {
  /** Docker's closing line ("Downloaded newer image for ..."), or the reason it failed. */
  finished: Promise<string>;
  /** Abandons the pull; `finished` then fails with "Cancelled". */
  cancel: () => void;
}

/**
 * Pulls `image` (a bare name means `:latest`) from its registry. Public
 * images only. Fails at once if Docker refuses the name.
 */
export async function pullImage(distro: string, image: string, onProgress: (p: PullProgress) => void): Promise<Pull> {
  if (!live) return demo.pullImage(distro, image, onProgress);
  let finish!: (message: string) => void;
  let fail!: (reason: string) => void;
  const finished = new Promise<string>((resolve, reject) => {
    finish = resolve;
    fail = reject;
  });
  const onEvent = new Channel<PullEvent>();
  onEvent.onmessage = (e) => (e.kind === "progress" ? onProgress(e) : e.kind === "done" ? finish(e.message) : fail(e.message));
  const streamId = await invoke<number>("pull_image", { window: windowNumber, distro, image, onEvent });
  const stop = () => void invoke("stop_stream", { streamId });
  finished.then(stop, stop);
  return { finished, cancel: () => fail("Cancelled") };
}

/** Where docker commands typed on Windows go. */
export interface DockerContext {
  /** docker.exe is installed (on PATH). */
  cli: boolean;
  /** The context in use, when the CLI would say. */
  current: string | null;
  /** That context is Pier's. */
  active: boolean;
  /** While Pier's is in use: the context switching back returns to, if one was remembered. */
  previous: string | null;
}
export const dockerContext = () => (live ? invoke<DockerContext>("docker_context") : demo.dockerContext());
/** Points docker commands on Windows at Pier (`true`), or back at what they used before. */
export const setDockerContext = (pier: boolean) =>
  live ? invoke<DockerContext>("set_docker_context", { pier }) : demo.setDockerContext(pier);

export const openTerminal = (distro: string, id: string) =>
  live ? invoke<void>("open_terminal", { distro, id }) : demo.ok();
export const openInBrowser = (url: string) => (live ? openUrl(url) : void window.open(url));

/** Fires whenever something changes in Docker (container started, image pulled...). */
export interface DockerEvent {
  type: string; // container | image | volume | network ...
  action: string; // start | die | destroy | pull ...
  id: string;
}

export function onDockerEvent(handler: (event: DockerEvent) => void): () => void {
  if (!live) return () => {};
  const unlisten = listen<DockerEvent>("docker-event", (e) => handler(e.payload));
  return () => void unlisten.then((off) => off());
}

/**
 * Reports whether anyone can see the window, so polling can pause while it is
 * minimised or hidden to the tray.
 */
export function onVisibility(handler: (visible: boolean) => void): () => void {
  const onChange = () => handler(document.visibilityState === "visible");
  document.addEventListener("visibilitychange", onChange);
  // Hiding to the tray does not always change document.visibilityState, so the backend says so too.
  const unlisten = live ? listen<boolean>("window-visible", (e) => handler(e.payload)) : null;
  return () => {
    document.removeEventListener("visibilitychange", onChange);
    void unlisten?.then((off) => off());
  };
}

/** True inside the desktop shell, where Pier draws its own title bar buttons. */
export const hasWindowControls = live;
export const minimizeWindow = () => (live ? getCurrentWindow().minimize() : demo.ok());
export const toggleMaximizeWindow = () => (live ? getCurrentWindow().toggleMaximize() : demo.ok());
/** Closing hides Pier to the tray; quitting is in the tray menu. */
export const closeWindow = () => (live ? getCurrentWindow().close() : demo.ok());

/** Calls `handler` now and whenever the window is maximised or restored. */
export function onMaximizedChange(handler: (maximized: boolean) => void): () => void {
  if (!live) return () => {};
  const win = getCurrentWindow();
  const check = () => void win.isMaximized().then(handler);
  check();
  const unlisten = win.onResized(check);
  return () => void unlisten.then((off) => off());
}

/**
 * Runs `task`, and if it fails runs it again after each of `waits` (ms), as
 * long as `wanted()` still holds. Gives the value, or the last error once the
 * tries are used up or the result is no longer wanted.
 */
export async function retrying<T>(
  task: () => Promise<T>,
  wanted: () => boolean,
  waits = [1000, 3000],
): Promise<{ ok: true; value: T } | { ok: false; error: unknown }> {
  for (let attempt = 0; ; attempt++) {
    try {
      return { ok: true, value: await task() };
    } catch (error) {
      if (attempt >= waits.length || !wanted()) return { ok: false, error };
      await new Promise((r) => setTimeout(r, waits[attempt]));
      if (!wanted()) return { ok: false, error };
    }
  }
}

export const errorText = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : String(e));

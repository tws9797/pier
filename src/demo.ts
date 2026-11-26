// Sample data so the UI can be worked on in a browser without Tauri or WSL.
import type { Container, Image, Status, Volume } from "./api";

let engine = true;
const now = Math.floor(Date.now() / 1000);

const c = (
  name: string,
  image: string,
  state: string,
  status: string,
  ports: number[] = [],
  project?: string,
): Container => ({
  Id: (name + "0".repeat(64)).replace(/[^a-z0-9]/g, "").slice(0, 64),
  Names: ["/" + name],
  Image: image,
  State: state,
  Status: status,
  Ports: ports.map((p) => ({ PrivatePort: p, PublicPort: p, Type: "tcp" })),
  Labels: project ? { "com.docker.compose.project": project, "com.docker.compose.service": name.split("-")[1] } : {},
});

let containers: Container[] = [
  c("shop-api-1", "php:8.5-fpm", "running", "Up 3 hours", [9000], "shop"),
  c("shop-nginx-1", "nginx:1.29-alpine", "running", "Up 3 hours", [8080], "shop"),
  c("shop-mysql-1", "mysql:8.4", "running", "Up 3 hours (healthy)", [3306], "shop"),
  c("shop-redis-1", "redis:8-alpine", "exited", "Exited (0) 2 hours ago", [], "shop"),
  c("blog-gateway-1", "blog/gateway:dev", "running", "Up 26 minutes", [8787], "blog"),
  c("blog-postgres-1", "postgres:18", "running", "Up 26 minutes", [5432], "blog"),
  c("mailer", "mailer/mailer:latest", "exited", "Exited (137) 4 days ago"),
  c("adminer", "adminer:5", "running", "Up 2 days", [8081]),
];

let images: Image[] = [
  ["php:8.5-fpm", 512e6, 3, 1],
  ["nginx:1.29-alpine", 52e6, 12, 1],
  ["mysql:8.4", 601e6, 20, 1],
  ["redis:8-alpine", 41e6, 9, 1],
  ["postgres:18", 438e6, 6, 1],
  ["blog/gateway:dev", 188e6, 0, 1],
  ["node:24-slim", 231e6, 31, 0],
  ["golang:1.26", 842e6, 45, 0],
].map(([tag, size, days, used], i) => ({
  Id: "sha256:" + (i + 10).toString(16).repeat(32),
  RepoTags: [tag as string],
  Size: size as number,
  Created: now - (days as number) * 86400 - 3600,
  Containers: used as number,
}));

let volumes: Volume[] = ["shop_mysql-data", "shop_redis-data", "blog_pgdata", "mailer-uploads"].map(
  (Name, i) => ({ Name, Driver: "local", CreatedAt: new Date((now - i * 9 * 86400) * 1000).toISOString(), Labels: null }),
);

const wait = <T>(v: T) => new Promise<T>((r) => setTimeout(() => r(v), 120));
export const ok = () => wait(undefined);

export const status = (): Promise<Status> =>
  wait({
    distros: ["Ubuntu-22.04", "Debian"],
    distro: selected,
    distroRunning: engine,
    engineRunning: engine,
    sleeping: false,
    keepRunning,
    startOnLogin,
    version: engine ? "29.8.2" : null,
    pipe: "\\\\.\\pipe\\pier_docker",
  });

export const setEngine = (on: boolean) => {
  engine = on;
  return ok();
};

let startOnLogin = false;
export const setStartOnLogin = (enabled: boolean) => {
  startOnLogin = enabled;
  return ok();
};

let keepRunning = false;
export const setKeepRunning = (keep: boolean) => {
  keepRunning = keep;
  return ok();
};

/** The sample data lives in Ubuntu-22.04; Debian is an engine with nothing in it. */
let selected = "Ubuntu-22.04";
export const setDistro = (name: string) => {
  selected = name;
  return ok();
};

export function docker(distro: string, method: string, path: string): Promise<unknown> {
  if (!engine) return Promise.reject("Docker engine is not reachable");
  // Like the real backend: a request for a distro that is no longer selected is refused.
  if (distro !== selected) return Promise.reject(`Nothing was done: this was meant for ${distro}`);
  if (selected === "Debian") return wait(path === "/volumes" ? { Volumes: [] } : path.endsWith("/json") || path.includes("/json?") ? [] : null);
  if (path.startsWith("/containers/json")) return wait(containers);
  if (path.startsWith("/images/json")) return wait(images);
  if (path === "/volumes") return wait({ Volumes: volumes });
  // Pruning takes a while on a real engine; keep that visible in the demo.
  if (path.startsWith("/images/prune")) {
    return new Promise((r) => setTimeout(r, 1500)).then(() => {
      images = images.filter((i) => i.Containers > 0);
      return null;
    });
  }
  if (path.startsWith("/volumes/prune")) return new Promise((r) => setTimeout(() => r(null), 1500));
  if (method === "DELETE" && path.startsWith("/images/")) {
    const id = decodeURIComponent(path.slice("/images/".length));
    if (images.find((i) => i.Id === id)?.Containers) return Promise.reject("conflict: image is being used by a container");
    images = images.filter((i) => i.Id !== id);
    return wait(null);
  }
  if (method === "DELETE" && path.startsWith("/volumes/")) {
    const name = decodeURIComponent(path.slice("/volumes/".length));
    volumes = volumes.filter((v) => v.Name !== name);
    return wait(null);
  }
  const inspected = path.match(/^\/containers\/([^/?]+)\/json$/);
  if (inspected) {
    const c = containers.find((x) => x.Id === inspected[1]);
    return c ? wait(inspect(c)) : Promise.reject("No such container");
  }
  const m = path.match(/^\/containers\/([^/?]+)(?:\/(start|stop|restart))?/);
  if (m) {
    if (method === "DELETE") containers = containers.filter((x) => x.Id !== m[1]);
    else
      containers = containers.map((x) =>
        x.Id !== m[1]
          ? x
          : m[2] === "stop"
            ? { ...x, State: "exited", Status: "Exited (0) just now" }
            : { ...x, State: "running", Status: "Up 1 second" },
      );
  }
  return wait(null);
}

/** A plausible `docker inspect` for a sample container. */
function inspect(c: Container) {
  const running = c.State === "running";
  const name = c.Names[0] ?? "/" + c.Id;
  const project = c.Labels["com.docker.compose.project"];
  return {
    Id: c.Id.repeat(6).slice(0, 64),
    Name: name,
    Created: "2026-11-20T08:14:03.512Z",
    Path: "docker-entrypoint.sh",
    Args: [c.Image.split(":")[0].split("/").pop() ?? "app"],
    State: {
      Status: c.State,
      Running: running,
      StartedAt: "2026-11-25T06:02:11.004Z",
      FinishedAt: running ? "0001-01-01T00:00:00Z" : "2026-11-25T09:40:00.000Z",
      ExitCode: running ? 0 : 137,
      Health: c.Status.includes("healthy") ? { Status: "healthy" } : undefined,
    },
    Config: {
      Image: c.Image,
      Env: ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "TZ=UTC", "APP_ENV=local"],
      Labels: c.Labels,
      WorkingDir: "/app",
    },
    HostConfig: { RestartPolicy: { Name: project ? "unless-stopped" : "no" } },
    Mounts: project
      ? [{ Type: "volume", Name: `${project}_data`, Source: `/var/lib/docker/volumes/${project}_data/_data`, Destination: "/data", RW: true }]
      : [],
    NetworkSettings: {
      Ports: Object.fromEntries(
        c.Ports.map((p) => [`${p.PrivatePort}/${p.Type}`, p.PublicPort ? [{ HostIp: "0.0.0.0", HostPort: String(p.PublicPort) }] : null]),
      ),
      Networks: { [project ? `${project}_default` : "bridge"]: { IPAddress: running ? "172.18.0.3" : "" } },
    },
  };
}

/** Counters that grow like a real engine's, so the CPU percentage has something to work on. */
const used: Record<string, number> = {};
let machine = 0;
export function containerStats(distro: string, ids: string[]) {
  if (!engine || distro !== selected) return Promise.reject("Docker engine is not reachable");
  machine += 3e9 * 8; // three seconds on eight cores
  return wait(
    ids.map((id, i) => {
      const c = containers.find((x) => x.Id === id);
      if (!c || c.State !== "running") return null;
      const busy = ((i * 37) % 11) / 100 + Math.random() * 0.02; // share of one core
      used[id] = (used[id] ?? 0) + busy * 3e9;
      return {
        read: new Date().toISOString(),
        cpu_stats: { cpu_usage: { total_usage: used[id] }, system_cpu_usage: machine, online_cpus: 8 },
        memory_stats: { usage: (40 + ((i * 53) % 400)) * 1e6 + Math.random() * 2e6, stats: { inactive_file: 8e6 } },
      };
    }),
  );
}

export function followLogs(onText: (t: string) => void) {
  const lines = [
    '172.18.0.1 - - "GET /health HTTP/1.1" 200 2',
    "[info] worker 12 accepted connection",
    '172.18.0.1 - - "POST /api/v2/orders HTTP/1.1" 201 418',
    "\x1b[33m[warn]\x1b[0m slow query (212 ms): SELECT * FROM products WHERE sku = ?",
  ];
  onText(Array.from({ length: 30 }, (_, i) => lines[i % lines.length]).join("\n") + "\n");
  const t = setInterval(() => onText(lines[Math.floor(Math.random() * lines.length)] + "\n"), 1200);
  return () => clearInterval(t);
}

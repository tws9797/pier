import { useEffect, useState, type ReactNode } from "react";
import * as api from "./api";
import { timeAgo } from "./ui";

/** A time from the engine, or nothing for its "never" value (year 1). */
function when(value: unknown) {
  const date = typeof value === "string" ? new Date(value) : null;
  if (!date || isNaN(date.getTime()) || date.getFullYear() < 1971) return null;
  return `${timeAgo(date)} (${date.toLocaleString()})`;
}

function Section({ title, empty, children }: { title: string; empty?: string; children?: ReactNode }) {
  const has = Array.isArray(children) ? children.some(Boolean) : Boolean(children);
  return (
    <section>
      <h3>{title}</h3>
      {has ? <dl>{children}</dl> : <p className="muted">{empty ?? "None"}</p>}
    </section>
  );
}

function Field({ name, children }: { name: string; children: ReactNode }) {
  if (children === null || children === undefined || children === "") return null;
  return (
    <>
      <dt>{name}</dt>
      <dd>{children}</dd>
    </>
  );
}

/** What `docker inspect` says about a container, laid out for reading. */
export function Inspect({ distro, id }: { distro: string; id: string }) {
  const [info, setInfo] = useState<api.Inspect | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api.inspectContainer(distro, id).then(
        (i) => !cancelled && (setInfo(i), setError(null)),
        (e) => !cancelled && setError(api.errorText(e)),
      );
    load();
    // Its state changes when it starts, stops or is renamed.
    const off = api.onDockerEvent((e) => e.type === "container" && e.id === id && load());
    return () => {
      cancelled = true;
      off();
    };
  }, [distro, id]);

  if (error) return <div className="inspect muted">{error}</div>;
  if (!info) return <div className="inspect muted">Loading…</div>;

  const state = info.State ?? {};
  const config = info.Config ?? {};
  const ports = Object.entries<{ HostIp: string; HostPort: string }[] | null>(info.NetworkSettings?.Ports ?? {});
  const networks = Object.entries<{ IPAddress?: string }>(info.NetworkSettings?.Networks ?? {});
  const mounts: { Type: string; Name?: string; Source: string; Destination: string; RW: boolean }[] = info.Mounts ?? [];
  const env: string[] = config.Env ?? [];
  const labels = Object.entries<string>(config.Labels ?? {});
  const command = [info.Path, ...(info.Args ?? [])].filter(Boolean).join(" ");
  const status = [state.Status, state.Health?.Status && `(${state.Health.Status})`].filter(Boolean).join(" ");

  return (
    <div className="inspect">
      <Section title="Container">
        <Field name="Name">{String(info.Name ?? "").replace(/^\//, "")}</Field>
        <Field name="ID">
          <span className="mono">{String(info.Id ?? "").slice(0, 12)}</span>
        </Field>
        <Field name="Image">{config.Image}</Field>
        <Field name="Command">
          <span className="mono">{command}</span>
        </Field>
        <Field name="Working dir">{config.WorkingDir}</Field>
        <Field name="Status">{status}</Field>
        <Field name="Created">{when(info.Created)}</Field>
        <Field name="Started">{when(state.StartedAt)}</Field>
        {!state.Running && <Field name="Finished">{when(state.FinishedAt)}</Field>}
        {!state.Running && <Field name="Exit code">{String(state.ExitCode ?? "")}</Field>}
        <Field name="Restart policy">{info.HostConfig?.RestartPolicy?.Name || "no"}</Field>
      </Section>

      <Section title="Ports" empty="No ports">
        {ports.map(([port, hosts]) => (
          <Field key={port} name={port}>
            {hosts?.length ? hosts.map((h) => `${h.HostIp || "0.0.0.0"}:${h.HostPort}`).join(", ") : "not published"}
          </Field>
        ))}
      </Section>

      <Section title="Mounts" empty="No mounts">
        {mounts.map((m) => (
          <Field key={m.Destination} name={m.Destination}>
            <span className="mono">{m.Type === "volume" ? (m.Name ?? m.Source) : m.Source}</span>
            <span className="muted">
              {" "}
              {m.Type}
              {m.RW ? "" : ", read-only"}
            </span>
          </Field>
        ))}
      </Section>

      <Section title="Networks" empty="No networks">
        {networks.map(([name, n]) => (
          <Field key={name} name={name}>
            {n.IPAddress || "no address"}
          </Field>
        ))}
      </Section>

      <Section title="Environment" empty="No variables">
        {env.map((line) => {
          const at = line.indexOf("=");
          return (
            <Field key={line} name={at < 0 ? line : line.slice(0, at)}>
              <span className="mono">{at < 0 ? " " : line.slice(at + 1) || " "}</span>
            </Field>
          );
        })}
      </Section>

      <Section title="Labels" empty="No labels">
        {labels.map(([name, value]) => (
          <Field key={name} name={name}>
            <span className="mono">{value || " "}</span>
          </Field>
        ))}
      </Section>

      <details>
        <summary>Raw JSON</summary>
        <pre>{JSON.stringify(info, null, 2)}</pre>
      </details>
    </div>
  );
}

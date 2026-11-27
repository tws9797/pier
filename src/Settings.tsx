import { useEffect, useState } from "react";
import * as api from "./api";
import type { Status } from "./api";
import { Check } from "./ui";

/** Amounts offered as a limit, in GB; those the PC does not have are left out. */
const LIMITS = [2, 4, 6, 8, 12, 16, 24, 32, 48, 64];

/**
 * The memory limit of WSL as a whole (`.wslconfig`). Not a setting of Pier's:
 * it is read from the file when the page opens and written there on change.
 */
function MemoryLimit({ onError }: { onError: (e: string) => void }) {
  const [memory, setMemory] = useState<api.WslMemory | null>(null);
  const [saving, setSaving] = useState(false);
  const [changed, setChanged] = useState(false);
  useEffect(() => {
    let wanted = true;
    api.wslMemory().then((m) => wanted && setMemory(m), (e) => wanted && onError(api.errorText(e)));
    return () => {
      wanted = false;
    };
  }, [onError]);
  if (!memory) return null;

  const offered = LIMITS.filter((gb) => gb < memory.totalGb);
  // A limit written by hand in another form ("6144MB") is shown as it is.
  const custom = memory.limit !== null && (memory.limitGb === null || !offered.includes(memory.limitGb));
  const value = memory.limit === null ? "none" : custom ? "custom" : String(memory.limitGb);
  const change = (choice: string) => {
    if (choice === "custom") return;
    setSaving(true);
    api
      .setWslMemory(choice === "none" ? null : Number(choice))
      .then((m) => {
        setMemory(m);
        setChanged(true);
      }, (e) => onError(api.errorText(e)))
      .finally(() => setSaving(false));
  };

  return (
    <section>
      <h2>Memory limit</h2>
      <p className="muted">
        WSL runs all distros in one virtual machine. Without a limit it may grow to half of this PC's memory (
        {Math.floor(memory.totalGb / 2)} of {memory.totalGb} GB) and is slow to give it back. The limit is for WSL as a
        whole, not only for Docker.
      </p>
      <select aria-label="Memory limit for WSL" value={value} disabled={saving} onChange={(e) => change(e.target.value)}>
        <option value="none">No limit</option>
        {custom && <option value="custom">{memory.limit}</option>}
        {offered.map((gb) => (
          <option key={gb} value={gb}>
            {gb} GB
          </option>
        ))}
      </select>
      <p className="muted note">
        {changed ? "Saved to .wslconfig in your user folder. " : ""}A change applies the next time WSL starts, which is
        once every distro has stopped. Running <code>wsl --shutdown</code> in PowerShell does that at once; it stops
        everything running in every distro.
      </p>
    </section>
  );
}

/**
 * Which version is running and whether a newer one is out. `update` is what
 * the app found when the window opened (at most one look a day); the button
 * looks again now.
 */
function Updates(props: {
  /** The version that is running. */
  version: string;
  update: api.UpdateInfo | null;
  onUpdate: (u: api.UpdateInfo) => void;
  onError: (e: string) => void;
}) {
  const { update, onUpdate, onError } = props;
  const [busy, setBusy] = useState<"checking" | "installing" | null>(null);
  const [checked, setChecked] = useState(false);
  const check = () => {
    setBusy("checking");
    api
      .checkUpdate(true)
      .then((u) => {
        onUpdate(u);
        setChecked(true);
      }, (e) => onError(api.errorText(e)))
      .finally(() => setBusy(null));
  };
  const install = () => {
    setBusy("installing");
    // On success Pier closes, so there is nothing to do afterwards.
    api.installUpdate().catch((e) => {
      onError(api.errorText(e));
      setBusy(null);
    });
  };
  return (
    <section>
      <h2>Updates</h2>
      <p className="muted">
        This is Pier {props.version}.{" "}
        {update?.available
          ? `Version ${update.available} is available.`
          : checked
            ? "It is the newest version."
            : "Pier looks for a newer version on GitHub when this window opens, at most once a day."}
      </p>
      {update?.available ? (
        <>
          <button className="btn primary" disabled={busy !== null} onClick={install}>
            {busy === "installing" ? "Installing…" : `Install ${update.available} and restart`}
          </button>
          <p className="muted note">
            Pier closes while the update installs and starts again afterwards. If only Pier is keeping the distro awake,
            WSL may shut it down in the meantime, which stops running containers.
          </p>
        </>
      ) : (
        <button className="btn" disabled={busy !== null} onClick={check}>
          {busy === "checking" ? "Looking…" : "Look for updates now"}
        </button>
      )}
    </section>
  );
}

export function Settings(props: {
  status: Status | null;
  update: api.UpdateInfo | null;
  onUpdate: (u: api.UpdateInfo) => void;
  onChanged: () => void;
  onShutdown: () => void;
  onError: (e: string) => void;
}) {
  const { status } = props;
  const [copied, setCopied] = useState(false);
  // Asked once when the page opens: finding out runs docker.exe. Until the
  // answer is in, the command to copy is shown, which is always right.
  const [context, setContext] = useState<api.DockerContext | null>(null);
  const [switching, setSwitching] = useState(false);
  useEffect(() => {
    let wanted = true;
    api.dockerContext().then((c) => wanted && setContext(c), () => {});
    return () => {
      wanted = false;
    };
  }, []);
  const { onError } = props;
  const switchContext = (pier: boolean) => {
    setSwitching(true);
    api
      .setDockerContext(pier)
      .then(setContext, (e) => onError(api.errorText(e)))
      .finally(() => setSwitching(false));
  };
  if (!status) return null;

  const pipeUrl = "npipe://" + status.pipe.replace(/\\/g, "/");
  const contextCommand = `docker context create pier --docker "host=${pipeUrl}"; docker context use pier`;

  return (
    <div className="pane">
      <header className="toolbar">
        <h1>Settings</h1>
      </header>
      <div className="scroll settings">
        <section>
          <h2>WSL distro</h2>
          <p className="muted">Pier manages the Docker Engine installed inside this distro.</p>
          <select
            value={status.distro ?? ""}
            onChange={(e) =>
              api
                .setDistro(e.target.value)
                .catch((err) => props.onError(api.errorText(err)))
                // The switch happens even if it could not be saved, so always read it back.
                .finally(props.onChanged)
            }
          >
            {status.distros.map((d) => (
              <option key={d}>{d}</option>
            ))}
          </select>
        </section>

        <section>
          <h2>Use docker from Windows</h2>
          <p className="muted">
            While Pier is running, the engine is also reachable from PowerShell, docker.exe and editors through a named
            pipe.{" "}
            {context?.cli === false
              ? "docker.exe was not found on PATH, and Pier does not include one. Install it (winget install Docker.DockerCLI), then run this:"
              : ""}
            {context?.cli &&
              context.active &&
              `docker commands on Windows go to Pier now. Switching back returns them to the "${context.previous ?? "default"}" context.`}
            {context?.cli &&
              !context.active &&
              (context.current
                ? `docker commands on Windows go to the "${context.current}" context now.`
                : "docker commands on Windows do not go to Pier yet.")}
          </p>
          {context?.cli ? (
            <button className="btn" disabled={switching} onClick={() => switchContext(!context.active)}>
              {context.active ? "Stop using Pier for docker commands" : "Use Pier for docker commands"}
            </button>
          ) : (
            <div className="code">
              <code>{contextCommand}</code>
              <button
                className="btn small"
                onClick={() =>
                  navigator.clipboard.writeText(contextCommand).then(() => {
                    setCopied(true);
                    setTimeout(() => setCopied(false), 1500);
                  })
                }
              >
                {copied ? "Copied" : "Copy"}
              </button>
            </div>
          )}
        </section>

        <section>
          <h2>Start-up</h2>
          <label className="option">
            <Check
              label="Start when I sign in to Windows"
              checked={status.startOnLogin}
              onChange={() =>
                api
                  .setStartOnLogin(!status.startOnLogin)
                  .catch((err) => props.onError(api.errorText(err)))
                  .finally(props.onChanged)
              }
            />
            <span>Start when I sign in to Windows</span>
          </label>
          <p className="muted">
            Starts in the tray, without this window, so docker commands work from the moment you sign in. Click the tray
            icon to open the window.
          </p>
        </section>

        <section>
          <h2>When idle</h2>
          <label className="option">
            <Check
              label="Keep the engine running when idle"
              checked={status.keepRunning}
              onChange={() =>
                api
                  .setKeepRunning(!status.keepRunning)
                  .catch((err) => props.onError(api.errorText(err)))
                  // Read the setting back either way: if it could not be saved it was not applied.
                  .finally(props.onChanged)
              }
            />
            <span>Keep the engine running when nothing is using it</span>
          </label>
          <p className="muted">
            {status.keepRunning
              ? `Pier keeps ${status.distro ?? "the distro"} awake until you stop the engine, so docker commands never wait for it to start.`
              : `After 5 minutes with this window closed, no containers running and no docker command connected, Pier lets go of ${status.distro ?? "the distro"}, and WSL shuts it down to free its memory once nothing else is using it. The next docker command, or opening this window, starts it again.`}
          </p>
          {!status.keepRunning && (
            <p className="muted">
              Pier never stops Docker by itself, so a docker build or pull started in a shell inside the distro is not
              interrupted. On a distro without systemd WSL cannot shut the distro down while Docker runs: use Stop
              engine to free its memory there.
            </p>
          )}
        </section>

        <MemoryLimit onError={onError} />

        <section>
          <h2>Free memory</h2>
          <p className="muted">
            Stops Docker and shuts the distro down, which returns its memory to Windows. Anything else running in{" "}
            {status.distro ?? "the distro"} is stopped too.
          </p>
          <button className="btn" disabled={!status.distroRunning} onClick={props.onShutdown}>
            Stop engine and shut down distro
          </button>
        </section>

        <Updates version={status.appVersion} update={props.update} onUpdate={props.onUpdate} onError={onError} />
      </div>
    </div>
  );
}

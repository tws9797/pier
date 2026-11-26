import { useEffect, useState } from "react";
import * as api from "./api";
import type { Status } from "./api";
import { Check } from "./ui";

export function Settings(props: {
  status: Status | null;
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
      </div>
    </div>
  );
}

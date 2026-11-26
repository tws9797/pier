import { useState } from "react";
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
            While Pier is open, the engine is also reachable from PowerShell, docker.exe and editors through a named
            pipe. Run this once:
          </p>
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
              : `After 5 minutes with this window closed, no containers running and no docker command connected, Pier lets WSL shut ${status.distro ?? "the distro"} down to free its memory. The next docker command, or opening this window, starts it again.`}
          </p>
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

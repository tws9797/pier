import { useState } from "react";
import * as api from "./api";
import type { Container, Image, Volume } from "./api";
import { PullBar, usePulling } from "./Pull";
import { Check, ConfirmButton, ConfirmTextButton, Empty, formatBytes, timeAgo, useSelection } from "./ui";

type Run = (action: () => Promise<unknown>) => Promise<void>;
type Selection = ReturnType<typeof useSelection>;

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;

/**
 * The right-hand side of a list toolbar: "Remove unused" normally, and the
 * bulk delete for the ticked rows once any are ticked.
 */
function BulkActions(props: {
  selection: Selection;
  noun: string;
  remove: (id: string) => Promise<unknown>;
  run: Run;
  prune: React.ReactNode;
}) {
  const { selection, noun } = props;
  const count = selection.selected.size;
  if (count === 0) return <>{props.prune}</>;
  return (
    <>
      <button className="btn" onClick={selection.clear}>
        Clear
      </button>
      <ConfirmTextButton
        key="bulk"
        text={`Delete ${plural(count, noun)}`}
        confirmText={`Click again to delete ${plural(count, noun)}`}
        busyText={`Deleting ${plural(count, noun)}…`}
        onConfirm={() => props.run(() => api.runEach([...selection.selected], props.remove, "could not be deleted"))}
      />
    </>
  );
}

// `distro` is the one these rows were loaded from; every action names it, and
// the backend refuses the action if another distro has been selected since.

export function Images({ distro, images, run }: { distro: string; images: Image[]; run: Run }) {
  const sorted = [...images].sort((a, b) => b.Created - a.Created);
  const total = images.reduce((sum, i) => sum + i.Size, 0);
  const unused = images.filter((i) => i.Containers === 0).length;
  const selection = useSelection(sorted.map((i) => i.Id));
  // The strip is open while the user is typing a name, and for as long as a
  // pull runs, whichever page it was started from.
  const [asking, setAsking] = useState(false);
  const pullOpen = usePulling() || asking;

  return (
    <div className="pane">
      <header className="toolbar">
        <Check
          label="Select all images"
          checked={selection.all}
          partial={selection.some}
          onChange={selection.toggleAll}
        />
        <h1>Images</h1>
        <span className="muted">
          {selection.selected.size > 0
            ? `${selection.selected.size} of ${images.length} selected`
            : `${images.length} images, ${formatBytes(total)}`}
        </span>
        <span className="spacer" />
        {!pullOpen && (
          <button className="btn" onClick={() => setAsking(true)}>
            Pull image
          </button>
        )}
        <BulkActions
          selection={selection}
          noun="image"
          remove={(id) => api.removeImage(distro, id)}
          run={run}
          prune={
            <ConfirmTextButton
              key="prune"
              text={`Remove unused${unused > 0 ? ` (${unused})` : ""}`}
              confirmText={`Click again to delete ${plural(unused, "image")}`}
              busyText="Removing images…"
              disabled={unused === 0}
              onConfirm={() => run(() => api.pruneImages(distro))}
            />
          }
        />
      </header>
      {pullOpen && <PullBar distro={distro} run={run} onClose={() => setAsking(false)} />}
      <div className="scroll">
        {images.length === 0 && <Empty title="No images" hint="Pulled and built images appear here." />}
        {sorted.map((img) => {
          const tag = img.RepoTags?.[0] ?? "<untagged>";
          const ticked = selection.selected.has(img.Id);
          return (
            <div
              key={img.Id}
              className={"row pick" + (ticked ? " selected" : "")}
              onClick={(e) => selection.toggle(img.Id, e.shiftKey)}
            >
              <Check label={`Select ${tag}`} checked={ticked} onChange={(shift) => selection.toggle(img.Id, shift)} />
              <div className="row-main">
                <div className="row-title">{tag}</div>
                <div className="row-sub mono">{img.Id.replace("sha256:", "").slice(0, 12)}</div>
              </div>
              {img.Containers > 0 && <span className="badge">in use</span>}
              <div className="cell muted">{timeAgo(new Date(img.Created * 1000))}</div>
              <div className="cell num">{formatBytes(img.Size)}</div>
              <div className="row-actions">
                <ConfirmButton label="Delete image" onConfirm={() => run(() => api.removeImage(distro, img.Id))} />
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

export function Volumes(props: { distro: string; volumes: Volume[]; containers: Container[]; run: Run }) {
  const { distro, volumes, containers, run } = props;
  const sorted = [...volumes].sort((a, b) => a.Name.localeCompare(b.Name));
  // Compose names volumes "<project>_<volume>"; use that to hint at the owner.
  const projects = new Set(containers.map((c) => c.Labels["com.docker.compose.project"]).filter(Boolean));
  const selection = useSelection(sorted.map((v) => v.Name));

  return (
    <div className="pane">
      <header className="toolbar">
        <Check
          label="Select all volumes"
          checked={selection.all}
          partial={selection.some}
          onChange={selection.toggleAll}
        />
        <h1>Volumes</h1>
        <span className="muted">
          {selection.selected.size > 0
            ? `${selection.selected.size} of ${volumes.length} selected`
            : `${volumes.length} volumes`}
        </span>
        <span className="spacer" />
        <BulkActions
          selection={selection}
          noun="volume"
          remove={(name) => api.removeVolume(distro, name)}
          run={run}
          prune={
            <ConfirmTextButton
              key="prune"
              text="Remove unused"
              confirmText="Click again to delete unused volumes and their data"
              busyText="Removing volumes…"
              disabled={volumes.length === 0}
              onConfirm={() => run(() => api.pruneVolumes(distro))}
            />
          }
        />
      </header>
      <div className="scroll">
        {volumes.length === 0 && <Empty title="No volumes" hint="Named volumes created by your containers appear here." />}
        {sorted.map((v) => {
          const owner = v.Labels?.["com.docker.compose.project"] ?? [...projects].find((p) => v.Name.startsWith(p + "_"));
          const ticked = selection.selected.has(v.Name);
          return (
            <div
              key={v.Name}
              className={"row pick" + (ticked ? " selected" : "")}
              onClick={(e) => selection.toggle(v.Name, e.shiftKey)}
            >
              <Check label={`Select ${v.Name}`} checked={ticked} onChange={(shift) => selection.toggle(v.Name, shift)} />
              <div className="row-main">
                <div className="row-title">{v.Name}</div>
                <div className="row-sub">{owner ? `${owner} · ${v.Driver}` : v.Driver}</div>
              </div>
              <div className="cell muted">{v.CreatedAt ? timeAgo(new Date(v.CreatedAt)) : ""}</div>
              <div className="row-actions">
                <ConfirmButton label="Delete volume" onConfirm={() => run(() => api.removeVolume(distro, v.Name))} />
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

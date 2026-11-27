# Pier

Lightweight Docker manager for Windows (OrbStack-like). Tauri 2 + React/TS.
It manages the Docker Engine inside an existing WSL2 distro; it ships no VM.
Read README.md for the architecture and the "Not done yet" list.

## Status

Runs on Windows. Until 2026-11-26 it was run day to day against Docker
Desktop's engine through its WSL integration in Ubuntu. Since then Ubuntu
(systemd) has its own docker-ce, and Pier manages that.

The parts that need an engine Pier owns (start, stop, idle sleep, wake on a
pipe connection) have had two real runs: one end to end against a throwaway
Alpine distro with its own dockerd, and one against the systemd-managed
docker-ce in Ubuntu (sleep after the idle time, wake from the pipe in 1.8 s,
wake from the window, no sleep while a container runs, Stop and Start
buttons). With Docker Desktop closed, the systemd distro shut down about
15 s after the sleep and a pipe connection brought it back, cold, in 5 s.
The `login -- user` processes WSL keeps in a systemd distro do not hold it
up; Docker Desktop's integration proxy does.

Pier never stops Docker by itself: going to sleep only lets go of Pier's
own session (`release_asleep` in engine.rs says why). Idle detection that
decides when to stop dockerd was tried in three forms and each left a
window in which a `docker build` or `docker pull` run inside the distro was
cut off; do not bring it back. Run for real on Ubuntu (systemd): a native
`docker pull` and a native `docker events` each ran across the moment Pier
went to sleep, untouched, and the distro shut down by itself afterwards.
On a distro without systemd dockerd keeps the distro up, so sleep gives no
memory back there until Stop is pressed; that case has not been run since
the change.

When testing sleep with a hidden Pier, make sure the distro is up before
Pier starts: started against a stopped distro it attaches to nothing and
"holds 0 wsl.exe" then looks like sleep.

Updating was run for real: a copy with a lower version number found the
published release, installed it and came back up as the new version in 5 s.
The first try (0.1.6) failed: the installer was a child of Pier, in the job
that ends Pier's children with it, and died when Pier closed.

Still only covered by unit tests with a stand-in backend (src-tauri/src/engine.rs):
two real distros at once (switching, background keep-alive), a Docker that
hangs. Clicking the tray icon was only simulated.

The state machine in engine.rs was hardened over many review rounds before it
had been run for real. The one real run then found three bugs no review had
(BusyBox `nc`, dockerd keeping a non-systemd distro up, leftover `wslhost.exe`).
Prefer a real run to another review round.

## Commands

- `npm run tauri dev`: run the app (Windows only)
- `npm run dev`: UI in a browser on sample data from src/demo.ts
- `npm run build`: type-check and build the frontend
- `cargo check` in src-tauri: check the backend
- `npm run tauri build -- --no-bundle`: the release program, no installer
- `.\scripts\release.ps1`: the signed installer and `latest.json` for a
  release. Publish both files with `gh release create`: installed copies
  find a new version only through `latest.json`. The signing key is in
  `%UserProfile%\.tauri\pier.key`, not in the repository; never print it,
  commit it or send it anywhere.

CI (`.github/workflows/ci.yml`) builds the frontend and runs `cargo test
--lib` on Windows for every push to main and every pull request.

Build and run from Windows (PowerShell), not from inside WSL: a build inside
WSL produces a Linux app that cannot call `wsl.exe` the same way.

## Conventions

- `src/api.ts` is the only frontend file that calls the backend. Every call
  has a demo fallback in `src/demo.ts`; keep them in step.
- Prefer the generic `docker_api` pass-through over new Rust commands. Add
  Rust only for streaming, WSL control or Windows integration.
- Pier does not stop Docker unless the user presses Stop. Nothing on a
  timer and no idle rule may stop dockerd or shut a distro down.
- Nothing that runs on a timer may boot a stopped distro. Check
  `wsl::running()` or the `engine_up` flag first.
- Every `wsl.exe` spawn goes through `wsl()` in wsl.rs (sets CREATE_NO_WINDOW
  and WSL_UTF8).
- UI: plain CSS variables in src/styles.css, no component library. Simple,
  dense, developer-oriented; rows must not shift on hover.

## Suggested next steps

Pier is meant to stay light. A feature earns its place if the command line
cannot do it (the engine's and WSL's life cycle) or if a glance beats
typing (lists, status, logs). The owner decided against a built-in
terminal on 2026-11-27: the shell button opens Windows Terminal, which does
the job better.

1. Use Pier on its own engine day to day, with Docker Desktop closed or its
   WSL integration for the distro turned off.
2. `name.pier.local` domains via a local DNS responder and reverse proxy.

Done since the first version: per-container CPU/memory (one kept connection,
`container_stats`), the Inspect tab, start at sign-in (`--hidden`, startup.rs),
the NSIS installer (`npm run tauri build`), pulling an image, the button
that sets up the Windows `docker` context, the WSL memory limit
(`.wslconfig`; writing the file was run for real, the limit taking effect
after a WSL restart was not).

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

Build and run from Windows (PowerShell), not from inside WSL: a build inside
WSL produces a Linux app that cannot call `wsl.exe` the same way.

## Conventions

- `src/api.ts` is the only frontend file that calls the backend. Every call
  has a demo fallback in `src/demo.ts`; keep them in step.
- Prefer the generic `docker_api` pass-through over new Rust commands. Add
  Rust only for streaming, WSL control or Windows integration.
- Nothing that runs on a timer may boot a stopped distro. Check
  `wsl::running()` or the `engine_up` flag first.
- Every `wsl.exe` spawn goes through `wsl()` in wsl.rs (sets CREATE_NO_WINDOW
  and WSL_UTF8).
- UI: plain CSS variables in src/styles.css, no component library. Simple,
  dense, developer-oriented; rows must not shift on hover.

## Suggested next steps

1. Use Pier on its own engine day to day, with Docker Desktop closed or its
   WSL integration for the distro turned off. Missing for that: pulling an
   image from the UI, and a button that sets up the Windows `docker` context.
2. Built-in terminal (xterm.js over `/exec` + hijacked connection).
3. `.wslconfig` memory settings.
4. `name.pier.local` domains via a local DNS responder and reverse proxy.

Done since the first version: per-container CPU/memory (one kept connection,
`container_stats`), the Inspect tab, start at sign-in (`--hidden`, startup.rs),
the NSIS installer (`npm run tauri build`).

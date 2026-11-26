# Pier

Lightweight Docker manager for Windows (OrbStack-like). Tauri 2 + React/TS.
It manages the Docker Engine inside an existing WSL2 distro; it ships no VM.
Read README.md for the architecture and the "Not done yet" list.

## Status

Runs on Windows. Day to day it has been run against Docker Desktop's engine
through its WSL integration in Ubuntu. The parts that need an engine Pier
owns (start, stop, idle sleep, wake on a pipe connection) were run once, end
to end, against a throwaway Alpine distro with its own dockerd.

Still only covered by unit tests with a stand-in backend (src-tauri/src/engine.rs):
two real distros at once (switching, background keep-alive), a Docker that
hangs, a systemd-managed dockerd. Clicking the tray icon was only simulated.

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

1. Install docker-ce in the WSL distro and run Pier against its own engine
   instead of Docker Desktop's. Everything about start, stop and sleep has
   only had one real run.
2. Built-in terminal (xterm.js over `/exec` + hijacked connection).
3. `.wslconfig` memory settings.
4. `name.pier.local` domains via a local DNS responder and reverse proxy.

Done since the first version: per-container CPU/memory (one kept connection,
`container_stats`), the Inspect tab, start at sign-in (`--hidden`, startup.rs),
the NSIS installer (`npm run tauri build`).

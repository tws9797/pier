# Pier

A small Docker manager for Windows. It does not ship its own VM: it drives the
Docker Engine that already lives in one of your WSL2 distros.

Working name; to rename, change `productName`/`identifier` in
`src-tauri/tauri.conf.json`, the pipe name in `src-tauri/src/bridge.rs` and the
brand in `src/App.tsx`.

## Prerequisites (Windows)

- Node 20+
- Rust: `winget install Rustlang.Rustup`, plus the "Desktop development with
  C++" workload of Visual Studio Build Tools
- A WSL2 distro with Docker Engine installed (`docker.io` or `docker-ce`)

## Run

```powershell
npm install
npm run tauri dev      # first build takes a few minutes
npm run tauri build    # installer in src-tauri\target\release\bundle\nsis
```

`npm run dev` alone opens the UI in a browser on sample data, which is the
quickest way to work on the frontend.

## How it works

```
React UI ── invoke ──► Rust core ── wsl.exe -d <distro> -u root ── nc -U /var/run/docker.sock
                          │
docker.exe ── \\.\pipe\pier_docker ──┘   (same relay, raw bytes)
```

- `src-tauri/src/wsl.rs`: everything that calls `wsl.exe`: listing distros,
  starting/stopping dockerd, the socket relay, the keep-alive process.
- `src-tauri/src/docker.rs`: HTTP/1.1 client for the Docker Engine API that
  runs over the relay's stdin/stdout.
- `src-tauri/src/bridge.rs`: named pipe so Windows-side tools can use the
  engine.
- `src-tauri/src/lib.rs`: Tauri commands, tray icon, Docker event forwarding.
- `src/`: the UI. `api.ts` is the only file that talks to the backend;
  `docker_api` is a generic pass-through, so most new features need no Rust.

Things worth knowing:

- Status polling never boots a stopped distro. Starting the engine, or a
  Windows tool connecting to the pipe, does. The exception is an engine you
  stopped yourself (Stop, or "shut down distro"): until it is started again
  the pipe closes connections unanswered, so an IDE probing in the background
  cannot bring the distro back.
- While the engine is in use Pier holds a `sleep infinity` session open,
  because WSL shuts a distro down soon after its last session ends. "In use"
  means the window is open, a Windows tool is connected to the pipe, or a
  container is running. After 5 minutes with none of those (`PIER_IDLE_SECS`
  overrides it) Pier lets go of the session and the event stream, so WSL can
  shut the distro down; your own sessions in that distro are left alone. The
  next pipe connection, or opening the window, starts the engine again. The
  rule lives in `src-tauri/src/engine.rs`. "Keep the engine running when
  nothing is using it" in Settings turns this off; the choice is saved in
  `config.json` as `keepRunning`.
- Pier never stops Docker by itself. Only Stop in the window (and "Stop
  engine and shut down distro") does. Going to sleep lets go of Pier's own
  WSL session and event stream and nothing else, because Pier cannot see
  Docker being used from inside the distro: a `docker build` or `docker
  pull` typed into a shell there is neither a container nor a pipe
  connection, it can connect at any moment, and stopping dockerd would cut
  it off.

  When memory comes back is therefore up to WSL, which shuts a distro down
  once nothing in it holds it open:
  - With systemd, that happens by itself shortly after Pier lets go
    (services, dockerd included, do not hold a distro open), unless you
    have a shell or a task of your own in the distro.
  - Without systemd, dockerd is an ordinary process and does hold the
    distro open. Nothing is given back until you press Stop, which frees
    the engine's memory; the rest follows when WSL shuts the distro down.
- Ending a `wsl.exe` also ends its `wslhost.exe` helper (`Session` in
  wsl.rs). WSL starts that helper to keep a console-less session open if
  wsl.exe goes away; left behind, it holds the distro up for ever. Pier's
  own process is in a job object, so its children end with it even on a
  crash.
- The relay into the distro is whichever of these it finds: `nc` if it
  supports unix sockets (netcat-openbsd; BusyBox's does not), `socat`,
  `docker system dial-stdio`, `python3`.
- Which distro the window shows and which distros Pier keeps awake are
  separate. Selecting another distro does not let go of the previous one if
  its engine was up and it may have containers running: it stays held in the
  background and is checked every 5 minutes, and let go once nothing runs
  there. Stopping it from Pier lets go of it at once.
- Docker not answering is not the same as Docker being gone. A probe that
  fails changes nothing; after three in a row, or five failed reconnects of
  the event stream, Pier looks for the `dockerd` process in the distro and
  lets go only if it is not there.
- Closing the window destroys it and its WebView2 processes; the tray icon,
  the bridge and the event watcher keep running in the Rust process. Clicking
  the tray icon creates the window again. Quit from the tray menu.
- The relay runs as root in the distro, so the pipe gives your Windows user
  full control of Docker, the same as Docker Desktop's pipe does.
- "Use Pier for docker commands" in Settings runs `docker context create
  pier` and `docker context use pier` for you (`src-tauri/src/context.rs`).
  The context in use before is remembered (in `docker-context` next to
  `config.json`) and is what "Stop using Pier" goes back to. It needs a
  docker.exe on PATH; Pier does not ship one (`winget install
  Docker.DockerCLI` installs the CLI on its own, without Docker Desktop).
- "Memory limit" in Settings is the `memory` line under `[wsl2]` in
  `%UserProfile%\.wslconfig` (`src-tauri/src/wslconfig.rs`). It is WSL's
  setting, for all distros together, not Pier's: Pier changes that one line
  and leaves the rest of the file alone. WSL reads it when its virtual
  machine starts, so a change applies once every distro has stopped; Pier
  does not run `wsl --shutdown` for you.
- "Pull image" pulls public images only: no registry credentials are sent.
  Stopping the request (Cancel, or closing the window) makes Docker abandon
  the pull.

## Not done yet

- Pulling from a private registry, building an image
- Stop does not stop containerd (about 75 MB with no containers, for as
  long as the distro stays up).
- `name.pier.local` domains with HTTPS
- `docker run -i` with piped stdin through the bridge (named pipes have no
  half-close; Docker's own pipe uses a zero-byte-write convention for it)

//! Everything that touches `wsl.exe`.
//!
//! On non-Windows hosts (useful for hacking on the UI from Linux/macOS) the
//! same functions talk to a local Docker socket instead, so the rest of the
//! app does not need to care.

use std::process::Stdio;
use tokio::process::Command;

/// Shell snippet that connects stdin/stdout to the Docker socket using
/// whichever tool the distro has. Ubuntu ships `nc` (netcat-openbsd) and
/// `python3` by default, so this works without installing anything.
///
/// Each tool is chosen by what it can do, not by its name: `nc` on Alpine
/// (and anything else BusyBox-based) is BusyBox's, which has no `-U` and
/// cannot open a unix socket at all. Picking it just because it is called
/// `nc` left Pier unable to reach an engine that was up and running. Where
/// nothing else fits, the Docker CLI itself is used: `docker system
/// dial-stdio` is a stdin/stdout relay to the daemon, and it is there
/// wherever Docker is installed.
const RELAY_SCRIPT: &str = r#"S=${PIER_DOCKER_SOCK:-/var/run/docker.sock}
if nc -h 2>&1 | grep -q -e '-U'; then exec nc -N -U "$S"
elif command -v socat >/dev/null 2>&1; then exec socat -t 3600 - "UNIX-CONNECT:$S"
elif command -v docker >/dev/null 2>&1; then exec docker -H "unix://$S" system dial-stdio
elif ! command -v python3 >/dev/null 2>&1; then
  echo "Pier found no way to reach $S: install netcat-openbsd, socat, python3 or the docker CLI in this distro" >&2; exit 127
else exec python3 -c '
import os, socket, sys, threading
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
def up():
    while True:
        d = os.read(0, 65536)
        if not d: break
        s.sendall(d)
    try: s.shutdown(socket.SHUT_WR)
    except OSError: pass
threading.Thread(target=up, daemon=True).start()
while True:
    d = s.recv(65536)
    if not d: break
    os.write(1, d)
' "$S"
fi"#;

/// Base `wsl.exe` command that never flashes a console window.
///
/// Every command is killed if the code waiting for it stops waiting (a
/// timeout, a cancelled request). Otherwise a start script that timed out
/// would keep running behind a lock that has already been released, and
/// collide with the stop or retry that follows.
fn wsl() -> Command {
    let mut cmd = Command::new(wsl_exe());
    cmd.kill_on_drop(true);
    cmd.env("WSL_UTF8", "1");
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// The WSL client to run. `wsl.exe` on the PATH is a small launcher in
/// System32 that starts the real client from Program Files; running the real
/// one directly saves a process per call and puts the session's helper one
/// step closer.
fn wsl_exe() -> &'static std::path::Path {
    static EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| {
        std::env::var_os("ProgramFiles")
            .map(|dir| std::path::Path::new(&dir).join("WSL").join("wsl.exe"))
            .filter(|real| cfg!(windows) && real.is_file())
            .unwrap_or_else(|| "wsl.exe".into())
    })
}

/// A running `wsl.exe` that is ended completely when dropped.
///
/// Killing wsl.exe alone is not the end of it. For a session with no console
/// WSL starts a helper, `wslhost.exe`, whose job is to keep the session open
/// if wsl.exe goes away, and it does exactly that when wsl.exe is killed: it
/// stays, and as long as it stays WSL counts the distro as in use and never
/// shuts it down. Every keep-alive and relay Pier "let go of" left one
/// behind, so a distro Pier had put to sleep stayed up for ever. (Found by
/// running a real sleep against a real distro; nothing short of that shows
/// it.) So the helpers are ended first, then wsl.exe.
pub struct Session(tokio::process::Child);

impl Session {
    fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        cmd.kill_on_drop(true).spawn().map(Self)
    }
}

impl std::ops::Deref for Session {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Only while it is still running: once it has exited its process id
        // may belong to something else.
        #[cfg(windows)]
        if let (Some(pid), Ok(None)) = (self.0.id(), self.0.try_wait()) {
            win::end_helpers_of(pid);
        }
        // kill_on_drop ends wsl.exe itself.
    }
}

/// Runs a command to its end and collects its output. If the caller stops
/// waiting (a timeout), the command and its helpers are ended, like a
/// `Session`.
async fn output(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    struct EndHelpers(Option<u32>);
    impl Drop for EndHelpers {
        fn drop(&mut self) {
            #[cfg(windows)]
            if let Some(pid) = self.0 {
                win::end_helpers_of(pid);
            }
        }
    }
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
    let mut still_running = EndHelpers(child.id());
    let out = child.wait_with_output().await;
    still_running.0 = None; // it ran to its end: nothing to clean up
    out
}

/// Makes every process Pier starts end when Pier does, however that
/// happens: quit, crash, or being killed from Task Manager. Without it each
/// such end left its `wsl.exe … sleep infinity` running with nobody to stop
/// it, holding the distro awake until the next reboot. Call once at startup.
pub fn end_children_with_pier() {
    #[cfg(windows)]
    win::end_children_with_this_process();
}

/// Called just before Pier starts the installer of a newer version and ends:
/// the installer has to outlive Pier, and a child born in the job would be
/// killed with it. Everything already running still ends with Pier.
/// Nothing else may be started after this.
pub fn let_the_installer_outlive_pier() {
    #[cfg(windows)]
    win::let_new_children_outlive_this_process();
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[repr(C)]
    struct ProcessEntry {
        size: u32,
        usage: u32,
        pid: u32,
        default_heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        priority: i32,
        flags: u32,
        exe: [u16; 260],
    }

    /// A creation time. `high` comes first so that the derived ordering is by time.
    #[derive(Clone, Copy, PartialEq, PartialOrd)]
    struct FileTime {
        high: u32,
        low: u32,
    }

    #[repr(C)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn TerminateProcess(process: Handle, code: u32) -> i32;
        fn GetProcessTimes(process: Handle, created: *mut [u32; 2], exited: *mut [u32; 2], kernel: *mut [u32; 2], user: *mut [u32; 2]) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn GetCurrentProcess() -> Handle;
    }

    const SNAPSHOT_PROCESSES: u32 = 0x2;
    const TERMINATE: u32 = 0x0001;
    const QUERY_LIMITED: u32 = 0x1000;

    /// When a process was created, or `None` if it cannot be opened.
    fn created(pid: u32) -> Option<FileTime> {
        unsafe {
            let process = OpenProcess(QUERY_LIMITED, 0, pid);
            if process.is_null() {
                return None;
            }
            let (mut made, mut a, mut b, mut c) = ([0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2]);
            let ok = GetProcessTimes(process, &mut made, &mut a, &mut b, &mut c) != 0;
            CloseHandle(process);
            // FILETIME is { low, high }; keep it as (high, low) so it orders by time.
            ok.then_some(FileTime { high: made[1], low: made[0] })
        }
    }

    /// Ends the WSL helper processes started under `root` (a wsl.exe that is
    /// still running): its `wslhost.exe`, and a second `wsl.exe` when the
    /// launcher was used. Only processes with those names, descended from
    /// `root`, and created after it are touched, so a process id that was
    /// reused can never lead this to something unrelated.
    pub fn end_helpers_of(root: u32) {
        let Some(root_made) = created(root) else { return };
        let mut processes = Vec::new();
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(SNAPSHOT_PROCESSES, 0);
            if snapshot.is_null() || snapshot as isize == -1 {
                return;
            }
            let mut entry: ProcessEntry = std::mem::zeroed();
            entry.size = std::mem::size_of::<ProcessEntry>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                let len = entry.exe.iter().position(|c| *c == 0).unwrap_or(entry.exe.len());
                let name = String::from_utf16_lossy(&entry.exe[..len]).to_ascii_lowercase();
                if name == "wslhost.exe" || name == "wsl.exe" {
                    processes.push((entry.pid, entry.parent));
                }
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        let mut helpers = Vec::new();
        let mut parents = vec![root];
        while let Some(parent) = parents.pop() {
            for (pid, of) in &processes {
                let is_new = *pid != root && !helpers.contains(pid);
                if *of == parent && is_new && created(*pid).is_some_and(|made| made >= root_made) {
                    helpers.push(*pid);
                    parents.push(*pid);
                }
            }
        }
        for pid in helpers.into_iter().rev() {
            unsafe {
                let process = OpenProcess(TERMINATE, 0, pid);
                if !process.is_null() {
                    TerminateProcess(process, 1);
                    CloseHandle(process);
                }
            }
        }
    }

    const KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const BREAKAWAY_OK: u32 = 0x0800; // lets the terminal Pier opens outlive it
    const SILENT_BREAKAWAY_OK: u32 = 0x1000;

    /// The job this process is in, once `end_children_with_this_process` has
    /// made it.
    static JOB: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

    fn set_job_limits(job: Handle, flags: u32) -> bool {
        const EXTENDED_LIMIT_INFORMATION: i32 = 9;
        unsafe {
            let mut limits: ExtendedLimits = std::mem::zeroed();
            limits.basic.limit_flags = flags;
            SetInformationJobObject(
                job,
                EXTENDED_LIMIT_INFORMATION,
                &limits as *const ExtendedLimits as *const c_void,
                std::mem::size_of::<ExtendedLimits>() as u32,
            ) != 0
        }
    }

    /// Puts this process in a job that kills everything in it when the job's
    /// last handle closes, which is when this process ends. Children are in
    /// the job from birth. The handle is deliberately never closed.
    pub fn end_children_with_this_process() {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job.is_null() {
                return;
            }
            if !set_job_limits(job, KILL_ON_JOB_CLOSE | BREAKAWAY_OK) || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                CloseHandle(job);
                return;
            }
            JOB.store(job, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// From now on, processes this one starts are born outside the job and
    /// so outlive it. Those already running stay in it and still end with
    /// this process.
    pub fn let_new_children_outlive_this_process() {
        let job = JOB.load(std::sync::atomic::Ordering::SeqCst);
        if !job.is_null() {
            set_job_limits(job, KILL_ON_JOB_CLOSE | BREAKAWAY_OK | SILENT_BREAKAWAY_OK);
        }
    }
}
/// A raw byte stream to the Docker socket on its stdin/stdout.
pub fn relay(distro: &str) -> std::io::Result<Session> {
    let mut cmd = if cfg!(windows) {
        let mut c = wsl();
        c.args(["-d", distro, "-u", "root", "-e", "sh", "-c", RELAY_SCRIPT]);
        c
    } else {
        let _ = distro;
        let mut c = Command::new("sh");
        c.args(["-c", RELAY_SCRIPT]);
        c
    };
    Session::spawn(cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()))
}

/// wsl.exe prints UTF-16LE unless WSL_UTF8 is honoured (older builds ignore it).
fn decode(bytes: &[u8]) -> String {
    let looks_utf16 = bytes.len() >= 2 && bytes.iter().skip(1).step_by(2).take(8).all(|b| *b == 0);
    if looks_utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn lines(bytes: &[u8]) -> Vec<String> {
    decode(bytes)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// How long a question to wsl.exe (which distros exist, which are running,
/// shut one down) may take. These normally answer in well under a second; a
/// hung one would otherwise block the status poll and the event watcher,
/// which wait for the answer.
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Why a `wsl -l` did not produce a list.
enum ListError {
    /// wsl.exe could not be run, or did not answer in time.
    NoAnswer(String),
    /// wsl.exe ran and exited non-zero, with this message.
    Refused(String),
}

impl ListError {
    fn message(self) -> String {
        match self {
            ListError::NoAnswer(m) | ListError::Refused(m) => m,
        }
    }
}

async fn list(extra: &[&str]) -> Result<Vec<String>, ListError> {
    if !cfg!(windows) {
        return Ok(vec!["local".into()]);
    }
    let out = tokio::time::timeout(QUERY_TIMEOUT, output(wsl().arg("-l").args(extra).arg("-q")))
        .await
        .map_err(|_| ListError::NoAnswer("wsl.exe did not answer in time".into()))?
        .map_err(|e| ListError::NoAnswer(format!("Could not run wsl.exe: {e}")))?;
    if !out.status.success() {
        // wsl.exe reports its own errors on stdout.
        let message = [decode(&out.stdout), decode(&out.stderr)].join(" ").trim().to_string();
        return Err(ListError::Refused(if message.is_empty() { "wsl.exe failed".into() } else { message }));
    }
    Ok(lines(&out.stdout))
}

/// Installed distros, default first. Docker Desktop's own distros are hidden.
/// An error means the list could not be had, not that it is empty.
pub async fn distros() -> Result<Vec<String>, String> {
    Ok(list(&[])
        .await
        .map_err(ListError::message)?
        .into_iter()
        .filter(|d| !d.starts_with("docker-desktop"))
        .collect())
}

/// Distros that are currently booted. Does not boot anything.
///
/// `Ok(empty)` means none are running; `Err` means WSL could not tell us.
/// The two must not be mixed up: on "none running" Pier lets go of the
/// engine, which would be wrong if wsl.exe merely hiccuped.
pub async fn running() -> Result<Vec<String>, String> {
    match list(&["--running"]).await {
        Ok(running) => Ok(running),
        // `wsl -l --running` also exits non-zero when nothing is running, and
        // says so only in a sentence in the user's language. So "nothing is
        // running" is reported only when a second, independent look confirms
        // it: the verbose list, whose rows name every distro's state. Any
        // other failure stays a failure, and the caller keeps what it knows
        // and asks again.
        Err(ListError::Refused(message)) => match states().await {
            Some(states) if states.iter().all(|(_, running)| !running) => Ok(vec![]),
            Some(states) => Ok(states.into_iter().filter(|(_, running)| *running).map(|(name, _)| name).collect()),
            None => Err(message),
        },
        Err(no_answer) => Err(no_answer.message()),
    }
}

/// Every installed distro and whether it is running, from `wsl -l -v`.
/// `None` when the answer cannot be had or cannot be read with certainty.
async fn states() -> Option<Vec<(String, bool)>> {
    let out = tokio::time::timeout(QUERY_TIMEOUT, output(wsl().args(["-l", "-v"]))).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    parse_states(&decode(&out.stdout))
}

/// Reads the table printed by `wsl -l -v`:
///
/// ```text
///   NAME            STATE           VERSION
/// * Ubuntu-26.04    Running         2
///   docker-desktop  Stopped         2
/// ```
///
/// Returns `None` unless every row has a state this code knows, so a format
/// it does not understand (a translated state, say) is never read as
/// "nothing is running".
fn parse_states(table: &str) -> Option<Vec<(String, bool)>> {
    const NOT_RUNNING: [&str; 4] = ["Stopped", "Installing", "Uninstalling", "Converting"];
    let mut states = Vec::new();
    for row in table.lines().skip(1) {
        let row = row.trim().trim_start_matches('\u{feff}').trim_start_matches('*').trim();
        if row.is_empty() {
            continue;
        }
        let mut columns = row.split_whitespace();
        let (name, state) = (columns.next()?, columns.next()?);
        if state == "Running" {
            states.push((name.to_string(), true));
        } else if NOT_RUNNING.contains(&state) {
            states.push((name.to_string(), false));
        } else {
            return None;
        }
    }
    (!states.is_empty()).then_some(states)
}

/// Run a shell script as root inside the distro (boots it if needed).
pub async fn run_root(distro: &str, script: &str) -> Result<String, String> {
    let out = if cfg!(windows) {
        output(wsl().args(["-d", distro, "-u", "root", "-e", "sh", "-c", script])).await
    } else {
        Command::new("sh").args(["-c", script]).kill_on_drop(true).output().await
    }
    .map_err(|e| format!("Could not run command in {distro}: {e}"))?;

    let stdout = decode(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = decode(&out.stderr).trim().to_string();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

/// Whether the dockerd process exists in `distro`. Only to be asked of a distro
/// that is running (it would boot a stopped one). `None` when it cannot be
/// told: the question failed, timed out, or the distro has no `pgrep`.
///
/// This is how "Docker does not answer" is told apart from "Docker is not
/// there": only the second is a reason to let go of the distro.
pub async fn daemon_alive(distro: &str) -> Option<bool> {
    const ASK: &str = "command -v pgrep >/dev/null 2>&1 || { echo unknown; exit 0; }; pgrep -x dockerd >/dev/null 2>&1 && echo yes || echo no";
    match tokio::time::timeout(QUERY_TIMEOUT, run_root(distro, ASK)).await {
        Ok(Ok(answer)) if answer == "yes" => Some(true),
        Ok(Ok(answer)) if answer == "no" => Some(false),
        _ => None,
    }
}

/// A do-nothing process that keeps the distro from idling out while the
/// engine is meant to be up. WSL shuts a distro down shortly after its last
/// session exits, even when systemd services are still running.
pub fn keepalive(distro: &str) -> Option<Session> {
    if !cfg!(windows) {
        return None;
    }
    let mut cmd = wsl();
    cmd.args(["-d", distro, "-e", "sleep", "infinity"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    Session::spawn(&mut cmd).ok()
}

/// Starts dockerd. The start command is also bounded inside the distro
/// (`timeout`, where the distro has it): killing wsl.exe on the Windows side
/// is not a promise that the command it launched stops, so the command ends
/// itself before Pier's own 60 s start budget (`Engine::start_timeout`) does.
pub const START_ENGINE: &str = r#"command -v dockerd >/dev/null 2>&1 || { echo "Docker Engine is not installed in this distro" >&2; exit 1; }
T=""; command -v timeout >/dev/null 2>&1 && T="timeout 45"
if [ -d /run/systemd/system ]; then $T systemctl start docker
else $T service docker start
fi
rc=$?
[ "$rc" -eq 124 ] && echo "Starting Docker took too long" >&2
exit $rc"#;

pub const STOP_ENGINE: &str = r#"if [ -d /run/systemd/system ]; then systemctl stop docker.socket docker
else service docker stop
fi"#;

/// Shut the whole distro down, giving its memory back to Windows.
pub async fn terminate(distro: &str) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let out = tokio::time::timeout(QUERY_TIMEOUT * 3, output(wsl().args(["--terminate", distro])))
        .await
        .map_err(|_| format!("Shutting down {distro} took too long"))?
        .map_err(|e| format!("Could not run wsl.exe: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // wsl.exe reports its own errors on stdout.
    let message = [decode(&out.stdout), decode(&out.stderr)].join(" ").trim().to_string();
    Err(if message.is_empty() { format!("Could not shut down {distro}") } else { message })
}

/// Open an interactive shell in a container, in Windows Terminal if present.
pub fn open_terminal(distro: &str, container: &str) -> Result<(), String> {
    if !container.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        return Err("Invalid container id".into());
    }
    let wsl_args = ["-d", distro, "-u", "root", "-e", "docker", "exec", "-it", container, "sh"];
    // The terminal is the user's from here on: it is started outside the job
    // that ends Pier's own children, so it stays open if Pier quits.
    let detached = |program: &str| {
        let mut cmd = std::process::Command::new(program);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0100_0000); // CREATE_BREAKAWAY_FROM_JOB
        }
        cmd
    };
    let wt = detached("wt.exe").args(["new-tab", "--title", container, "wsl.exe"]).args(wsl_args).spawn();
    if wt.is_ok() {
        return Ok(());
    }
    detached("cmd.exe")
        .args(["/c", "start", "", "wsl.exe"])
        .args(wsl_args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open a terminal: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a real, running WSL distro, so it only runs when asked for:
    /// `PIER_TEST_DISTRO=<name> cargo test -- --ignored`. It starts nothing
    /// but a `sleep` in that distro.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "needs a running WSL distro named by PIER_TEST_DISTRO"]
    async fn a_command_that_times_out_is_killed_on_both_sides() {
        let distro = std::env::var("PIER_TEST_DISTRO").expect("set PIER_TEST_DISTRO");
        // An odd duration, so the process can be told apart from any other sleep.
        let nap = format!("sleep 31{:03}", std::process::id() % 1000);
        let find = format!("pgrep -fx '{nap}' || true");

        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(2), run_root(&distro, &nap)).await;
        assert!(timed_out.is_err(), "the command should still have been running");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let left = run_root(&distro, &find).await.unwrap();
        assert!(left.is_empty(), "still running inside the distro: pid {left}");
    }

    #[test]
    fn reads_distro_states_only_when_sure() {
        let table = "  NAME              STATE           VERSION\r\n* Ubuntu-26.04      Stopped         2\r\n  docker-desktop    Running         2\r\n";
        assert_eq!(
            parse_states(table),
            Some(vec![("Ubuntu-26.04".to_string(), false), ("docker-desktop".to_string(), true)])
        );
        let all_stopped = "  NAME     STATE     VERSION\n* Ubuntu   Stopped   2\n";
        assert_eq!(parse_states(all_stopped), Some(vec![("Ubuntu".to_string(), false)]));

        // A state it does not know (translated, or new) means "cannot tell",
        // never "not running".
        assert_eq!(parse_states("  NAME     STATE     VERSION\n* Ubuntu   已停止   2\n"), None);
        // An error message instead of a table, or an empty table.
        assert_eq!(parse_states("The service cannot be started.\n"), None);
        assert_eq!(parse_states("  NAME     STATE     VERSION\n"), None);
    }

    /// The real `wsl -l -v` on this machine must be readable.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "needs WSL with at least one distro"]
    async fn reads_the_real_state_table() {
        let states = states().await.expect("wsl -l -v should be readable");
        assert!(!states.is_empty());
    }

    #[test]
    fn decodes_utf16_and_utf8() {
        let utf16: Vec<u8> = "Ubuntu\r\nDebian\r\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(lines(&utf16), vec!["Ubuntu", "Debian"]);
        assert_eq!(lines(b"Ubuntu\nDebian\n"), vec!["Ubuntu", "Debian"]);
    }
}

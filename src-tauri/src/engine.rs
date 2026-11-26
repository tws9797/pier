//! What Pier knows about the engine it manages, shared between the Tauri
//! commands, the event watcher, the pipe bridge and the idle monitor.
//!
//! Pier keeps a distro awake (a `sleep infinity` session plus the event
//! stream) only while there is something to keep it awake for: the window is
//! open, a Windows tool is connected to the pipe, or a container is running.
//! After `idle_after()` with none of those it lets go ("sleeping"), so WSL can
//! shut the distro down and hand its memory back. Other sessions the user has
//! in that distro are left alone and keep it up on their own. The next pipe
//! connection, or opening the window, starts the engine again.
//!
//! Two locks keep that honest:
//! - `gate` makes "something starts using the engine" (a pipe connection, the
//!   window opening) and "the engine goes to sleep" exclusive, so a user that
//!   arrives while the idle monitor is deciding is either seen by it or finds
//!   the engine asleep and wakes it. Never neither.
//! - `starting` makes every caller that needs the engine wait for the one
//!   start that is in progress instead of connecting to a half-started engine.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hyper::Method;
use crate::wsl::Session as Child;

use crate::{docker, wsl};

/// Failed Docker probes in a row, with the distro running, before the engine
/// counts as down. The window probes every 5 s, so this is about 15 s.
const PROBE_FAILURES_TO_STOP: u32 = 3;

/// How often the idle monitor looks.
const IDLE_CHECK: Duration = Duration::from_secs(30);

/// How long nothing must need the engine before Pier lets go of it.
/// `PIER_IDLE_SECS` overrides the 5 minute default.
fn idle_after() -> Duration {
    let secs = std::env::var("PIER_IDLE_SECS").ok().and_then(|s| s.parse().ok());
    Duration::from_secs(secs.unwrap_or(300))
}

pub struct Engine {
    /// The WSL distro whose Docker Engine we manage.
    pub distro: Mutex<Option<String>>,
    /// Keeps the distro from idling out while the engine is wanted.
    ///
    /// One per distro, and not tied to which distro is selected: a distro
    /// that was switched away from stays held while it has work (see
    /// `background`). A key means Pier means to hold that distro; the value
    /// is the process doing it, when there is one.
    keepalive: Mutex<HashMap<String, Option<Child>>>,
    /// Distros that are no longer selected but still held, because they had
    /// running containers (or "keep running" is on) when the user switched
    /// away. Each is looked at again from time to time and let go once it
    /// has nothing running. The value is when it was last looked at.
    background: Mutex<HashMap<String, Instant>>,
    /// The engine is supposed to be running: it was seen up, or started, and
    /// has not been stopped or put to sleep since. The event watcher keeps
    /// (re)connecting only while this holds.
    pub expected: AtomicBool,
    /// The engine answered recently.
    pub up: AtomicBool,
    /// The event stream is open, which proves the engine is up for free.
    pub events_live: AtomicBool,
    /// Pier let go of an idle engine and will start it again on demand.
    sleeping: AtomicBool,
    /// A wake-up is in progress.
    waking: AtomicBool,
    /// Starts that are running or waiting for their turn, manual ones
    /// included. While this is not zero a pipe connection waits in `wake`.
    pending_starts: AtomicUsize,
    /// Goes up whenever the engine is stopped or another distro is selected.
    /// A start that finishes under a different number than it began with is
    /// out of date and must not record its result.
    epoch: AtomicU64,
    backend: Backend,
    /// Time budget for one start: the start script plus waiting for Docker.
    start_timeout: Duration,
    /// How long the stop script may take before Pier stops waiting for it.
    stop_timeout: Duration,
    /// The user stopped the engine on purpose (Stop, or shutting the distro
    /// down). Until it is started again, pipe connections are turned away
    /// instead of being relayed, because a relay would boot the distro again.
    ///
    /// Kept per distro, like `stopping`: a tool still connected to distro A
    /// must be turned away because A was stopped, whichever distro is
    /// selected at that moment.
    halted: Mutex<HashSet<String>>,
    /// Probes in a row that found the distro running but got no answer from
    /// Docker. See `probe_failed`.
    probe_failures: AtomicU32,
    /// Goes up every time the engine shows it is alive (a probe answered,
    /// the event stream connected, a start succeeded). A slow health check
    /// compares it before and after: if it moved, the check is out of date.
    signs_of_life: AtomicU64,
    /// Stops in progress, by distro. While one runs, a probe that still finds
    /// the engine answering is ignored: it is on its way down.
    stopping: Mutex<HashMap<String, usize>>,
    window_open: AtomicBool,
    /// The user's choice to never let go of an idle engine (Settings).
    keep_running: AtomicBool,
    /// Open connections on the Windows pipe, by the distro serving them.
    connections: Mutex<HashMap<String, usize>>,
    /// Running containers, as of the last count.
    running: AtomicUsize,
    /// A container event arrived since the last count.
    pub count_stale: AtomicBool,
    /// Last moment something needed the engine.
    activity: Mutex<Instant>,
    /// See the module comment.
    gate: Mutex<()>,
    starting: tokio::sync::Mutex<()>,
}

/// A distro, and the engine epoch it was read under. See `Engine::target`.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub distro: String,
    epoch: u64,
}

type Task<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The few things the engine state machine does to the outside world, kept
/// behind functions so the tests can stand in for WSL and Docker.
pub struct Backend {
    start: Box<dyn Fn(String) -> Task<Result<(), String>> + Send + Sync>,
    stop: Box<dyn Fn(String) -> Task<Result<(), String>> + Send + Sync>,
    ping: Box<dyn Fn(String) -> Task<bool> + Send + Sync>,
    /// Running containers in a distro's engine, if it answers.
    count: Box<dyn Fn(String) -> Task<Option<usize>> + Send + Sync>,
    /// Whether the dockerd process exists in a (running) distro. `None`
    /// when that cannot be told.
    daemon_alive: Box<dyn Fn(String) -> Task<Option<bool>> + Send + Sync>,
    /// Shuts the whole distro down.
    terminate: Box<dyn Fn(String) -> Task<Result<(), String>> + Send + Sync>,
    keepalive: Box<dyn Fn(&str) -> Option<Child> + Send + Sync>,
}

impl Backend {
    fn wsl() -> Self {
        Self {
            start: Box::new(|d| Box::pin(async move { wsl::run_root(&d, wsl::START_ENGINE).await.map(|_| ()) })),
            stop: Box::new(|d| Box::pin(async move { wsl::run_root(&d, wsl::STOP_ENGINE).await.map(|_| ()) })),
            ping: Box::new(|d| Box::pin(async move { docker::ping(&d).await })),
            count: Box::new(|d| {
                Box::pin(async move {
                    let list = docker::json(&d, Method::GET, "/containers/json", None).await.ok()?;
                    Some(list.as_array()?.len())
                })
            }),
            daemon_alive: Box::new(|d| Box::pin(async move { wsl::daemon_alive(&d).await })),
            terminate: Box::new(|d| Box::pin(async move { wsl::terminate(&d).await })),
            keepalive: Box::new(wsl::keepalive),
        }
    }
}

/// Marks a distro as being stopped until dropped, however the stop ends.
struct Stopping<'a>(&'a Mutex<HashMap<String, usize>>, String);
impl<'a> Stopping<'a> {
    fn begin(stopping: &'a Mutex<HashMap<String, usize>>, distro: &str) -> Self {
        *stopping.lock().unwrap().entry(distro.to_string()).or_insert(0) += 1;
        Self(stopping, distro.to_string())
    }
}
impl Drop for Stopping<'_> {
    fn drop(&mut self) {
        let mut stopping = self.0.lock().unwrap();
        if let Some(count) = stopping.get_mut(&self.1) {
            *count -= 1;
            if *count == 0 {
                stopping.remove(&self.1);
            }
        }
    }
}

fn still_running(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Removes one pending start when dropped, however the start ends.
struct PendingStart<'a>(&'a AtomicUsize);
impl<'a> PendingStart<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count)
    }
}
impl Drop for PendingStart<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

const CANCELLED: &str = "The start was cancelled: the engine was stopped or another distro was selected";

impl Engine {
    pub fn new(distro: Option<String>) -> Self {
        Self::with_backend(distro, Backend::wsl())
    }

    fn with_backend(distro: Option<String>, backend: Backend) -> Self {
        Self {
            distro: Mutex::new(distro),
            keepalive: Mutex::new(HashMap::new()),
            background: Mutex::new(HashMap::new()),
            expected: AtomicBool::new(false),
            up: AtomicBool::new(false),
            events_live: AtomicBool::new(false),
            sleeping: AtomicBool::new(false),
            waking: AtomicBool::new(false),
            pending_starts: AtomicUsize::new(0),
            epoch: AtomicU64::new(0),
            backend,
            start_timeout: Duration::from_secs(60),
            stop_timeout: Duration::from_secs(30),
            halted: Mutex::new(HashSet::new()),
            probe_failures: AtomicU32::new(0),
            signs_of_life: AtomicU64::new(0),
            stopping: Mutex::new(HashMap::new()),
            window_open: AtomicBool::new(true),
            keep_running: AtomicBool::new(false),
            connections: Mutex::new(HashMap::new()),
            running: AtomicUsize::new(0),
            count_stale: AtomicBool::new(true),
            activity: Mutex::new(Instant::now()),
            gate: Mutex::new(()),
            starting: tokio::sync::Mutex::new(()),
        }
    }

    pub fn distro(&self) -> Result<String, String> {
        self.distro.lock().unwrap().clone().ok_or_else(|| "No WSL distro selected".to_string())
    }

    /// Something needs the engine right now; restart the idle clock.
    fn touch(&self) {
        *self.activity.lock().unwrap() = Instant::now();
    }

    pub fn hold_keepalive(&self, distro: &str) {
        let mut held = self.keepalive.lock().unwrap();
        let alive = held.get_mut(distro).is_some_and(|child| match child {
            Some(c) => still_running(c),
            None => false,
        });
        if !alive {
            held.insert(distro.to_string(), (self.backend.keepalive)(distro));
        }
    }

    /// Lets go of one distro, selected or in the background.
    pub fn drop_keepalive(&self, distro: &str) {
        self.background.lock().unwrap().remove(distro);
        if let Some(Some(mut child)) = self.keepalive.lock().unwrap().remove(distro) {
            let _ = child.start_kill();
        }
    }

    /// Lets go of every distro (Pier is quitting).
    pub fn drop_all_keepalives(&self) {
        self.background.lock().unwrap().clear();
        for (_, child) in self.keepalive.lock().unwrap().drain() {
            if let Some(mut child) = child {
                let _ = child.start_kill();
            }
        }
    }

    /// Whether Pier means to keep `distro` awake right now.
    pub fn holds(&self, distro: &str) -> bool {
        self.keepalive.lock().unwrap().contains_key(distro)
    }

    // ---- who is using the engine -------------------------------------

    /// A Windows tool connected to the pipe and is being served by `distro`.
    /// Pair with `disconnected`. Connections are counted per distro: a tool
    /// still attached to a distro that is no longer selected (an IDE
    /// listening for its events, say) does not keep the selected one awake.
    pub fn connected(&self, distro: &str) {
        let _gate = self.gate.lock().unwrap();
        *self.connections.lock().unwrap().entry(distro.to_string()).or_insert(0) += 1;
        self.touch();
    }

    pub fn disconnected(&self, distro: &str) {
        let _gate = self.gate.lock().unwrap();
        let mut connections = self.connections.lock().unwrap();
        if let Some(count) = connections.get_mut(distro) {
            *count -= 1;
            if *count == 0 {
                connections.remove(distro);
            }
        }
        drop(connections);
        self.touch();
    }

    /// Open pipe connections served by the selected distro.
    fn connections(&self) -> usize {
        let selected = self.distro.lock().unwrap();
        let connections = self.connections.lock().unwrap();
        selected.as_ref().and_then(|d| connections.get(d)).copied().unwrap_or(0)
    }

    pub fn window_opened(&self) {
        let _gate = self.gate.lock().unwrap();
        self.window_open.store(true, Ordering::Relaxed);
        self.touch();
    }

    pub fn window_closed(&self) {
        let _gate = self.gate.lock().unwrap();
        self.window_open.store(false, Ordering::Relaxed);
        self.touch();
    }

    // ---- what is known about the engine -------------------------------

    // Everything Pier learns about the engine comes from something slow (a
    // probe, a start, an event stream) that was aimed at one distro. By the
    // time the answer is in, the user may have stopped the engine or picked
    // another distro. So each of those takes a `Target` first and hands it
    // back with its result, and a result for an outdated target is dropped.

    /// The distro in use right now, and the epoch that goes with it.
    pub fn target(&self) -> Result<Target, String> {
        let _gate = self.gate.lock().unwrap();
        Ok(Target { distro: self.distro()?, epoch: self.epoch.load(Ordering::Relaxed) })
    }

    fn is_current(&self, target: &Target) -> bool {
        self.epoch.load(Ordering::Relaxed) == target.epoch
    }

    /// Fails unless `distro` is the one in use. Requests from the window
    /// name the distro they were made for; this is what stops a delete meant
    /// for one distro from landing in another after a switch.
    pub fn require(&self, distro: &str) -> Result<(), String> {
        match self.distro.lock().unwrap().as_deref() {
            Some(current) if current == distro => Ok(()),
            _ => Err(format!("Nothing was done: this was meant for {distro}, which is no longer the selected distro")),
        }
    }

    /// Makes sure a distro is selected and still installed, picking the
    /// first one otherwise (which counts as a switch).
    pub fn ensure_distro(&self, installed: &[String]) -> Option<String> {
        let _gate = self.gate.lock().unwrap();
        let current = self.distro.lock().unwrap().clone();
        if current.as_ref().is_some_and(|d| installed.contains(d)) {
            return current;
        }
        let first = installed.first().cloned();
        self.select_locked(first.clone()); // the same switch as picking it by hand
        first
    }

    /// The window asked and the engine answered: it is in use, not asleep.
    pub fn seen_running(&self, target: &Target) {
        let _gate = self.gate.lock().unwrap();
        if !self.is_current(target) {
            return;
        }
        let distro = target.distro.as_str();
        if self.stopping.lock().unwrap().contains_key(distro) {
            return; // still answering, but being stopped: not a reason to hold on to it
        }
        self.halted.lock().unwrap().remove(distro); // it is running, whoever started it
        self.adopted(distro);
        self.probe_failures.store(0, Ordering::Relaxed);
        self.sleeping.store(false, Ordering::Relaxed);
        self.expected.store(true, Ordering::Relaxed);
        self.up.store(true, Ordering::Relaxed);
        self.touch();
        self.hold_keepalive(distro);
    }

    /// The distro is running, but Docker did not answer a probe (it timed
    /// out, or returned an error). One such failure proves nothing: Docker
    /// may just be busy, and letting go of the keep-alive on the strength of
    /// it could take a distro full of running containers down with it. So
    /// the state is left as it was. "In a row" means with no sign of life in
    /// between: an answered probe or a reconnected event stream starts the
    /// count again.
    ///
    /// Returns true from the `PROBE_FAILURES_TO_STOP`th failure in a row on.
    /// Even that does not mean the engine is down, only that it is time to
    /// find out: the caller then asks `confirm_down`.
    pub fn probe_failed(&self, target: &Target) -> bool {
        let _gate = self.gate.lock().unwrap();
        if !self.is_current(target) {
            return false;
        }
        let failures = self.probe_failures.fetch_add(1, Ordering::Relaxed) + 1;
        failures >= PROBE_FAILURES_TO_STOP
    }

    /// Docker has not answered for a while. Finds out whether it is really
    /// gone, by looking for the dockerd process in the distro (which the
    /// caller knows to be running), and only then lets go:
    ///
    /// - dockerd is not there: the engine is down. Recorded as stopped, the
    ///   keep-alive is released, true is returned.
    /// - dockerd is there, or it cannot be told: the engine is unreachable,
    ///   not stopped. It is shown as not running, but the keep-alive stays
    ///   and the event watcher keeps trying, because its containers may be
    ///   running all the while. False is returned.
    pub async fn confirm_down(&self, target: &Target) -> bool {
        // Looking for dockerd takes a moment, and Docker may come back in
        // that moment: the event stream reconnects, a probe is answered. Then
        // this check is about a past that is over, and whatever it found must
        // not be recorded. Above all it must not clear `events_live` under a
        // stream that is open and being read.
        let before = self.signs_of_life.load(Ordering::Relaxed);
        let gone = (self.backend.daemon_alive)(target.distro.clone()).await == Some(false);
        if self.signs_of_life.load(Ordering::Relaxed) != before {
            return false;
        }
        if gone {
            self.seen_stopped(target);
            return true;
        }
        let _gate = self.gate.lock().unwrap();
        if self.is_current(target) && self.signs_of_life.load(Ordering::Relaxed) == before {
            self.up.store(false, Ordering::Relaxed);
            self.events_live.store(false, Ordering::Relaxed);
        }
        false
    }

    /// The engine in `distro` has just shown it is alive and is being
    /// watched as the selected one. Called with the gate held.
    ///
    /// From here the usual idle rule looks after it, so its background
    /// record, if it had one, is no longer needed. Not before: until this
    /// happens nothing else is watching its keep-alive.
    fn adopted(&self, distro: &str) {
        self.signs_of_life.fetch_add(1, Ordering::Relaxed);
        self.background.lock().unwrap().remove(distro);
    }

    /// The engine is known to be down: its distro is not running, or Docker
    /// failed to answer repeatedly. Ignored while it is asleep or a start is
    /// under way, so a status poll cannot cancel one. Otherwise there is
    /// nothing left to keep the distro awake for.
    pub fn seen_stopped(&self, target: &Target) {
        let _gate = self.gate.lock().unwrap();
        if self.is_current(target) && !self.is_asleep() && self.pending_starts.load(Ordering::Relaxed) == 0 {
            self.expected.store(false, Ordering::Relaxed);
            self.up.store(false, Ordering::Relaxed);
            self.events_live.store(false, Ordering::Relaxed);
            self.drop_keepalive(&target.distro);
        }
    }

    /// The engine is gone for good (stopped on purpose, unreachable, its
    /// distro shut down, or another distro selected): nothing reconnects or
    /// wakes it until it is started again, any start still in flight is
    /// made out of date, and Pier's keep-alive is released. The event
    /// watcher drops its stream itself.
    /// Resets, unless the engine changed hands since `target` was taken.
    pub fn give_up(&self, target: &Target) {
        let _gate = self.gate.lock().unwrap();
        if self.is_current(target) {
            self.reset_locked();
            self.drop_keepalive(&target.distro);
        }
    }

    /// The event stream for `target` is open. False when it is out of date
    /// and must be dropped.
    pub fn events_connected(&self, target: &Target) -> bool {
        let _gate = self.gate.lock().unwrap();
        if !self.is_current(target) || !self.expected.load(Ordering::Relaxed) {
            return false;
        }
        self.probe_failures.store(0, Ordering::Relaxed); // it answers: not "in a row" any more
        self.adopted(&target.distro);
        self.up.store(true, Ordering::Relaxed);
        self.count_stale.store(true, Ordering::Relaxed);
        self.events_live.store(true, Ordering::Relaxed);
        true
    }

    /// The event stream for `target` ended, or could not be opened.
    pub fn events_lost(&self, target: &Target) {
        let _gate = self.gate.lock().unwrap();
        if self.is_current(target) {
            self.events_live.store(false, Ordering::Relaxed);
            self.up.store(false, Ordering::Relaxed);
        }
    }

    /// Whether a status probe for `target` may be sent: it is still the
    /// engine in question and is not in the middle of being stopped. An
    /// engine that was stopped earlier may be probed (its distro is checked
    /// to be running first), which is how one started from inside the distro
    /// is noticed again.
    pub fn can_probe(&self, target: &Target) -> bool {
        self.is_current(target) && !self.is_stopping(&target.distro)
    }

    /// Whether `target` is still the engine as it stands: not stopped, not
    /// switched away from, since the target was taken. Asked again right
    /// before each relay is started for it, because the awaits in between
    /// are where a stop or a switch slips in.
    pub fn still(&self, target: &Target) -> bool {
        self.is_current(target) && !self.refuses(&target.distro)
    }

    /// Whether the event stream for `target` should stay open.
    pub fn wants_events(&self, target: &Target) -> bool {
        self.is_current(target) && self.expected.load(Ordering::Relaxed)
    }

    fn reset_locked(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.probe_failures.store(0, Ordering::Relaxed);
        self.expected.store(false, Ordering::Relaxed);
        self.up.store(false, Ordering::Relaxed);
        self.events_live.store(false, Ordering::Relaxed);
        self.sleeping.store(false, Ordering::Relaxed);
        self.count_stale.store(true, Ordering::Relaxed);
        // The keep-alive is not touched here: whether to let go of a distro
        // is each caller's decision, and for a distro that is merely being
        // switched away from the answer is often no.
    }

    /// Asleep, or on its way back: the UI shows this instead of "stopped".
    pub fn is_asleep(&self) -> bool {
        self.sleeping.load(Ordering::Relaxed) || self.waking.load(Ordering::Relaxed)
    }

    // ---- starting and waking -------------------------------------------

    // Start, wake, stop and select are the only ways the engine changes hands.
    // They all queue on `starting`, and a start only records its result if no
    // stop or distro switch happened while it ran (`epoch`).

    /// Starts dockerd in `distro` and waits until it answers (the Start
    /// button). The target is fixed when the request is made, like a stop's:
    /// if the engine is stopped or another distro selected while this start
    /// waits its turn, it is cancelled instead of starting whatever happens
    /// to be selected by then.
    pub async fn start(&self, distro: &str) -> Result<(), String> {
        let target = self.target()?;
        if target.distro != distro {
            return Err(CANCELLED.into());
        }
        let _pending = PendingStart::new(&self.pending_starts);
        let _one = self.starting.lock().await;
        if !self.is_current(&target) {
            return Err(CANCELLED.into());
        }
        self.start_locked(target).await
    }

    async fn start_locked(&self, target: Target) -> Result<(), String> {
        let distro = target.distro.clone();
        // The whole start, script and probes together, has one time budget.
        // Without it a Docker that accepts connections but never answers
        // would hold the start lock, and every CLI call and stop queued
        // behind it, for minutes.
        let deadline = tokio::time::Instant::now() + self.start_timeout;
        // Nothing came up, so do not keep the distro awake for it, unless a
        // newer start or stop already took over the keep-alive.
        let failed = |message: String| {
            if self.is_current(&target) {
                self.drop_keepalive(&target.distro);
            }
            Err(message)
        };

        self.hold_keepalive(&distro);
        match tokio::time::timeout_at(deadline, (self.backend.start)(distro.clone())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return failed(e),
            Err(_) => return failed(format!("Starting Docker in {distro} took too long")),
        }
        loop {
            if !self.is_current(&target) {
                return Err(CANCELLED.into());
            }
            // Each probe gets whatever time is left, not a budget of its own.
            match tokio::time::timeout_at(deadline, (self.backend.ping)(distro.clone())).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(_) => return failed("Docker was started but is not answering yet".into()),
            }
            if tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_millis(250))).await.is_err() {
                return failed("Docker was started but is not answering yet".into());
            }
        }
        let _gate = self.gate.lock().unwrap();
        if !self.is_current(&target) {
            return Err(CANCELLED.into()); // stopped or switched while it started
        }
        self.halted.lock().unwrap().remove(&distro);
        self.adopted(&distro);
        self.sleeping.store(false, Ordering::Relaxed);
        self.count_stale.store(true, Ordering::Relaxed);
        self.touch();
        self.expected.store(true, Ordering::Relaxed);
        self.up.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Returns once the engine is ready to be talked to, as far as Pier can
    /// arrange that: a sleeping engine is started, and a start that is
    /// already in progress (a wake-up or the Start button) is waited for, so
    /// no caller connects to a half-started engine. Does nothing for an
    /// engine that is up, or one the user stopped on purpose. A failed
    /// attempt is not repeated for every later connection: the engine then
    /// simply counts as stopped.
    ///
    /// `distro` is the one the caller is about to talk to. A caller bound to
    /// a distro that is not the selected one has nothing to wait for or wake.
    ///
    /// Returns whether the caller may go on and connect. It may not when
    /// the user has stopped this engine on purpose: relaying the connection
    /// would boot the distro again and undo the stop.
    pub async fn wake(&self, distro: &str) -> bool {
        // Asked about the distro the caller is bound to, not the selected
        // one: it is that distro a relay would boot.
        if self.refuses(distro) {
            return false;
        }
        let Ok(target) = self.target() else { return true };
        if target.distro != distro {
            return true;
        }
        if !self.is_asleep() && self.pending_starts.load(Ordering::Relaxed) == 0 {
            return true;
        }
        let _pending = PendingStart::new(&self.pending_starts);
        let _one = self.starting.lock().await; // waits for a start in progress
        {
            let _gate = self.gate.lock().unwrap();
            if !self.is_current(&target) || !self.sleeping.load(Ordering::Relaxed) {
                // The start we waited for did the job, or the engine changed hands.
                return !self.refuses(distro);
            }
            // `waking` goes up before `sleeping` comes down, so `is_asleep`
            // never reads false in between.
            self.waking.store(true, Ordering::Relaxed);
            self.sleeping.store(false, Ordering::Relaxed);
        }
        if let Err(e) = self.start_locked(target).await {
            eprintln!("engine: could not wake: {e}");
        }
        self.waking.store(false, Ordering::Relaxed);
        !self.refuses(distro) // it may have been stopped while it woke
    }

    /// Whether `distro`'s engine is being stopped, or was stopped on purpose
    /// and not started since. Nothing may start a relay into it then.
    fn refuses(&self, distro: &str) -> bool {
        self.stopping.lock().unwrap().contains_key(distro) || self.halted.lock().unwrap().contains(distro)
    }

    /// The same question for requests from the window, as an error to show.
    /// This is the one check that covers both doors: the pipe asks through
    /// `wake`, the panel through here.
    pub fn allows(&self, distro: &str) -> Result<(), String> {
        if self.stopping.lock().unwrap().contains_key(distro) {
            Err(format!("Docker in {distro} is being stopped"))
        } else if self.halted.lock().unwrap().contains(distro) {
            Err(format!("Docker in {distro} is stopped; start it first"))
        } else {
            Ok(())
        }
    }

    /// A stop of `distro` is in progress.
    pub fn is_stopping(&self, distro: &str) -> bool {
        self.stopping.lock().unwrap().contains_key(distro)
    }

    /// Stops dockerd on purpose (the Stop button). Any start in flight is
    /// made out of date first, then waited for, so start and stop never run
    /// their scripts at the same time.
    ///
    /// The stop is for `distro` from beginning to end: if another distro is
    /// selected while it waits, it still stops the one it was asked to stop
    /// and leaves the newly selected one alone.
    ///
    /// With `shutdown` the distro is shut down afterwards, whatever the
    /// stop script did, and that is what the result reports.
    ///
    /// From the first line to the last the engine counts as being stopped: a
    /// status probe that still gets an answer in that time does not bring
    /// back the keep-alive or reopen the pipe. A new epoch at the end drops
    /// probes that were sent before the stop finished and return after it.
    pub async fn stop(&self, distro: &str, shutdown: bool) -> Result<(), String> {
        let _stopping = Stopping::begin(&self.stopping, distro);
        self.halted.lock().unwrap().insert(distro.to_string());
        self.drop_keepalive(distro); // selected or held in the background, a stop lets go of it
        self.reset_if_selected(distro);
        let _one = self.starting.lock().await;
        self.reset_if_selected(distro); // a start that was queued behind the old one may have run
        // Bounded, so a stop script that hangs cannot hold the lock (and with
        // it every start and wake-up) for ever. Giving up drops the script's
        // wsl.exe, which kills it. The caller can still shut the distro down.
        let stopped = match tokio::time::timeout(self.stop_timeout, (self.backend.stop)(distro.to_string())).await {
            Ok(result) => result,
            Err(_) => Err(format!("Stopping Docker in {distro} took too long")),
        };
        let result = if shutdown { (self.backend.terminate)(distro.to_string()).await } else { stopped };
        self.reset_if_selected(distro);
        result
    }

    fn reset_if_selected(&self, distro: &str) {
        let _gate = self.gate.lock().unwrap();
        if self.distro.lock().unwrap().as_deref() == Some(distro) {
            self.reset_locked();
        }
    }

    /// Switches to another distro. Whatever was known or being started for
    /// the old one is dropped in the same step. Its keep-alive is not
    /// necessarily: see `select_locked`.
    pub fn select(&self, distro: String) {
        let _gate = self.gate.lock().unwrap();
        self.select_locked(Some(distro));
    }

    /// Every change of distro goes through here, by hand or automatic.
    fn select_locked(&self, distro: Option<String>) {
        // Which distro the window shows and which distros Pier keeps awake
        // are two different things. Looking at another distro must not take
        // down the one that was selected: if its engine was up and it may
        // have containers running (or "keep running" is on), it stays held,
        // in the background, until a later check finds it idle.
        //
        // The rule is written from the keep-alive's side: one that is held is
        // only let go of here when Pier positively knows nothing depends on
        // it. In every other case it goes on being held, in the background.
        // What Pier happens to know about the engine at this instant is not
        // a safe guide by itself: right after a switch it knows nothing yet
        // (`expected` and `up` were just cleared), so reading "not up" as
        // "nothing running" let a quick A → B → A → B take down A's containers.
        let old = self.distro.lock().unwrap().clone();
        if let Some(old) = old.filter(|old| Some(old) != distro.as_ref()) {
            let in_background = self.background.lock().unwrap().contains_key(&old);
            // Known to be idle: the event stream is open (so every container
            // start would have been seen), the count is fresh, and it is zero.
            let known_idle = self.events_live.load(Ordering::Relaxed)
                && !self.count_stale.load(Ordering::Relaxed)
                && self.running_now() == 0;
            if !self.holds(&old) || in_background {
                // Nothing held, or already looked after by the background
                // check from an earlier switch: leave it exactly as it is.
            } else if self.refuses(&old) || (known_idle && !self.keep_running.load(Ordering::Relaxed)) {
                self.drop_keepalive(&old);
            } else {
                self.background.lock().unwrap().insert(old, Instant::now());
            }
        }
        // Stop marks are per distro, so there is nothing to carry over or
        // clear here: the newly selected distro has its own, usually none.
        self.reset_locked();
        // A distro that is selected again keeps its background record for
        // now. Selecting it has just wiped what was known about its engine,
        // so nothing in the foreground is watching its keep-alive yet; the
        // record goes when the engine is seen running again (`adopted`).
        // If that never happens (the window is closed first, say), the
        // background check still lets go of it once it is idle.
        *self.distro.lock().unwrap() = distro;
    }

    /// Looks again at the distros held in the background, each at most once
    /// per `every`, and lets go of those whose engine has nothing running.
    /// One that does not answer is kept: not knowing is not a reason to risk
    /// its containers.
    async fn review_background(&self, every: Duration) {
        let due: Vec<(String, Instant)> = {
            let background = self.background.lock().unwrap();
            background.iter().filter(|(_, seen)| seen.elapsed() >= every).map(|(d, seen)| (d.clone(), *seen)).collect()
        };
        // Two at a time: one distro whose Docker is slow to answer does not
        // hold up all the others, and no more than two relays run for this.
        for pair in due.chunks(2) {
            let first = self.review_one(&pair[0].0, pair[0].1);
            let second = async {
                if let Some((distro, seen)) = pair.get(1) {
                    self.review_one(distro, *seen).await;
                }
            };
            tokio::join!(first, second);
        }
    }

    /// One background distro: `seen` is the record this check starts from.
    async fn review_one(&self, distro: &str, seen: Instant) {
        // If its keep-alive process has gone, the distro was shut down from
        // outside. Asking Docker would only boot it again.
        let gone = self.keepalive.lock().unwrap().get_mut(distro).is_some_and(|child| match child {
            Some(c) => !still_running(c),
            None => false, // nothing to watch (tests, non-Windows): assume it is there
        });
        if gone || !self.holds(distro) {
            self.drop_keepalive(distro);
            return;
        }
        let counted = (self.backend.count)(distro.to_string()).await;
        // Everything the decision rests on is read now, after the wait, and
        // under the gate: the record must be the very one this check started
        // from (not removed, not renewed by a switch in the meantime), and
        // "keep running" is whatever it is at this moment, not what it was
        // when the question was sent.
        let _gate = self.gate.lock().unwrap();
        if self.background.lock().unwrap().get(distro) != Some(&seen) {
            return;
        }
        if counted == Some(0) && !self.keep_running.load(Ordering::Relaxed) {
            eprintln!("engine: {distro} has nothing running any more, letting it go");
            self.drop_keepalive(distro);
        } else {
            self.background.lock().unwrap().insert(distro.to_string(), Instant::now());
        }
    }

    // ---- going to sleep --------------------------------------------------

    fn running_now(&self) -> usize {
        #[cfg(debug_assertions)]
        if std::env::var_os("PIER_DEBUG_ASSUME_NO_CONTAINERS").is_some() {
            return 0; // lets the idle path be exercised on a machine with containers running
        }
        self.running.load(Ordering::Relaxed)
    }

    /// Counts running containers again if a container event arrived since
    /// the last count. On failure the count stays stale, which blocks sleep.
    async fn recount(&self) {
        if !self.count_stale.swap(false, Ordering::Relaxed) {
            return;
        }
        let counted = match self.distro() {
            Ok(d) => (self.backend.count)(d).await,
            Err(_) => None,
        };
        match counted {
            Some(running) => self.running.store(running, Ordering::Relaxed),
            None => self.count_stale.store(true, Ordering::Relaxed),
        }
    }

    fn in_use(&self) -> bool {
        self.window_open.load(Ordering::Relaxed) || self.connections() > 0
    }

    /// The one place the engine is put to sleep. Everything is read again
    /// here, under the gate: a connection that arrived, or a container event
    /// that made the count stale, while the containers were being counted
    /// cancels it.
    fn try_sleep(&self, idle_after: Duration) -> bool {
        let _gate = self.gate.lock().unwrap();
        let fresh = !self.count_stale.load(Ordering::Relaxed);
        let idle_for = self.activity.lock().unwrap().elapsed();
        let sleep = self.events_live.load(Ordering::Relaxed)
            && !self.keep_running.load(Ordering::Relaxed)
            && fresh
            && should_sleep(
                self.window_open.load(Ordering::Relaxed),
                self.connections(),
                self.running_now(),
                idle_for,
                idle_after,
            );
        if sleep {
            eprintln!("engine: idle for {}s, letting the distro go", idle_for.as_secs());
            // A new epoch: the event stream that is open right now belongs to
            // the old one and must close, even if the engine is woken again
            // before the stream next looks. Otherwise it would live on without
            // `events_live`, and the idle monitor would never look again.
            self.epoch.fetch_add(1, Ordering::Relaxed);
            self.sleeping.store(true, Ordering::Relaxed);
            self.expected.store(false, Ordering::Relaxed);
            self.up.store(false, Ordering::Relaxed);
            self.events_live.store(false, Ordering::Relaxed);
            // The keep-alive is let go of by `release_asleep`, in turn with
            // starts and stops.
        }
        sleep
    }

    /// Finishes putting the engine to sleep: lets go of Pier's own hold on
    /// the distro, and nothing else.
    ///
    /// Pier never stops Docker by itself. Its view of "idle" (no container
    /// running, nothing on its pipe, no window) misses Docker being used
    /// from inside the distro: a `docker build` or `docker pull` typed into
    /// a shell there connects straight to the socket, at any moment, and
    /// stopping dockerd would cut it off. No check beforehand can rule that
    /// out, so there is none, and no stop. Only the Stop button stops it.
    ///
    /// What that means for memory depends on the distro. With systemd, WSL
    /// shuts the distro down once nothing else is in it, services and all,
    /// and a task run from a shell keeps it up by itself. Without systemd,
    /// dockerd is an ordinary process and WSL does not shut a distro down
    /// while one runs (found by running it for real): there, nothing is
    /// given back until the user presses Stop.
    ///
    /// It queues with starts and stops. If a wake-up got in first, the engine
    /// is no longer asleep and nothing is done.
    async fn release_asleep(&self) {
        let Ok(distro) = self.distro() else { return };
        let _one = self.starting.lock().await;
        let _gate = self.gate.lock().unwrap();
        if self.sleeping.load(Ordering::Relaxed) && self.distro().ok().as_deref() == Some(distro.as_str()) {
            self.drop_keepalive(&distro);
        }
    }

    /// Whether the engine is kept running even when nothing uses it.
    pub fn keeps_running(&self) -> bool {
        self.keep_running.load(Ordering::Relaxed)
    }

    /// Taken under the gate, like everything else the sleep decision reads:
    /// once this returns, an idle check that had not yet decided will not sleep.
    pub fn set_keep_running(&self, keep: bool) {
        let _gate = self.gate.lock().unwrap();
        self.keep_running.store(keep, Ordering::Relaxed);
        self.touch();
    }

    /// Runs for the life of the app: the idle rule for the selected engine,
    /// and the look at distros held in the background. The two run side by
    /// side, so background distros that are slow to answer never delay
    /// putting the selected one to sleep.
    pub async fn watch_idle(&self) {
        let idle_after = idle_after();
        let every = IDLE_CHECK.min(idle_after).max(Duration::from_secs(1));
        let background = async {
            loop {
                tokio::time::sleep(every).await;
                self.review_background(idle_after).await;
            }
        };
        tokio::join!(self.watch_selected(idle_after, every), background);
    }

    /// Only looks at an engine whose event stream is open, so it can never
    /// boot a stopped distro.
    async fn watch_selected(&self, idle_after: Duration, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            self.idle_step(idle_after).await;
        }
    }

    /// One look at the selected engine, and putting it to sleep if it is idle.
    async fn idle_step(&self, idle_after: Duration) {
        if !self.events_live.load(Ordering::Relaxed) {
            return;
        }
        if !self.in_use() {
            self.recount().await; // one request, and only after a container event
        }
        if self.in_use() || self.count_stale.load(Ordering::Relaxed) || self.running_now() > 0 {
            self.touch();
            return;
        }
        if self.try_sleep(idle_after) {
            self.release_asleep().await;
        }
    }
}

/// The whole idle rule in one place.
fn should_sleep(window_open: bool, connections: usize, running: usize, idle_for: Duration, idle_after: Duration) -> bool {
    !window_open && connections == 0 && running == 0 && idle_for >= idle_after
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// An engine that looks idle in every respect: `try_sleep(0)` succeeds.
    fn idle_engine() -> Engine {
        let engine = engine_recording_starts("test", Ok(())).0; // never the real WSL
        engine.window_open.store(false, Ordering::Relaxed);
        engine.events_live.store(true, Ordering::Relaxed);
        engine.expected.store(true, Ordering::Relaxed);
        engine.up.store(true, Ordering::Relaxed);
        engine.count_stale.store(false, Ordering::Relaxed);
        engine
    }

    /// Stands in for WSL and Docker: starting blocks until `release` is
    /// notified, after which the engine answers at once.
    fn engine_with_slow_start(distro: &str) -> (Arc<Engine>, Arc<tokio::sync::Notify>) {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let backend = Backend {
            start: Box::new(move |_| {
                let gate = gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(())
                })
            }),
            stop: Box::new(|_| Box::pin(async { Ok(()) })),
            terminate: Box::new(|_| Box::pin(async { Ok(()) })),
            ping: Box::new(|_| Box::pin(async { true })),
            keepalive: Box::new(|_| None),
            count: Box::new(|_| Box::pin(async { Some(0) })),
            daemon_alive: Box::new(|_| Box::pin(async { Some(false) })),
        };
        (Arc::new(Engine::with_backend(Some(distro.into()), backend)), release)
    }

    /// Runs `start()` in the background and waits until it is inside the backend.
    async fn begin_start(engine: &Arc<Engine>) -> tokio::task::JoinHandle<Result<(), String>> {
        let task = tokio::spawn({
            let engine = engine.clone();
            async move {
                let distro = engine.distro().unwrap();
                engine.start(&distro).await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        task
    }

    #[tokio::test]
    async fn a_connection_waits_for_a_manual_start() {
        let (engine, release) = engine_with_slow_start("ubuntu");
        let start = begin_start(&engine).await;

        let connection = tokio::spawn({
            let engine = engine.clone();
            async move { engine.wake("ubuntu").await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!connection.is_finished(), "must not connect while the Start button's start is running");

        release.notify_one();
        assert!(start.await.unwrap().is_ok());
        connection.await.unwrap();
        assert!(engine.up.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_start_for_the_previous_distro_is_not_recorded() {
        let (engine, release) = engine_with_slow_start("old");
        let start = begin_start(&engine).await;

        engine.select("new".into());
        release.notify_one();

        assert_eq!(start.await.unwrap(), Err(CANCELLED.to_string()));
        assert_eq!(engine.distro().unwrap(), "new");
        assert!(!engine.up.load(Ordering::Relaxed), "the new distro was never started");
        assert!(!engine.expected.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn stopping_during_a_start_wins() {
        let (engine, release) = engine_with_slow_start("ubuntu");
        let start = begin_start(&engine).await;

        let stop = tokio::spawn({
            let engine = engine.clone();
            async move { engine.stop("ubuntu", false).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!stop.is_finished(), "stop waits for the start to get out of the way");
        release.notify_one();

        assert_eq!(start.await.unwrap(), Err(CANCELLED.to_string()));
        assert!(stop.await.unwrap().is_ok());
        assert!(!engine.up.load(Ordering::Relaxed));
        assert!(!engine.expected.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_status_poll_cannot_cancel_a_manual_start() {
        let (engine, release) = engine_with_slow_start("ubuntu");
        let start = begin_start(&engine).await;
        engine.seen_stopped(&engine.target().unwrap()); // the window polls while Docker is still coming up
        release.notify_one();
        assert!(start.await.unwrap().is_ok());
        assert!(engine.up.load(Ordering::Relaxed));
    }

    /// Records every distro the stop script ran in.
    fn engine_recording_stops(distro: &str) -> (Arc<Engine>, Arc<tokio::sync::Notify>, Arc<Mutex<Vec<String>>>) {
        let (engine, release) = engine_with_slow_start(distro);
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let log = stopped.clone();
        let mut engine = Arc::into_inner(engine).unwrap();
        engine.backend.stop = Box::new(move |d| {
            log.lock().unwrap().push(d);
            Box::pin(async { Ok(()) })
        });
        (Arc::new(engine), release, stopped)
    }

    #[tokio::test]
    async fn a_queued_stop_keeps_its_target_through_a_distro_switch() {
        let (engine, release, stopped) = engine_recording_stops("a");
        let start = begin_start(&engine).await;
        let stop = tokio::spawn({
            let engine = engine.clone();
            async move { engine.stop("a", false).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        engine.select("b".into()); // while the stop for "a" waits its turn
        let b = engine.target().unwrap();
        engine.seen_running(&b);
        release.notify_one();
        let _ = start.await.unwrap();
        stop.await.unwrap().unwrap();

        assert_eq!(*stopped.lock().unwrap(), vec!["a".to_string()], "the stop ran where it was asked to");
        assert!(engine.up.load(Ordering::Relaxed), "and left the newly selected distro alone");
    }

    #[test]
    fn a_probe_of_the_previous_distro_is_not_recorded() {
        let engine = Engine::with_backend(Some("a".into()), engine_backend_that_never_holds());
        let probe_of_a = engine.target().unwrap(); // status() starts probing "a"
        engine.select("b".into());
        engine.seen_running(&probe_of_a); // ...and its answer arrives after the switch
        assert!(!engine.up.load(Ordering::Relaxed));
        assert!(!engine.expected.load(Ordering::Relaxed));
        assert_eq!(*HELD.lock().unwrap(), Vec::<String>::new(), "no keep-alive asked for the old distro");
    }

    #[test]
    fn an_event_stream_of_the_previous_distro_is_dropped() {
        let engine = idle_engine();
        let stream = engine.target().unwrap();
        engine.select("other".into());
        assert!(!engine.events_connected(&stream));
        assert!(!engine.events_live.load(Ordering::Relaxed));
    }

    #[test]
    fn requests_for_another_distro_are_refused() {
        let engine = Engine::new(Some("b".into()));
        assert!(engine.require("b").is_ok());
        assert!(engine.require("a").is_err(), "a delete meant for a must not reach b");
    }

    #[tokio::test]
    async fn a_start_that_never_answers_gives_up_within_its_budget() {
        let mut engine = Engine::with_backend(
            Some("ubuntu".into()),
            Backend {
                start: Box::new(|_| Box::pin(async { Ok(()) })),
                stop: Box::new(|_| Box::pin(async { Ok(()) })),
            terminate: Box::new(|_| Box::pin(async { Ok(()) })),
                // Docker accepts the connection and then says nothing.
                ping: Box::new(|_| Box::pin(std::future::pending())),
                keepalive: Box::new(|_| None),
                count: Box::new(|_| Box::pin(async { Some(0) })),
                daemon_alive: Box::new(|_| Box::pin(async { Some(false) })),
            },
        );
        engine.start_timeout = Duration::from_millis(100);
        let began = std::time::Instant::now();
        assert!(engine.start("ubuntu").await.is_err());
        assert!(began.elapsed() < Duration::from_secs(2), "took {:?}", began.elapsed());
        assert!(!engine.up.load(Ordering::Relaxed));
        // The lock is free again for whatever was queued behind it.
        assert!(engine.starting.try_lock().is_ok());
    }

    /// Keep-alives asked for by `engine_backend_that_never_holds`.
    static HELD: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn engine_backend_that_never_holds() -> Backend {
        Backend {
            start: Box::new(|_| Box::pin(async { Ok(()) })),
            stop: Box::new(|_| Box::pin(async { Ok(()) })),
            terminate: Box::new(|_| Box::pin(async { Ok(()) })),
            ping: Box::new(|_| Box::pin(async { true })),
            keepalive: Box::new(|d| {
                HELD.lock().unwrap().push(d.to_string());
                None
            }),
            count: Box::new(|_| Box::pin(async { Some(0) })),
            daemon_alive: Box::new(|_| Box::pin(async { Some(false) })),
        }
    }

    #[tokio::test]
    async fn one_failed_probe_does_not_let_go_of_a_running_engine() {
        let engine = idle_engine();
        let target = engine.target().unwrap();

        assert!(!engine.probe_failed(&target), "one timeout proves nothing");
        assert!(engine.expected.load(Ordering::Relaxed), "still reconnecting");
        assert!(engine.up.load(Ordering::Relaxed));
        assert!(!engine.probe_failed(&target));

        // An answer in between starts the count again.
        engine.seen_running(&target);
        assert!(!engine.probe_failed(&target));
        assert!(!engine.probe_failed(&target));
        assert!(engine.expected.load(Ordering::Relaxed));

        // The third failure in a row is when to find out. Here dockerd is
        // gone (the test backend says so), so the engine is recorded as down.
        assert!(engine.probe_failed(&target));
        assert!(engine.expected.load(Ordering::Relaxed), "not let go of before it is confirmed");
        assert!(engine.confirm_down(&target).await);
        assert!(!engine.expected.load(Ordering::Relaxed));
        assert!(!engine.up.load(Ordering::Relaxed));
        assert!(!engine.holds("test"));
    }

    #[test]
    fn a_reconnected_event_stream_starts_the_failure_count_again() {
        // The reviewer's order: fail, fail, reconnect, fail, fail, reconnect, fail.
        let engine = idle_engine();
        let target = engine.target().unwrap();
        for _ in 0..3 {
            assert!(!engine.probe_failed(&target));
            assert!(!engine.probe_failed(&target));
            assert!(engine.events_connected(&target));
        }
        assert!(!engine.probe_failed(&target), "never three in a row");
        assert!(engine.expected.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_docker_that_hangs_but_is_there_keeps_its_distro_awake() {
        let mut engine = idle_engine();
        engine.backend.daemon_alive = Box::new(|_| Box::pin(async { Some(true) })); // dockerd runs, just does not answer
        let target = engine.target().unwrap();
        engine.seen_running(&target);
        assert!(engine.holds("test"));

        for _ in 0..6 {
            if engine.probe_failed(&target) {
                assert!(!engine.confirm_down(&target).await);
            }
        }
        assert!(engine.holds("test"), "its containers may be running: the keep-alive stays");
        assert!(engine.expected.load(Ordering::Relaxed), "and the watcher keeps trying");
        assert!(!engine.up.load(Ordering::Relaxed), "but it is shown as not running");

        // Not being able to tell is treated the same way.
        engine.backend.daemon_alive = Box::new(|_| Box::pin(async { None }));
        assert!(!engine.confirm_down(&target).await);
        assert!(engine.holds("test"));
    }

    /// An engine that is up in `distro`, held, with `running` containers.
    fn busy_engine(distro: &str, running: usize) -> Engine {
        let (engine, _) = engine_recording_starts(distro, Ok(()));
        engine.window_open.store(false, Ordering::Relaxed);
        engine.seen_running(&engine.target().unwrap());
        engine.running.store(running, Ordering::Relaxed);
        engine.count_stale.store(false, Ordering::Relaxed);
        engine
    }

    #[test]
    fn switching_away_from_a_distro_with_containers_keeps_it_awake() {
        let engine = busy_engine("a", 2);
        engine.select("b".into());
        assert!(engine.holds("a"), "a still has containers running");
        assert!(!engine.expected.load(Ordering::Relaxed), "nothing is assumed about b");

        // Back on a: still held, and still on the background list, because
        // nothing in the foreground is watching it yet.
        engine.select("a".into());
        assert!(engine.holds("a"));
        assert!(engine.background.lock().unwrap().contains_key("a"));

        // Once its engine is seen running the foreground has taken over.
        engine.seen_running(&engine.target().unwrap());
        assert!(engine.holds("a"));
        assert!(engine.background.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_distro_switched_back_to_but_never_seen_again_is_still_let_go() {
        // Back on a, and the window is closed before any status came in: no
        // event stream, nothing expected, yet the keep-alive is there.
        let engine = busy_engine("a", 2);
        engine.select("b".into());
        engine.select("a".into());
        engine.window_closed();
        assert!(engine.holds("a") && !engine.expected.load(Ordering::Relaxed));

        // The background check is what still watches it. (The test backend
        // reports no containers.)
        engine.review_background(Duration::ZERO).await;
        assert!(!engine.holds("a"), "not held for ever with nobody watching");
    }

    /// A value the test hands out when it chooses, standing in for a slow answer.
    fn answered_later<T: Clone + Send + Sync + 'static>(
        answer: T,
    ) -> (Arc<tokio::sync::Notify>, Box<dyn Fn(String) -> Task<T> + Send + Sync>) {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let ask: Box<dyn Fn(String) -> Task<T> + Send + Sync> = Box::new(move |_| {
            let (gate, answer) = (gate.clone(), answer.clone());
            Box::pin(async move {
                gate.notified().await;
                answer
            })
        });
        (release, ask)
    }

    #[tokio::test]
    async fn a_health_check_overtaken_by_a_reconnect_changes_nothing() {
        for dockerd_there in [Some(true), Some(false), None] {
            let mut engine = idle_engine();
            let (release, ask) = answered_later(dockerd_there);
            engine.backend.daemon_alive = ask;
            let engine = Arc::new(engine);
            let target = engine.target().unwrap();
            engine.seen_running(&target);

            let check = tokio::spawn({
                let (engine, target) = (engine.clone(), target.clone());
                async move { engine.confirm_down(&target).await }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;

            // Docker comes back while the check is still looking for dockerd.
            assert!(engine.events_connected(&target));
            release.notify_one();

            assert!(!check.await.unwrap(), "{dockerd_there:?}: an out-of-date check confirms nothing");
            assert!(engine.events_live.load(Ordering::Relaxed), "{dockerd_there:?}: the open stream stays registered");
            assert!(engine.up.load(Ordering::Relaxed));
            assert!(engine.holds("test"));
        }
    }

    #[tokio::test]
    async fn keep_running_switched_on_during_a_background_check_is_honoured() {
        let mut engine = busy_engine("a", 2);
        let (release, ask) = answered_later(Some(0usize)); // the answer will be "no containers"
        engine.backend.count = ask;
        let engine = Arc::new(engine);
        engine.select("b".into());

        let review = tokio::spawn({
            let engine = engine.clone();
            async move { engine.review_background(Duration::ZERO).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        engine.set_keep_running(true); // while the question is out
        release.notify_one();
        review.await.unwrap();

        assert!(engine.holds("a"), "keep running was on by the time the answer came");
    }

    #[tokio::test]
    async fn a_background_check_does_not_act_on_a_record_that_was_replaced() {
        let mut engine = busy_engine("a", 2);
        let (release, ask) = answered_later(Some(0usize));
        engine.backend.count = ask;
        let engine = Arc::new(engine);
        engine.select("b".into());

        let review = tokio::spawn({
            let engine = engine.clone();
            async move { engine.review_background(Duration::ZERO).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // While the question is out, a is selected, seen running with
        // containers, and left again: a new background record, a new story.
        engine.select("a".into());
        engine.seen_running(&engine.target().unwrap());
        engine.running.store(3, Ordering::Relaxed);
        engine.count_stale.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(5)).await;
        engine.select("b".into());
        release.notify_one();
        review.await.unwrap();

        assert!(engine.holds("a"), "the old answer is not about this record");
    }

    #[tokio::test]
    async fn one_slow_background_distro_does_not_hold_up_another() {
        let mut engine = busy_engine("slow", 2);
        engine.backend.count = Box::new(|distro| {
            Box::pin(async move {
                if distro == "slow" {
                    std::future::pending::<()>().await; // never answers
                }
                Some(0)
            })
        });
        // Two distros held in the background, "slow" first.
        engine.select("quick".into());
        engine.seen_running(&engine.target().unwrap());
        engine.running.store(1, Ordering::Relaxed);
        engine.count_stale.store(false, Ordering::Relaxed);
        engine.select("selected".into());
        assert!(engine.holds("slow") && engine.holds("quick"));

        let _ = tokio::time::timeout(Duration::from_millis(200), engine.review_background(Duration::ZERO)).await;
        assert!(!engine.holds("quick"), "checked and let go although \"slow\" never answered");
        assert!(engine.holds("slow"));
    }

    #[test]
    fn switching_quickly_back_and_forth_never_lets_go_of_a_distro_with_containers() {
        let engine = busy_engine("a", 2);
        // A → B → A → B, with no status answer in between: each time a is
        // selected again Pier knows nothing about its engine yet.
        for _ in 0..5 {
            engine.select("b".into());
            assert!(engine.holds("a"));
            engine.select("a".into());
            assert!(engine.holds("a"));
        }
        engine.select("b".into());
        assert!(engine.holds("a"), "a's containers still depend on it");
        assert!(engine.background.lock().unwrap().contains_key("a"), "and the background check still watches it");

        // Three distros, round and round, b and c also busy.
        let engine = busy_engine("a", 2);
        for next in ["b", "c", "a", "c", "b", "a", "b"] {
            engine.select(next.into());
            if next != "a" {
                engine.seen_running(&engine.target().unwrap()); // its engine is up, containers unknown
            }
        }
        for distro in ["a", "b", "c"] {
            assert!(engine.holds(distro), "{distro} was let go of");
        }
    }

    #[test]
    fn a_switch_away_before_the_event_stream_is_back_keeps_the_distro() {
        // The count says zero, but the event stream is not open, so a
        // container may have been started unseen: not known to be idle.
        let engine = busy_engine("a", 0);
        engine.select("b".into());
        assert!(engine.holds("a"));
    }

    /// `busy_engine` with its event stream open, so its count can be trusted.
    fn watched_engine(distro: &str, running: usize) -> Engine {
        let engine = busy_engine(distro, running);
        assert!(engine.events_connected(&engine.target().unwrap()));
        engine.count_stale.store(false, Ordering::Relaxed);
        engine
    }

    #[test]
    fn switching_away_from_an_idle_distro_lets_it_go_unless_told_to_keep_it() {
        let engine = watched_engine("a", 0);
        engine.select("b".into());
        assert!(!engine.holds("a"), "nothing running there, and Pier knows it");

        let engine = watched_engine("a", 0);
        engine.set_keep_running(true);
        engine.select("b".into());
        assert!(engine.holds("a"), "keep running is on");

        // Not knowing how many containers run counts as "may have work".
        let engine = busy_engine("a", 0);
        engine.count_stale.store(true, Ordering::Relaxed);
        engine.select("b".into());
        assert!(engine.holds("a"));
    }

    #[tokio::test]
    async fn a_distro_in_the_background_is_let_go_once_it_is_idle() {
        let counts = Arc::new(Mutex::new(vec![Some(0), None, Some(2)])); // answers, last first
        let answers = counts.clone();
        let mut engine = busy_engine("a", 2);
        engine.backend.count = Box::new(move |_| {
            let next = answers.lock().unwrap().pop().unwrap_or(Some(0));
            Box::pin(async move { next })
        });
        engine.select("b".into());

        engine.review_background(Duration::ZERO).await; // 2 containers
        assert!(engine.holds("a"));
        engine.review_background(Duration::ZERO).await; // Docker did not answer
        assert!(engine.holds("a"), "not knowing is not a reason to let go");
        engine.review_background(Duration::ZERO).await; // none left
        assert!(!engine.holds("a"));
        assert!(engine.background.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopping_a_distro_held_in_the_background_lets_go_of_it() {
        let engine = busy_engine("a", 2);
        engine.select("b".into());
        engine.seen_running(&engine.target().unwrap());
        engine.stop("a", false).await.unwrap();
        assert!(!engine.holds("a"));
        assert!(engine.holds("b"), "b is untouched");
        assert!(engine.up.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn failures_counted_against_one_engine_are_not_another_engines() {
        // The event watcher keeps its failure count for one target and starts
        // over when the target is a different one. That holds for another
        // distro, and for the same distro after a stop or a sleep.
        let engine = busy_engine("a", 1);
        let a = engine.target().unwrap();
        assert_eq!(a, engine.target().unwrap(), "unchanged while nothing happens");

        engine.select("b".into());
        let b = engine.target().unwrap();
        assert_ne!(a, b);

        engine.select("a".into());
        assert_ne!(a, engine.target().unwrap(), "a again, but a new chapter");

        let before_stop = engine.target().unwrap();
        engine.stop("a", false).await.unwrap();
        assert_ne!(before_stop, engine.target().unwrap());
    }

    #[test]
    fn a_target_is_no_longer_good_after_a_stop_or_a_switch() {
        let engine = idle_engine();
        let target = engine.target().unwrap();
        assert!(engine.still(&target));
        engine.select("other".into());
        assert!(!engine.still(&target), "switched away");

        let engine = idle_engine();
        let target = engine.target().unwrap();
        engine.halted.lock().unwrap().insert("test".into());
        assert!(!engine.still(&target), "stopped on purpose");
    }

    #[test]
    fn a_stopped_engine_releases_the_keep_alive_state() {
        let engine = idle_engine();
        engine.seen_stopped(&engine.target().unwrap());
        assert!(!engine.expected.load(Ordering::Relaxed));
        assert!(!engine.events_live.load(Ordering::Relaxed));
    }

    #[test]
    fn sleeps_only_when_nothing_needs_the_engine() {
        let limit = Duration::from_secs(300);
        let long = Duration::from_secs(301);
        assert!(should_sleep(false, 0, 0, long, limit));
        assert!(!should_sleep(true, 0, 0, long, limit), "window open");
        assert!(!should_sleep(false, 1, 0, long, limit), "CLI connected");
        assert!(!should_sleep(false, 0, 2, long, limit), "containers running");
        assert!(!should_sleep(false, 0, 0, Duration::from_secs(299), limit), "not idle long enough");
    }

    #[test]
    fn keep_running_stops_an_idle_engine_from_sleeping() {
        let engine = idle_engine();
        engine.set_keep_running(true);
        assert!(!engine.try_sleep(Duration::ZERO));
        assert!(!engine.is_asleep());
        engine.set_keep_running(false);
        assert!(engine.try_sleep(Duration::ZERO), "and switching it off lets it rest again");
    }

    #[tokio::test]
    async fn going_to_sleep_lets_go_of_the_distro_and_stops_nothing_in_it() {
        // A docker build or pull typed into a shell in the distro may be
        // running: Pier cannot see it, so Pier never stops Docker by itself.
        let (engine, _, stopped) = engine_recording_stops("a");
        let mut engine = Arc::into_inner(engine).unwrap();
        engine.backend.start = Box::new(|_| Box::pin(async { Ok(()) }));
        engine.backend.terminate = Box::new(|_| panic!("going to sleep must not shut the distro down"));
        engine.window_open.store(false, Ordering::Relaxed);
        let target = engine.target().unwrap();
        engine.seen_running(&target);
        assert!(engine.events_connected(&target));
        engine.count_stale.store(false, Ordering::Relaxed);

        engine.idle_step(Duration::ZERO).await;
        assert!(stopped.lock().unwrap().is_empty(), "Docker was stopped: {:?}", stopped.lock().unwrap());
        assert!(!engine.holds("a"), "Pier's own hold is given up");
        assert!(engine.is_asleep());

        // It is a sleep, not a stop: the next connection brings it back.
        assert!(engine.wake("a").await);
        assert!(engine.up.load(Ordering::Relaxed));
        assert!(engine.holds("a"));
    }

    #[tokio::test]
    async fn a_wake_up_that_gets_in_first_cancels_the_rest_of_the_sleep() {
        let (engine, _, _) = engine_recording_stops("a");
        let mut engine = Arc::into_inner(engine).unwrap();
        engine.backend.start = Box::new(|_| Box::pin(async { Ok(()) }));
        engine.window_open.store(false, Ordering::Relaxed);
        let target = engine.target().unwrap();
        engine.seen_running(&target);
        assert!(engine.events_connected(&target));
        engine.count_stale.store(false, Ordering::Relaxed);

        assert!(engine.try_sleep(Duration::ZERO));
        assert!(engine.wake("a").await); // a docker command arrives before the distro is let go of
        engine.release_asleep().await;
        assert!(engine.holds("a"), "the distro is not let go of under a tool that just connected");
        assert!(engine.up.load(Ordering::Relaxed));
    }

    #[test]
    fn an_idle_engine_goes_to_sleep() {
        let engine = idle_engine();
        assert!(engine.try_sleep(Duration::ZERO));
        assert!(engine.is_asleep());
        assert!(!engine.expected.load(Ordering::Relaxed));
        assert!(!engine.events_live.load(Ordering::Relaxed));
    }

    #[test]
    fn a_connection_that_arrives_before_the_decision_cancels_sleep() {
        // The monitor saw no connections, then counted containers; meanwhile
        // a tool connected. The decision must see it, even with no idle time.
        let engine = idle_engine();
        engine.connected("test");
        assert!(!engine.try_sleep(Duration::ZERO));
        assert!(!engine.is_asleep());
        engine.disconnected("test");
        assert!(engine.try_sleep(Duration::ZERO));
    }

    #[test]
    fn a_container_event_during_the_count_cancels_sleep() {
        let engine = idle_engine();
        engine.count_stale.store(true, Ordering::Relaxed);
        assert!(!engine.try_sleep(Duration::ZERO));
    }

    #[test]
    fn the_window_opening_cancels_sleep() {
        let engine = idle_engine();
        engine.window_opened();
        assert!(!engine.try_sleep(Duration::ZERO));
    }

    #[test]
    fn a_status_poll_cannot_cancel_a_wake_up() {
        let engine = idle_engine();
        assert!(engine.try_sleep(Duration::ZERO));
        engine.seen_stopped(&engine.target().unwrap());
        assert!(engine.is_asleep(), "still asleep, so the next connection wakes it");
    }

    #[tokio::test]
    async fn wake_does_nothing_unless_sleeping() {
        let (engine, starts) = engine_recording_starts("ubuntu", Err("no dockerd".into()));
        engine.wake("ubuntu").await;
        assert!(starts.lock().unwrap().is_empty(), "nothing was started");
        assert!(!engine.is_asleep());
    }

    #[tokio::test]
    async fn failed_wake_is_not_retried() {
        let (engine, starts) = engine_recording_starts("ubuntu", Err("no dockerd".into()));
        engine.sleeping.store(true, Ordering::Relaxed);
        engine.wake("ubuntu").await;
        engine.wake("ubuntu").await;
        assert_eq!(starts.lock().unwrap().len(), 1, "one attempt, then it counts as stopped");
        assert!(!engine.is_asleep());
        assert!(!engine.up.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_connection_to_another_distro_wakes_nothing() {
        let (engine, starts) = engine_recording_starts("b", Ok(()));
        engine.sleeping.store(true, Ordering::Relaxed);
        engine.wake("a").await; // a tool still attached to the previously selected distro
        assert!(starts.lock().unwrap().is_empty());
        assert!(engine.is_asleep(), "the selected distro is left asleep");
    }

    #[tokio::test]
    async fn a_queued_start_does_not_follow_a_distro_switch() {
        // Start is pressed twice on "a"; the second waits behind the first.
        // Then "b" is selected. Neither start may run in "b".
        let release = Arc::new(tokio::sync::Notify::new());
        let starts = Arc::new(Mutex::new(Vec::new()));
        let (gate, log) = (release.clone(), starts.clone());
        let engine = Arc::new(Engine::with_backend(
            Some("a".into()),
            Backend {
                start: Box::new(move |d| {
                    log.lock().unwrap().push(d);
                    let gate = gate.clone();
                    Box::pin(async move {
                        gate.notified().await;
                        Ok(())
                    })
                }),
                stop: Box::new(|_| Box::pin(async { Ok(()) })),
            terminate: Box::new(|_| Box::pin(async { Ok(()) })),
                ping: Box::new(|_| Box::pin(async { true })),
                keepalive: Box::new(|_| None),
                count: Box::new(|_| Box::pin(async { Some(0) })),
                daemon_alive: Box::new(|_| Box::pin(async { Some(false) })),
            },
        ));
        let first = begin_start(&engine).await;
        let second = begin_start(&engine).await;

        engine.select("b".into());
        release.notify_one();
        release.notify_one();

        assert_eq!(first.await.unwrap(), Err(CANCELLED.to_string()));
        assert_eq!(second.await.unwrap(), Err(CANCELLED.to_string()));
        assert_eq!(*starts.lock().unwrap(), vec!["a".to_string()], "the start script never ran in b");
        assert!(!engine.up.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_start_asked_for_another_distro_is_refused() {
        let (engine, starts) = engine_recording_starts("b", Ok(()));
        assert_eq!(engine.start("a").await, Err(CANCELLED.to_string()));
        assert!(starts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_event_stream_from_before_a_sleep_does_not_survive_a_quick_wake() {
        // Order of events: the stream is open, the engine goes to sleep, and
        // it is woken again before the stream has looked (it looks every 2 s).
        let (engine, _) = engine_recording_starts("test", Ok(()));
        engine.window_open.store(false, Ordering::Relaxed);
        engine.expected.store(true, Ordering::Relaxed);
        engine.count_stale.store(false, Ordering::Relaxed);
        let stream = engine.target().unwrap();
        assert!(engine.events_connected(&stream));
        engine.count_stale.store(false, Ordering::Relaxed);

        assert!(engine.try_sleep(Duration::ZERO));
        assert!(engine.wake("test").await);
        assert!(engine.expected.load(Ordering::Relaxed), "awake again");

        assert!(!engine.wants_events(&stream), "the old stream must close");
        // ...so that the watcher connects a new one, which registers itself.
        assert!(engine.events_connected(&engine.target().unwrap()));
        assert!(engine.events_live.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn pipe_connections_are_turned_away_after_a_deliberate_stop() {
        let (engine, starts) = engine_recording_starts("ubuntu", Ok(()));
        assert!(engine.wake("ubuntu").await, "before any stop, connections go through");

        engine.stop("ubuntu", false).await.unwrap();
        assert!(!engine.wake("ubuntu").await, "an IDE probing the pipe must not boot the distro again");
        assert!(engine.wake("other").await, "a connection bound to another distro is not this engine's business");
        assert!(starts.lock().unwrap().is_empty());

        engine.start("ubuntu").await.unwrap(); // Start is pressed again
        assert!(engine.wake("ubuntu").await);
    }

    #[tokio::test]
    async fn a_probe_during_a_stop_does_not_undo_it() {
        // The stop script takes a while, and Docker still answers meanwhile.
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let mut engine = engine_recording_starts("ubuntu", Ok(())).0;
        engine.backend.stop = Box::new(move |_| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(())
            })
        });
        let engine = Arc::new(engine);
        engine.seen_running(&engine.target().unwrap());

        let stop = tokio::spawn({
            let engine = engine.clone();
            async move { engine.stop("ubuntu", false).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let probe = engine.target().unwrap(); // a status poll while the stop runs
        engine.seen_running(&probe);
        assert!(!engine.wake("ubuntu").await, "the pipe stays shut during the stop");
        assert!(!engine.expected.load(Ordering::Relaxed));

        let late = engine.target().unwrap(); // sent before the stop finished...
        release.notify_one();
        stop.await.unwrap().unwrap();
        engine.seen_running(&late); // ...answered after it
        assert!(!engine.wake("ubuntu").await, "and after it");
        assert!(!engine.up.load(Ordering::Relaxed));

        // A probe made once the stop is over is believed again.
        engine.seen_running(&engine.target().unwrap());
        assert!(engine.wake("ubuntu").await);
    }

    #[tokio::test]
    async fn shutting_down_happens_even_if_the_stop_script_fails() {
        let terminated = Arc::new(Mutex::new(Vec::new()));
        let log = terminated.clone();
        let mut engine = engine_recording_starts("ubuntu", Ok(())).0;
        engine.backend.stop = Box::new(|_| Box::pin(std::future::pending()));
        engine.backend.terminate = Box::new(move |d| {
            log.lock().unwrap().push(d);
            Box::pin(async { Ok(()) })
        });
        engine.stop_timeout = Duration::from_millis(50);

        assert!(engine.stop("ubuntu", true).await.is_ok(), "the shutdown is what counts");
        assert_eq!(*terminated.lock().unwrap(), vec!["ubuntu".to_string()]);
    }

    #[tokio::test]
    async fn a_connection_to_a_distro_being_shut_down_stays_refused_through_a_switch() {
        // Shutting `a` down takes a while (terminate hangs until released).
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let mut engine = engine_recording_starts("a", Ok(())).0;
        engine.backend.terminate = Box::new(move |_| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(())
            })
        });
        let engine = Arc::new(engine);
        let shutdown = tokio::spawn({
            let engine = engine.clone();
            async move { engine.stop("a", true).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!engine.wake("a").await, "refused while a is selected");

        engine.select("b".into());
        assert!(!engine.wake("a").await, "and while b is: the connection is still bound to a");
        assert!(engine.allows("a").is_err());
        assert!(engine.wake("b").await, "b itself is untouched");

        engine.select("a".into());
        assert!(!engine.wake("a").await, "and after switching back");

        release.notify_one();
        shutdown.await.unwrap().unwrap();
        assert!(!engine.wake("a").await, "shut down on purpose: still refused until it is started");
        engine.start("a").await.unwrap();
        assert!(engine.wake("a").await);
        assert!(engine.allows("a").is_ok());
    }

    #[tokio::test]
    async fn a_distro_picked_automatically_does_not_inherit_the_stop() {
        let (engine, _) = engine_recording_starts("a", Ok(()));
        engine.stop("a", false).await.unwrap();
        assert!(!engine.wake("a").await);

        // "a" was removed; the next status picks "b" on its own.
        assert_eq!(engine.ensure_distro(&["b".to_string()]), Some("b".to_string()));
        assert!(engine.wake("b").await, "b was never stopped");
    }

    #[tokio::test]
    async fn a_stop_script_that_hangs_is_given_up_on() {
        let mut engine = engine_recording_starts("ubuntu", Ok(())).0;
        engine.backend.stop = Box::new(|_| Box::pin(std::future::pending()));
        engine.stop_timeout = Duration::from_millis(100);

        let began = std::time::Instant::now();
        let stopped = engine.stop("ubuntu", false).await;
        assert!(stopped.unwrap_err().contains("took too long"));
        assert!(began.elapsed() < Duration::from_secs(2), "took {:?}", began.elapsed());
        // The lock is free, so shutting the distro down (or a start) can go ahead.
        assert!(engine.starting.try_lock().is_ok());
        assert!(!engine.wake("ubuntu").await, "and it still counts as stopped on purpose");
    }

    #[test]
    fn a_connection_to_another_distro_does_not_keep_the_selected_one_awake() {
        let engine = idle_engine(); // "test" is selected
        engine.connected("previous"); // an IDE still listening to the old distro
        assert!(engine.try_sleep(Duration::ZERO));
    }

    /// A backend whose start script returns `outcome` at once and records where it ran.
    fn engine_recording_starts(distro: &str, outcome: Result<(), String>) -> (Engine, Arc<Mutex<Vec<String>>>) {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let log = starts.clone();
        let backend = Backend {
            start: Box::new(move |d| {
                log.lock().unwrap().push(d);
                let outcome = outcome.clone();
                Box::pin(async move { outcome })
            }),
            stop: Box::new(|_| Box::pin(async { Ok(()) })),
            terminate: Box::new(|_| Box::pin(async { Ok(()) })),
            ping: Box::new(|_| Box::pin(async { true })),
            keepalive: Box::new(|_| None),
            count: Box::new(|_| Box::pin(async { Some(0) })),
            daemon_alive: Box::new(|_| Box::pin(async { Some(false) })),
        };
        (Engine::with_backend(Some(distro.into()), backend), starts)
    }

    #[tokio::test]
    async fn a_second_wake_waits_for_the_start_in_progress() {
        // State while the first connection is waking the engine: `sleeping`
        // already cleared, `waking` set, the start lock held.
        let engine = Arc::new(engine_recording_starts("ubuntu", Ok(())).0);
        engine.waking.store(true, Ordering::Relaxed);
        let start_in_progress = engine.starting.lock().await;

        let second = tokio::spawn({
            let engine = engine.clone();
            async move { engine.wake("ubuntu").await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished(), "must not return while the engine is still starting");

        engine.waking.store(false, Ordering::Relaxed);
        drop(start_in_progress);
        second.await.unwrap();
    }
}

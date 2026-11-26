mod bridge;
mod context;
mod docker;
mod engine;
mod startup;
mod wsl;

use engine::Engine;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use hyper::Method;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};

struct AppState {
    /// Engine state shared with the bridge, the event watcher and the idle monitor.
    engine: Arc<Engine>,
    version: Mutex<Option<String>>,
    distros: Mutex<Option<(Instant, Vec<String>)>>,
    streams: Mutex<HashMap<u32, LogStream>>,
    next_stream: AtomicU32,
    /// Goes up each time the main window is destroyed.
    window_epoch: AtomicU32,
    /// See `StatsLink`.
    stats: tokio::sync::Mutex<Option<StatsLink>>,
    config_path: PathBuf,
    /// The app's name, which is also the name of its start-up entry.
    product: String,
}

impl AppState {
    /// Writes the settings to disk. A failure is returned, not swallowed: a
    /// setting that was not saved must not look saved.
    fn save(&self) -> Result<(), String> {
        let distro = self.engine.distro.lock().unwrap().clone();
        write_config(&self.config_path, distro.as_deref(), self.engine.keeps_running())
    }
}

fn write_config(path: &std::path::Path, distro: Option<&str>, keep_running: bool) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("Could not save settings to {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    let config = json!({ "distro": distro, "keepRunning": keep_running });
    std::fs::write(path, config.to_string()).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_log_batch_is_out_at_a_time_and_the_backlog_keeps_the_newest() {
        let mut outbox = LogOutbox::default();
        assert_eq!(outbox.take(), None, "nothing to send yet");

        outbox.push("first\n");
        assert_eq!(outbox.take(), Some(LogBatch { seq: 1, text: "first\n".into() }));

        // The UI has not confirmed: more text arrives, nothing more is sent,
        // however long that lasts. There is no timer that lets it through.
        outbox.push("second\n");
        assert_eq!(outbox.take(), None);

        // A flood while waiting is capped, and it is the old text that goes.
        outbox.push(&"x".repeat(LOG_BATCH_MAX));
        outbox.push("newest\n");
        assert_eq!(outbox.take(), None);
        let kept = outbox.bytes;
        assert!(kept <= LOG_BATCH_MAX && kept > LOG_BATCH_MAX - LOG_PIECE, "kept {kept}");
        assert_eq!(kept, outbox.held.iter().map(String::len).sum::<usize>());

        outbox.confirmed(1);
        let batch = outbox.take().unwrap();
        assert_eq!(batch.seq, 2);
        assert_eq!(batch.text.len(), kept);
        assert!(batch.text.ends_with("newest\n"));
        assert!(!batch.text.contains("second"));
        assert_eq!(outbox.bytes, 0);

        // When the stream ends, what is held goes out even if unconfirmed.
        outbox.push("last words\n");
        assert_eq!(outbox.drain(), Some(LogBatch { seq: 3, text: "last words\n".into() }));
        assert_eq!(outbox.drain(), None);
    }

    #[test]
    fn a_full_log_buffer_takes_small_writes_cheaply_and_keeps_text_intact() {
        // The reviewer's case: the buffer is full and 8 KiB writes keep coming.
        let mut outbox = LogOutbox::default();
        outbox.push(&"x".repeat(LOG_BATCH_MAX));
        let line = format!("{}\n", "é".repeat(4095)); // 8191 bytes of 2-byte characters + newline
        let began = std::time::Instant::now();
        for _ in 0..16_384 {
            outbox.push(&line); // 128 MiB in all
        }
        let took = began.elapsed();
        assert!(took < Duration::from_secs(5), "took {took:?}");
        assert!(outbox.bytes <= LOG_BATCH_MAX && outbox.bytes > LOG_BATCH_MAX - LOG_PIECE);
        assert!(outbox.held.iter().all(|piece| piece.len() <= LOG_PIECE));

        // Whole pieces were dropped, so no character was cut in half: what
        // comes out is the tail of what went in.
        let text = outbox.take().unwrap().text;
        assert!(text.ends_with(&line));
        assert!(text.chars().all(|c| c == 'é' || c == '\n'));
    }

    #[test]
    fn tiny_log_writes_fill_pieces_instead_of_making_one_each() {
        // The reviewer's case: two bytes at a time until the buffer is full.
        let mut outbox = LogOutbox::default();
        for _ in 0..(LOG_BATCH_MAX / 2) {
            outbox.push("a\n");
        }
        assert_eq!(outbox.bytes, LOG_BATCH_MAX);
        assert_eq!(outbox.held.len(), LOG_BATCH_MAX / LOG_PIECE, "32 full pieces, not 262,144 strings");

        // And on: the count stays where it is while old pieces are dropped.
        for _ in 0..100_000 {
            outbox.push("b\n");
        }
        assert!(outbox.held.len() <= LOG_BATCH_MAX / LOG_PIECE + 1);
        assert!(outbox.bytes <= LOG_BATCH_MAX);

        // A character that does not fit in the rest of a piece starts the
        // next one; nothing is split in the middle.
        let mut outbox = LogOutbox::default();
        outbox.push(&"x".repeat(LOG_PIECE - 1));
        outbox.push("é!");
        assert_eq!(outbox.held.len(), 2);
        assert_eq!(outbox.held[1], "é!");
        assert_eq!(outbox.take().unwrap().text, format!("{}é!", "x".repeat(LOG_PIECE - 1)));
    }

    #[test]
    fn a_late_confirmation_of_an_older_batch_lets_nothing_through() {
        let mut outbox = LogOutbox::default();
        outbox.push("one\n");
        assert_eq!(outbox.take().unwrap().seq, 1);
        outbox.confirmed(1);
        outbox.push("two\n");
        assert_eq!(outbox.take().unwrap().seq, 2);

        outbox.push("three\n");
        outbox.confirmed(1); // batch 1 confirmed again, late: batch 2 is still out
        assert_eq!(outbox.take(), None);
        outbox.confirmed(7); // a number that was never sent
        assert_eq!(outbox.take(), None);

        outbox.confirmed(2);
        assert_eq!(outbox.take().unwrap().seq, 3);
    }

    #[test]
    fn an_event_stream_that_keeps_dying_at_once_backs_off_and_is_given_up() {
        let mut backoff = Backoff::new();
        let mut waits = vec![backoff.retry.as_secs()];
        let mut gave_up = false;
        // Docker answers /events with 200 and closes the stream immediately, every time.
        for _ in 0..Backoff::GIVE_UP_AFTER {
            assert!(!gave_up);
            if !backoff.stream_ended(false, Duration::from_millis(50)) {
                gave_up = backoff.failed();
            }
            waits.push(backoff.retry.as_secs());
        }
        assert!(gave_up, "not reconnecting every 2 s for ever");
        assert_eq!(waits, vec![2, 4, 8, 16, 32, 2], "each retry waits longer, then it starts over");

        // A stream that worked for a while, or that Pier closed itself, is not a failure.
        let mut backoff = Backoff::new();
        backoff.failed();
        assert!(backoff.stream_ended(false, Duration::from_secs(31)));
        assert_eq!((backoff.retry.as_secs(), backoff.failures), (2, 0));
        backoff.failed();
        assert!(backoff.stream_ended(true, Duration::from_millis(50)));
        assert_eq!(backoff.failures, 0);
    }

    #[test]
    fn settings_are_written_and_a_failure_is_reported() {
        let dir = std::env::temp_dir().join(format!("pier-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let file = dir.join("config.json");
        write_config(&file, Some("Ubuntu"), true).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved, json!({ "distro": "Ubuntu", "keepRunning": true }));

        // A config "directory" that is really a file cannot be written into.
        let blocked = file.join("config.json");
        let error = write_config(&blocked, Some("Ubuntu"), false).unwrap_err();
        assert!(error.starts_with("Could not save settings to "), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    distros: Vec<String>,
    distro: Option<String>,
    distro_running: bool,
    engine_running: bool,
    /// Pier let go of an idle engine; it is started again on demand.
    sleeping: bool,
    /// The engine is never let go of while idle (Settings).
    keep_running: bool,
    /// The app is set to start (in the tray) when the user signs in.
    start_on_login: bool,
    version: Option<String>,
    pipe: &'static str,
}

/// How long the list of installed distros is reused before asking wsl.exe again.
const DISTROS_TTL: Duration = Duration::from_secs(60);

/// Cheap enough to poll: never boots a stopped distro, and spawns nothing
/// while the event stream is connected.
#[tauri::command]
async fn status(state: State<'_, AppState>) -> Result<Status, String> {
    probe(&state).await
}

/// What `status` does, callable without a window: finds out whether the
/// engine is up and records it, which is what makes the keep-alive, the event
/// stream and the idle rule take hold.
async fn probe(state: &AppState) -> Result<Status, String> {
    let cached = state
        .distros
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(at, _)| at.elapsed() < DISTROS_TTL)
        .map(|(_, list)| list.clone());
    let distros = match cached {
        Some(list) => list,
        None => match wsl::distros().await {
            Ok(list) => {
                *state.distros.lock().unwrap() = Some((Instant::now(), list.clone()));
                list
            }
            // wsl.exe failing to answer is not "no distros": acting on an
            // empty list would deselect the distro and drop the engine. Go on
            // with the last list that was read, however old.
            Err(e) => match state.distros.lock().unwrap().as_ref() {
                Some((_, last)) => last.clone(),
                None => return Err(e),
            },
        },
    };

    let engine = &state.engine;
    engine.ensure_distro(&distros);
    // What is found below is about this target; if the engine is stopped or
    // another distro selected before the probe returns, it is not recorded.
    let target = engine.target().ok();
    let distro = target.as_ref().map(|t| t.distro.clone());

    let mut distro_running = false;
    let mut engine_running = false;
    let mut version = None;
    // False when WSL would not say whether the distro is running.
    let mut known = true;
    if let Some(d) = &distro {
        if engine.events_live.load(Ordering::Relaxed) {
            distro_running = true;
            engine_running = true;
            version = state.version.lock().unwrap().clone();
        } else {
            match wsl::running().await {
                Ok(running) => distro_running = running.contains(d),
                // WSL could not say. That is not "stopped": show the engine as
                // not running for now, but record nothing and ask again later.
                Err(_) => known = false,
            }
            // No probe while the engine is being stopped, or if it changed
            // hands while WSL was answering: by the time the probe ran the
            // distro may have been shut down, and its relay would boot it.
            let probe = target.as_ref().filter(|t| distro_running && engine.can_probe(t));
            if let Some(target) = probe {
                // One request answers both "is it up" and "which version".
                match docker::json(d, Method::GET, "/version", None).await {
                    Ok(v) => {
                        engine_running = true;
                        version = v.get("Version").and_then(Value::as_str).map(str::to_string);
                        *state.version.lock().unwrap() = version.clone();
                    }
                    // The distro is up and Docker did not answer this once.
                    // That is "unknown", not "stopped": keep showing what was
                    // known, keep the keep-alive, and let the next polls
                    // decide. Only repeated failures count as down.
                    Err(_) if !engine.probe_failed(target) => {
                        known = false;
                        engine_running = engine.up.load(Ordering::Relaxed);
                        if engine_running {
                            version = state.version.lock().unwrap().clone();
                        }
                    }
                    // It has not answered several times running. Find out
                    // whether dockerd is gone (then it is down, and let go
                    // of) or merely not answering (then it is shown as not
                    // running, but nothing is let go of). Either way the
                    // engine recorded what it found; nothing more to do here.
                    Err(_) => {
                        engine.confirm_down(target).await;
                        known = false;
                    }
                }
            } else if distro_running {
                known = false; // not probed, so nothing new is known
            }
        }
    }
    match &target {
        // `known` is false when the engine only looks up from memory.
        Some(target) if engine_running && known => engine.seen_running(target),
        // The distro itself is not running: the engine is certainly down.
        Some(target) if !engine_running && known => engine.seen_stopped(target),
        _ => {}
    }
    let sleeping = !engine_running && engine.is_asleep();

    Ok(Status {
        distros,
        distro,
        distro_running,
        engine_running,
        sleeping,
        keep_running: engine.keeps_running(),
        start_on_login: startup::enabled(&state.product),
        version,
        pipe: bridge::PIPE_NAME,
    })
}

#[tauri::command]
async fn set_distro(state: State<'_, AppState>, name: String) -> Result<(), String> {
    state.engine.select(name);
    *state.version.lock().unwrap() = None;
    // The switch itself stands; the error says it will not survive a restart.
    state.save()
}

/// Chooses whether an idle engine is kept running (true) or let go of after a
/// while so WSL can shut the distro down (false, the default).
///
/// If the choice cannot be saved it is not applied either, and the error is
/// returned: otherwise the switch would work until the next restart and then
/// silently go back.
#[tauri::command]
fn set_keep_running(state: State<'_, AppState>, keep: bool) -> Result<(), String> {
    let before = state.engine.keeps_running();
    state.engine.set_keep_running(keep);
    state.save().inspect_err(|_| state.engine.set_keep_running(before))
}

/// `distro` is the one the window was showing when Start was pressed; the
/// start is for that distro or it does not happen.
/// Turns starting at sign-in on or off. On, the app starts in the tray with
/// no window, so the pipe is there for docker commands from the first moment.
#[tauri::command]
fn set_start_on_login(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    startup::set(&state.product, enabled)
}

#[tauri::command]
async fn engine_start(state: State<'_, AppState>, distro: String) -> Result<(), String> {
    state.engine.require(&distro)?;
    state.engine.start(&distro).await
}

/// Stops dockerd. With `shutdown_distro` the whole distro is terminated too,
/// which hands its memory back to Windows.
#[tauri::command]
///
/// `distro` is the one the window was showing when the button was pressed.
/// It must still be the selected one, and the stop script and the shutdown
/// both act on it, whatever is selected by the time they run.
async fn engine_stop(state: State<'_, AppState>, distro: String, shutdown_distro: bool) -> Result<(), String> {
    state.engine.require(&distro)?;
    // A deliberate stop: nothing wakes the engine again until it is started.
    // With `shutdown_distro` the distro is shut down as part of the same stop,
    // whatever the stop script said or however long it took.
    state.engine.stop(&distro, shutdown_distro).await
}

/// Pass-through to the Docker Engine API. Keeping this generic means new
/// features are usually a frontend-only change.
///
/// `distro` is the one the window believes it is talking to. The request is
/// refused if that is no longer the selected distro (the list on screen is
/// then out of date), and otherwise goes to exactly that distro.
#[tauri::command]
async fn docker_api(
    state: State<'_, AppState>,
    distro: String,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    if !path.starts_with('/') {
        return Err("Path must start with /".into());
    }
    let method = Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    state.engine.require(&distro)?;
    // Not while it is being stopped, or after it was stopped on purpose: a
    // request would start a relay in the distro and boot it again. The same
    // rule the pipe follows, enforced here so no caller can get around it.
    state.engine.allows(&distro)?;
    docker::json(&distro, method, &path, body).await
}

/// CPU and memory figures of several containers at once, in the order asked
/// (`null` for one that could not be read). The window asks every few
/// seconds while the list is on screen, so all of them share one relay.
#[tauri::command]
async fn container_stats(state: State<'_, AppState>, distro: String, ids: Vec<String>) -> Result<Vec<Value>, String> {
    if !ids.iter().all(|id| valid_id(id)) {
        return Err("Invalid container id".into());
    }
    if ids.is_empty() {
        return Ok(vec![]);
    }
    state.engine.require(&distro)?;
    state.engine.allows(&distro)?;
    // One sample each, taken at once: Docker's default waits a second per
    // container to work out a CPU percentage, which the window does itself
    // from two samples.
    let paths: Vec<String> = ids.iter().map(|id| format!("/containers/{id}/stats?stream=false&one-shot=true")).collect();

    // The same connection is used again from one refresh to the next, so a
    // list that stays on screen starts a relay once, not every 3 seconds.
    let mut link = state.stats.lock().await;
    if let Some(open) = link.as_mut().filter(|open| open.distro == distro) {
        if let Ok(answers) = open.link.get_each(&paths).await {
            open.used = Instant::now();
            return Ok(answers);
        }
    }
    // None yet, one for another distro, or one that went stale: start afresh.
    *link = None;
    let mut fresh = docker::Link::open(&distro).await?;
    let answers = fresh.get_each(&paths).await?;
    *link = Some(StatsLink { distro, link: fresh, used: Instant::now() });
    Ok(answers)
}

/// The connection `container_stats` keeps between refreshes.
struct StatsLink {
    distro: String,
    link: docker::Link,
    used: Instant,
}

/// Closes the stats connection once nobody has asked for a while (the window
/// was minimised, or another page is showing): an open relay is a wsl.exe
/// and a session in the distro, not something to hold for nothing.
async fn close_idle_stats_link(app: AppHandle) {
    const IDLE: Duration = Duration::from_secs(10);
    loop {
        tokio::time::sleep(IDLE / 2).await;
        let state = app.state::<AppState>();
        let mut link = state.stats.lock().await;
        if link.as_ref().is_some_and(|open| open.used.elapsed() >= IDLE) {
            *link = None;
        }
    }
}

/// Most log text held back for the UI between two flushes (bytes).
const LOG_BATCH_MAX: usize = 512 * 1024;

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Follows a container's logs, pushing text to `on_chunk` until
/// `stop_stream` is called with the returned id.
#[tauri::command]
async fn stream_logs(
    state: State<'_, AppState>,
    distro: String,
    id: String,
    tail: u32,
    on_chunk: Channel<LogBatch>,
) -> Result<u32, String> {
    if !valid_id(&id) {
        return Err("Invalid container id".into());
    }
    // The stream belongs to the window that is open right now.
    let window = state.window_epoch.load(Ordering::Relaxed);
    state.engine.require(&distro)?;
    state.engine.allows(&distro)?;
    let info = docker::json(&distro, Method::GET, &format!("/containers/{id}/json"), None).await?;
    let tty = info["Config"]["Tty"].as_bool().unwrap_or(false);

    // Asked again before the second relay: the engine may have been stopped,
    // or another distro selected, while the first request was answered.
    state.engine.require(&distro)?;
    state.engine.allows(&distro)?;
    let path = format!("/containers/{id}/logs?stdout=1&stderr=1&follow=1&tail={tail}");
    let reply = docker::send(&distro, Method::GET, &path, None).await?;
    if !reply.response.status().is_success() {
        return Err(format!("Docker returned HTTP {}", reply.response.status()));
    }

    // Registering and the close-time clean-up take the same lock, so a
    // window that closed while Docker was answering is noticed here: the
    // reply is dropped (killing its relay) instead of becoming a stream that
    // nothing would ever stop.
    let mut streams = state.streams.lock().unwrap();
    if state.window_epoch.load(Ordering::Relaxed) != window {
        return Err("The window was closed".into());
    }
    let consumed = Arc::new(LogAck::default());
    let ack = consumed.clone();
    let task = tauri::async_runtime::spawn(async move {
        let docker::Reply { response, relay: _relay } = reply;
        let mut body = response.into_body();
        let mut decoder = docker::LogDecoder::new(tty);
        // Output goes to the UI in batches, at most one every 50 ms, and at
        // most one at a time: each batch has a number, and the next one is
        // sent only when the UI has confirmed that number (`ack_logs`).
        // Docker is read all the while, and what piles up meanwhile is
        // capped, oldest text first out. If the UI never confirms, sending
        // simply stays paused: there is no timer that gives up and sends
        // anyway. So at most one batch is ever in the channel to the
        // WebView, however fast the container logs and however long the
        // window does not draw.
        let mut outbox = LogOutbox::default();
        let mut flush = tokio::time::interval(Duration::from_millis(50));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                frame = body.frame() => {
                    let Some(Ok(frame)) = frame else { break };
                    let Some(data) = frame.data_ref() else { continue };
                    outbox.push(&decoder.push(data));
                }
                _ = flush.tick() => {
                    if let Some(batch) = outbox.take() {
                        if on_chunk.send(batch).is_err() {
                            return; // the window is gone
                        }
                    }
                }
                _ = consumed.changed.notified() => outbox.confirmed(consumed.batch.load(Ordering::Relaxed)),
            }
        }
        // The container stopped: what is left is its last words, send them.
        if let Some(batch) = outbox.drain() {
            let _ = on_chunk.send(batch);
        }
    });

    let stream_id = state.next_stream.fetch_add(1, Ordering::Relaxed);
    streams.insert(stream_id, LogStream { task, ack });
    Ok(stream_id)
}

/// What a pull tells the window: how far it is, and then how it ended.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum PullEvent {
    Progress(docker::PullProgress),
    Done { message: String },
    Failed { message: String },
}

/// Pulls `image` from its registry (public images only: no credentials are
/// sent). Progress and the outcome go to `on_event`; the returned id stops
/// the pull when given to `stop_stream`, which makes Docker abandon it.
///
/// It is a stream like a container's logs and is kept with them, so closing
/// the window ends it the same way.
#[tauri::command]
async fn pull_image(
    state: State<'_, AppState>,
    distro: String,
    image: String,
    on_event: Channel<PullEvent>,
) -> Result<u32, String> {
    let reference = docker::pull_reference(&image)?;
    let window = state.window_epoch.load(Ordering::Relaxed);
    state.engine.require(&distro)?;
    state.engine.allows(&distro)?;
    let reply = docker::send(&distro, Method::POST, &format!("/images/create?fromImage={reference}"), None).await?;
    let status = reply.response.status();
    if !status.is_success() {
        // An image that does not exist is refused here, before any progress.
        let body = reply.response.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        let said: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        return Err(said["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("Docker returned HTTP {status}")));
    }

    let mut streams = state.streams.lock().unwrap();
    if state.window_epoch.load(Ordering::Relaxed) != window {
        return Err("The window was closed".into());
    }
    let task = tauri::async_runtime::spawn(async move {
        let docker::Reply { response, relay: _relay } = reply;
        let mut body = response.into_body();
        let mut tracker = docker::PullTracker::default();
        // Docker reports every few kilobytes of every layer; the window gets
        // the sum a few times a second.
        let mut tick = tokio::time::interval(Duration::from_millis(150));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut changed = false;
        loop {
            tokio::select! {
                frame = body.frame() => {
                    let Some(Ok(frame)) = frame else { break };
                    if let Some(data) = frame.data_ref() {
                        tracker.push(data);
                        changed = true;
                    }
                }
                _ = tick.tick(), if changed => {
                    changed = false;
                    if on_event.send(PullEvent::Progress(tracker.progress())).is_err() {
                        return; // the window is gone
                    }
                }
            }
        }
        let _ = on_event.send(match tracker.finish() {
            Ok(message) => PullEvent::Done { message },
            Err(message) => PullEvent::Failed { message },
        });
    });
    let stream_id = state.next_stream.fetch_add(1, Ordering::Relaxed);
    streams.insert(stream_id, LogStream { task, ack: Arc::new(LogAck::default()) });
    Ok(stream_id)
}

/// Whether docker commands on Windows go to Pier, and what they go to if not.
#[tauri::command]
async fn docker_context() -> context::State {
    context::state().await
}

/// Points docker commands on Windows at Pier's pipe (`pier` true), or back at
/// what they used before.
#[tauri::command]
async fn set_docker_context(pier: bool) -> Result<context::State, String> {
    if pier {
        context::use_pier(&format!("npipe://{}", bridge::PIPE_NAME.replace('\\', "/"))).await?;
    } else {
        context::leave().await?;
    }
    Ok(context::state().await)
}

struct LogStream {
    task: tauri::async_runtime::JoinHandle<()>,
    ack: Arc<LogAck>,
}

/// The number of the last batch the UI said it drew.
#[derive(Default)]
struct LogAck {
    batch: std::sync::atomic::AtomicU64,
    changed: tokio::sync::Notify,
}

/// One piece of log text on its way to the UI. `seq` is what the UI sends
/// back to confirm it.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct LogBatch {
    seq: u64,
    text: String,
}

/// Log text waiting to go to the UI. One batch is out at a time; text that
/// arrives while it is unconfirmed is held here, newest `LOG_BATCH_MAX` bytes.
///
/// The text is kept as a queue of pieces of at most `LOG_PIECE` bytes. When
/// the queue is over the limit whole pieces are dropped from its front, which
/// costs nothing. (Kept as one string, every small write to a full buffer
/// had to shift the remaining half megabyte down.) The limit is therefore
/// held to within one piece: between `LOG_BATCH_MAX - LOG_PIECE` and
/// `LOG_BATCH_MAX` bytes are kept.
#[derive(Default)]
struct LogOutbox {
    held: std::collections::VecDeque<String>,
    /// Bytes in `held`.
    bytes: usize,
    /// Number of the batch that is out and not yet confirmed, if any.
    unconfirmed: Option<u64>,
    sent: u64,
}

/// Largest piece of log text kept as one unit.
const LOG_PIECE: usize = 16 * 1024;

impl LogOutbox {
    fn push(&mut self, mut text: &str) {
        // Text is appended to the newest piece until that piece is full, and
        // only then is a new one begun. So the number of pieces depends on
        // how much text is held, never on how it arrived: a container that
        // writes two bytes at a time does not cost one allocation per write.
        // And no piece is ever large, so dropping the oldest never drops much.
        while !text.is_empty() {
            let room = match self.held.back() {
                Some(newest) if newest.len() < LOG_PIECE => LOG_PIECE - newest.len(),
                _ => {
                    self.held.push_back(String::with_capacity(LOG_PIECE));
                    LOG_PIECE
                }
            };
            let mut end = text.len().min(room);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                // The next character does not fit in what is left of this
                // piece (it is several bytes long): it starts the next one.
                self.held.push_back(String::with_capacity(LOG_PIECE));
                continue;
            }
            let (fits, rest) = text.split_at(end);
            self.held.back_mut().expect("a piece was just ensured").push_str(fits);
            self.bytes += fits.len();
            text = rest;
        }
        while self.bytes > LOG_BATCH_MAX {
            match self.held.pop_front() {
                Some(oldest) => self.bytes -= oldest.len(),
                None => break,
            }
        }
    }

    /// Everything held, as one string, leaving the outbox empty.
    fn collect(&mut self) -> String {
        let mut text = String::with_capacity(self.bytes);
        text.extend(self.held.drain(..));
        self.bytes = 0;
        text
    }

    /// The next batch, if there is text and the last batch was confirmed.
    fn take(&mut self) -> Option<LogBatch> {
        if self.unconfirmed.is_some() || self.held.is_empty() {
            return None;
        }
        self.sent += 1;
        self.unconfirmed = Some(self.sent);
        Some(LogBatch { seq: self.sent, text: self.collect() })
    }

    /// The UI confirmed batch `seq`. Only the batch that is actually out
    /// counts: a late or repeated confirmation of an older one changes
    /// nothing.
    fn confirmed(&mut self, seq: u64) {
        if self.unconfirmed == Some(seq) {
            self.unconfirmed = None;
        }
    }

    /// Whatever is held, confirmed or not (the stream is over).
    fn drain(&mut self) -> Option<LogBatch> {
        if self.held.is_empty() {
            return None;
        }
        self.sent += 1;
        Some(LogBatch { seq: self.sent, text: self.collect() })
    }
}

/// The UI has drawn the batch it was sent; the next one may go.
#[tauri::command]
fn ack_logs(state: State<'_, AppState>, stream_id: u32, seq: u64) {
    if let Some(stream) = state.streams.lock().unwrap().get(&stream_id) {
        stream.ack.batch.store(seq, Ordering::Relaxed);
        stream.ack.changed.notify_one();
    }
}

#[tauri::command]
fn stop_stream(state: State<'_, AppState>, stream_id: u32) {
    if let Some(stream) = state.streams.lock().unwrap().remove(&stream_id) {
        stream.task.abort();
    }
}

#[tauri::command]
fn open_terminal(state: State<'_, AppState>, distro: String, id: String) -> Result<(), String> {
    state.engine.require(&distro)?;
    state.engine.allows(&distro)?;
    wsl::open_terminal(&distro, &id)
}

/// Forwards Docker's event stream to the UI so lists refresh instantly
/// instead of on a timer. Only connects while the engine is known to be up,
/// so it never boots a stopped distro.
///
/// While the stream is open `events_live` is set, which lets `status` answer
/// without spawning anything, and lets the idle monitor work.
///
/// The watcher runs on its own, with no window needed: as long as the engine
/// is `expected` to be running it reconnects a dropped stream, backing off
/// from 2 s to 60 s. Before each reconnect it checks that the distro is still
/// running (which boots nothing); if the distro is gone, so is the engine,
/// and Pier lets go of it instead of holding a keep-alive for nothing.
async fn watch_events(app: AppHandle, engine: Arc<Engine>) {
    let mut backoff = Backoff::new();
    // The target the failures in `backoff` were counted against.
    let mut watched: Option<engine::Target> = None;
    loop {
        tokio::time::sleep(backoff.retry).await;
        if !engine.expected.load(Ordering::Relaxed) {
            backoff.reset();
            continue;
        }
        // Everything below is for this target. Whatever it finds is only
        // recorded if the engine has not been stopped, or another distro
        // selected, in the meantime.
        let Ok(target) = engine.target() else { continue };
        // Failures are counted per engine: what went wrong with another
        // distro, or with this one before it was stopped or put to sleep,
        // says nothing about the one being tried now.
        if watched.as_ref() != Some(&target) {
            backoff.reset();
            watched = Some(target.clone());
        }
        let d = target.distro.clone();
        if !engine.up.load(Ordering::Relaxed) {
            // Not confirmed since the last drop: never connect to (and so
            // boot) a distro that has stopped.
            match wsl::running().await {
                Ok(running) if running.contains(&d) => {}
                Ok(_) => {
                    eprintln!("events: {d} is no longer running, letting go of the engine");
                    engine.give_up(&target);
                    continue;
                }
                Err(_) => {
                    backoff.slower(); // WSL could not say; ask again later
                    continue;
                }
            }
        }
        // Asked again right before the relay is started: a stop or a switch
        // may have happened while WSL was answering above.
        if !engine.wants_events(&target) || !engine.still(&target) {
            continue;
        }
        // One failure of the stream: back off, and after too many in a row
        // let go of the engine. The distro is up but Docker is not (stopped
        // from inside the distro, say); without this Pier's own keep-alive
        // would hold the distro awake for an engine that is gone.
        // Returns whether that was one too many.
        let failed = |backoff: &mut Backoff| {
            engine.events_lost(&target);
            backoff.failed()
        };
        let reply = match docker::send(&d, Method::GET, "/events", None).await {
            Ok(r) if r.response.status().is_success() => r,
            _ => {
                if failed(&mut backoff) {
                    // Too many in a row. That alone does not mean the engine is
                    // gone: let go only if dockerd really is. If it is still
                    // there (hung, overloaded), keep the distro awake for its
                    // containers and keep trying, slowly.
                    if engine.confirm_down(&target).await {
                        eprintln!("events: Docker in {d} is gone, letting go of the engine");
                    } else {
                        backoff.retry = Backoff::LAST_RETRY;
                    }
                }
                continue;
            }
        };
        if !engine.events_connected(&target) {
            continue; // out of date already: drop the stream, record nothing
        }
        // The failure count is not cleared here. Response headers alone do
        // not make a healthy stream: one that ends again at once is a
        // failure like any other, or Pier would reconnect every 2 s for
        // ever. It is cleared below, once the stream has lasted.
        let connected = Instant::now();
        let mut closed_on_purpose = false;
        let docker::Reply { response, relay: _relay } = reply;
        let mut body = response.into_body();
        let mut pending = Vec::new();
        let mut check = tokio::time::interval(Duration::from_secs(2));
        loop {
            let frame = tokio::select! {
                frame = body.frame() => frame,
                _ = check.tick() => {
                    // Engine stopped or put to sleep, or another distro was selected.
                    if !engine.wants_events(&target) {
                        closed_on_purpose = true;
                        break;
                    }
                    continue;
                }
            };
            let Some(Ok(frame)) = frame else { break };
            let Some(data) = frame.data_ref() else { continue };
            pending.extend_from_slice(data);
            while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=nl).collect();
                let Ok(event) = serde_json::from_slice::<Value>(&line) else { continue };
                // Health checks fire exec_* events every few seconds.
                let action = event["Action"].as_str().unwrap_or("");
                if event["Type"] == "container" && !action.starts_with("exec_") {
                    // The idle monitor counts running containers again.
                    engine.count_stale.store(true, Ordering::Relaxed);
                }
                if !action.starts_with("exec_") {
                    let _ = app.emit("docker-event", json!({ "type": event["Type"], "action": action, "id": event["Actor"]["ID"] }));
                }
            }
        }
        if backoff.stream_ended(closed_on_purpose, connected.elapsed()) {
            engine.events_lost(&target);
        } else {
            if failed(&mut backoff) {
                // Too many in a row. That alone does not mean the engine is
                // gone: let go only if dockerd really is. If it is still
                // there (hung, overloaded), keep the distro awake for its
                // containers and keep trying, slowly.
                if engine.confirm_down(&target).await {
                    eprintln!("events: Docker in {d} is gone, letting go of the engine");
                } else {
                    backoff.retry = Backoff::LAST_RETRY;
                }
            }
        }
    }
}

/// How the event watcher paces its reconnects.
struct Backoff {
    /// Wait before the next attempt.
    retry: Duration,
    /// Failures in a row.
    failures: u32,
}

impl Backoff {
    const FIRST_RETRY: Duration = Duration::from_secs(2);
    const LAST_RETRY: Duration = Duration::from_secs(60);
    /// Failures in a row (retries 4, 8, 16, 32 s apart: about a minute)
    /// before the engine counts as gone.
    const GIVE_UP_AFTER: u32 = 5;
    /// A stream that lasted this long was a working one.
    const HEALTHY_AFTER: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self { retry: Self::FIRST_RETRY, failures: 0 }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    /// Waits longer next time, without counting a failure.
    fn slower(&mut self) {
        self.retry = (self.retry * 2).min(Self::LAST_RETRY);
    }

    /// Counts a failure. True when it was one too many: give up, start over.
    fn failed(&mut self) -> bool {
        self.failures += 1;
        if self.failures >= Self::GIVE_UP_AFTER {
            self.reset();
            return true;
        }
        self.slower();
        false
    }

    /// A stream that was open has ended. True (and a fresh start) when that
    /// was fine: it was closed on purpose, or it had worked for a while.
    /// False when it ended almost as soon as it began, which is a failure
    /// for the caller to count, however successful its response headers were.
    fn stream_ended(&mut self, on_purpose: bool, lived: Duration) -> bool {
        let fine = on_purpose || lived >= Self::HEALTHY_AFTER;
        if fine {
            self.reset();
        }
        fine
    }
}

/// Shows the main window. Closing it destroys it (and its WebView2
/// processes, the bulk of Pier's memory), so it is created again here when
/// it is not there. Only the tray icon, the pipe bridge and the event watcher
/// live for the whole run.
fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        let _ = app.emit("window-visible", true);
        return;
    }
    // Opening the window is a reason to have the engine back.
    let engine = app.state::<AppState>().engine.clone();
    engine.window_opened();
    tauri::async_runtime::spawn(async move {
        if let Ok(distro) = engine.distro() {
            engine.wake(&distro).await;
        }
    });
    let Some(config) = app.config().app.windows.iter().find(|w| w.label == "main") else { return };
    match tauri::WebviewWindowBuilder::from_config(app, config).and_then(|b| b.build()) {
        Ok(window) => {
            let _ = window.set_focus();
        }
        Err(e) => eprintln!("could not open the main window: {e}"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Before anything is started: whatever Pier starts must end with it.
            wsl::end_children_with_pier();
            let config_path = app.path().app_config_dir()?.join("config.json");
            let config = std::fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or(Value::Null);

            let engine = Arc::new(Engine::new(config["distro"].as_str().map(str::to_string)));
            engine.set_keep_running(config["keepRunning"].as_bool().unwrap_or(false));
            app.manage(AppState {
                engine: engine.clone(),
                version: Mutex::new(None),
                distros: Mutex::new(None),
                streams: Mutex::new(HashMap::new()),
                next_stream: AtomicU32::new(1),
                window_epoch: AtomicU32::new(0),
                stats: tokio::sync::Mutex::new(None),
                config_path,
                product: app.config().product_name.clone().unwrap_or_else(|| app.package_info().name.clone()),
            });

            tauri::async_runtime::spawn(bridge::run(engine.clone()));
            tauri::async_runtime::spawn(watch_events(app.handle().clone(), engine.clone()));
            tauri::async_runtime::spawn(close_idle_stats_link(app.handle().clone()));
            tauri::async_runtime::spawn({
                let engine = engine.clone();
                async move { engine.watch_idle().await }
            });

            // The window is not created by the configuration but here, so that
            // a start at sign-in (`--hidden`) can do without one: just the tray
            // icon and the pipe, until the icon is clicked.
            if std::env::args().any(|arg| arg == startup::HIDDEN) {
                engine.window_closed();
                // No window will ask for the status, so ask once here: if the
                // engine is already up it is taken charge of (kept awake,
                // watched, put to sleep when idle). It boots nothing.
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let _ = probe(&handle.state::<AppState>()).await;
                });
            } else {
                show_main(app.handle());
            }

            let show = MenuItem::with_id(app, "show", "Open Pier", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Pier")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        app.state::<AppState>().engine.drop_all_keepalives();
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        // A destroyed window can no longer stop its own log streams, and a
        // quiet container would never make the send fail, so end them here.
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                let state = window.state::<AppState>();
                // From here on only the pipe or running containers keep the engine awake.
                state.engine.window_closed();
                // Bumped before the clean-up below, so a log request still
                // waiting for Docker sees it and does not register late.
                state.window_epoch.fetch_add(1, Ordering::Relaxed);
                // The stats connection belongs to the window too.
                if let Ok(mut link) = state.stats.try_lock() {
                    *link = None;
                }
                for (_, stream) in state.streams.lock().unwrap().drain() {
                    stream.task.abort();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            status,
            set_distro,
            set_keep_running,
            set_start_on_login,
            engine_start,
            engine_stop,
            docker_api,
            container_stats,
            stream_logs,
            ack_logs,
            stop_stream,
            open_terminal,
            pull_image,
            docker_context,
            set_docker_context
        ])
        .build(tauri::generate_context!())
        .expect("error while running Pier")
        // Closing the window lets it be destroyed, but Pier (tray icon and
        // bridge) keeps running: only Quit in the tray menu, which passes an
        // exit code, ends the process.
        .run(|_app, event| {
            if let RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

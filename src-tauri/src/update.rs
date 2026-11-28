//! Finding and installing a newer Pier.
//!
//! The newest release on GitHub carries a `latest.json` naming its version,
//! its installer and the installer's signature. Pier installs it only if the
//! signature was made with the key whose public half is in
//! `tauri.conf.json`; the plugin does the checking and cannot be told not to.
//!
//! Looking for a release is one HTTPS request to github.com. It is made when
//! the window opens, at most once a day, and when the user asks in Settings.
//! Nothing is ever installed without a click.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

/// How long the answer to "is there a newer release" is reused.
const REUSE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    /// The version that is running.
    current: String,
    /// A newer one that can be installed, if there is one.
    available: Option<String>,
    /// What its release says about it.
    notes: Option<String>,
}

/// The last answer and when it was given.
#[derive(Default)]
pub struct Cache(Mutex<Option<(Instant, Info)>>);

fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    app.updater_builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Could not look for updates: {e}"))
}

/// Whether a newer release exists. With `fresh`, GitHub is asked now;
/// otherwise an answer less than a day old is given again.
pub async fn check(app: &AppHandle, cache: &Cache, fresh: bool) -> Result<Info, String> {
    if !fresh {
        if let Some((at, info)) = cache.0.lock().unwrap().as_ref() {
            if at.elapsed() < REUSE {
                return Ok(info.clone());
            }
        }
    }
    let found = updater(app)?.check().await.map_err(|e| format!("Could not look for updates: {e}"))?;
    let info = Info {
        current: app.package_info().version.to_string(),
        available: found.as_ref().map(|u| u.version.clone()),
        notes: found.and_then(|u| u.body).filter(|notes| !notes.trim().is_empty()),
    };
    *cache.0.lock().unwrap() = Some((Instant::now(), info.clone()));
    Ok(info)
}

/// How long the download of an installer may take.
const DOWNLOAD: Duration = Duration::from_secs(120);

/// Downloads the newer release, checks its signature and runs its installer.
/// The installer needs Pier closed, so this ends the process; the installer
/// starts the new version when it is done.
///
/// The plugin's own `install` is not used. It starts the installer as an
/// ordinary child, and Pier's children are in a job that ends them when Pier
/// does (wsl.rs): in the first real run the installer died with Pier and the
/// update stopped half way. `download` is the plugin's and is where the
/// signature is checked.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    // One at a time, whatever the window does: its button comes back when
    // the Settings page is left and opened again, and a second run would
    // write the installer the first is about to start.
    let _only = Installing::begin().ok_or("An update is already being installed")?;
    download_and_start(app).await
}

/// Set while an update is being downloaded and its installer started.
static INSTALLING: AtomicBool = AtomicBool::new(false);

/// Held by the one install that is running; lets the next one in when it
/// ends, which it only does by failing.
struct Installing;

impl Installing {
    fn begin() -> Option<Self> {
        (!INSTALLING.swap(true, Ordering::SeqCst)).then_some(Self)
    }
}

impl Drop for Installing {
    fn drop(&mut self) {
        INSTALLING.store(false, Ordering::SeqCst);
    }
}

async fn download_and_start(app: &AppHandle) -> Result<(), String> {
    let update = app
        .updater_builder()
        .timeout(DOWNLOAD)
        .build()
        .map_err(|e| format!("Could not look for updates: {e}"))?
        .check()
        .await
        .map_err(|e| format!("Could not look for updates: {e}"))?
        .ok_or("There is no newer version to install")?;
    let failed = |e: String| format!("Could not install {}: {e}", update.version);
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| failed(e.to_string()))?;
    update.verify(&bytes).map_err(|e| failed(e.to_string()))?;

    let version: String = update.version.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')).collect();
    let dir = std::env::temp_dir().join(format!("Pier-{version}-updater-{}", std::process::id()));
    let installer = dir.join(format!("Pier-{version}-installer.exe"));
    std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&installer, &bytes)).map_err(|e| failed(e.to_string()))?;
    // /P: shows progress and asks nothing. /R: starts Pier again when done.
    // /UPDATE: this replaces an installed copy. The new Pier starts with its
    // window open, whatever this one was started with: the user is here.
    crate::wsl::start_outliving_pier(&installer, &["/P", "/R", "/UPDATE"]).map_err(|e| failed(e.to_string()))?;
    // Nothing is started after the installer, and ending now ends every
    // wsl.exe of Pier's with it, through the job.
    let _ = app;
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_install_runs_at_a_time() {
        let first = Installing::begin();
        assert!(first.is_some());
        assert!(Installing::begin().is_none(), "a second one while the first runs");
        drop(first);
        assert!(Installing::begin().is_some(), "after the first has failed");
    }
}
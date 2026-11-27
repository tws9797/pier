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

/// Downloads the newer release, checks its signature and runs its installer.
/// On Windows the installer needs Pier closed, so this ends the process; the
/// installer starts the new version when it is done.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    let update = updater(app)?
        .check()
        .await
        .map_err(|e| format!("Could not look for updates: {e}"))?
        .ok_or("There is no newer version to install")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("Could not install {}: {e}", update.version))?;
    app.restart()
}

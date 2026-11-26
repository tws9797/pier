//! Points the docker CLI on Windows at Pier's pipe.
//!
//! The CLI keeps named "contexts" and uses one of them by default. Pier adds
//! one called `pier` and selects it, by running the CLI itself: it owns the
//! files those contexts live in, and without the CLI there is nothing a
//! context would be for.

use serde::Serialize;
use std::path::Path;
use std::time::Duration;

const NAME: &str = "pier";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// docker.exe was found on PATH.
    cli: bool,
    /// The context docker commands use now.
    current: Option<String>,
    /// That context is Pier's.
    active: bool,
    /// While Pier's is in use: the one that was in use before, which
    /// switching back returns to.
    previous: Option<String>,
}

enum Failure {
    /// There is no docker.exe to run.
    Missing,
    Said(String),
}

impl From<Failure> for String {
    fn from(f: Failure) -> String {
        match f {
            Failure::Missing => "docker.exe was not found on PATH".into(),
            Failure::Said(text) => text,
        }
    }
}

async fn docker(args: &[&str]) -> Result<String, Failure> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
        Err(_) => return Err(Failure::Said("docker did not answer in time".into())),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(Failure::Missing),
        Ok(Err(e)) => return Err(Failure::Said(e.to_string())),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(Failure::Said(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

/// `note` is the file that remembers which context was in use before Pier's.
pub async fn state(note: &Path) -> State {
    match docker(&["context", "show"]).await {
        Ok(current) => {
            let active = current == NAME;
            State { cli: true, active, current: Some(current), previous: remembered(note).filter(|_| active) }
        }
        Err(Failure::Missing) => State { cli: false, current: None, active: false, previous: None },
        Err(Failure::Said(_)) => State { cli: true, current: None, active: false, previous: None },
    }
}

fn remembered(note: &Path) -> Option<String> {
    let name = std::fs::read_to_string(note).ok()?.trim().to_string();
    (!name.is_empty() && name != NAME).then_some(name)
}

/// Where `leave` goes: the context remembered when Pier's was selected, if
/// the CLI still has it, else the CLI's built-in `default`.
fn way_back(remembered: Option<String>, existing: &str) -> String {
    remembered.filter(|name| existing.lines().any(|n| n.trim() == name)).unwrap_or_else(|| "default".into())
}

/// Makes the `pier` context lead to `host` (creating it, or correcting one
/// left by an older version) and selects it, remembering in `note` which
/// context was in use until now.
pub async fn use_pier(host: &str, note: &Path) -> Result<(), String> {
    let before = docker(&["context", "show"]).await?;
    if before != NAME {
        // Written first: a switch that cannot be undone later is not made.
        if let Some(dir) = note.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(note, &before).map_err(|e| format!("Could not save settings to {}: {e}", note.display()))?;
    }
    let endpoint = format!("host={host}");
    match docker(&["context", "inspect", NAME, "--format", "{{.Endpoints.docker.Host}}"]).await {
        Ok(existing) if existing == host => {}
        Ok(_) => {
            docker(&["context", "update", NAME, "--docker", &endpoint]).await?;
        }
        Err(Failure::Missing) => return Err(Failure::Missing.into()),
        Err(Failure::Said(_)) => {
            docker(&["context", "create", NAME, "--description", "Pier", "--docker", &endpoint]).await?;
        }
    }
    docker(&["context", "use", NAME]).await?;
    Ok(())
}

/// Selects the context docker commands used before Pier's was selected (see
/// `way_back`). The `pier` context is left in place for `--context pier`.
pub async fn leave(note: &Path) -> Result<(), String> {
    let names = docker(&["context", "ls", "--format", "{{.Name}}"]).await?;
    docker(&["context", "use", &way_back(remembered(note), &names)]).await?;
    let _ = std::fs::remove_file(note);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_goes_back_to_the_context_that_was_in_use() {
        let existing = "default\ndesktop-linux\nmy-server\npier\n";
        assert_eq!(way_back(Some("my-server".into()), existing), "my-server");
        // Not to Docker Desktop's just because it exists.
        assert_eq!(way_back(None, existing), "default");
        // The remembered one was removed since.
        assert_eq!(way_back(Some("gone".into()), existing), "default");
    }

    #[test]
    fn pier_itself_is_never_remembered_as_the_way_back() {
        let dir = std::env::temp_dir().join(format!("pier-context-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("docker-context");
        assert_eq!(remembered(&note), None);
        std::fs::write(&note, "my-server\n").unwrap();
        assert_eq!(remembered(&note), Some("my-server".into()));
        std::fs::write(&note, "pier").unwrap();
        assert_eq!(remembered(&note), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

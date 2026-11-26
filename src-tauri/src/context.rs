//! Points the docker CLI on Windows at Pier's pipe.
//!
//! The CLI keeps named "contexts" and uses one of them by default. Pier adds
//! one called `pier` and selects it, by running the CLI itself: it owns the
//! files those contexts live in, and without the CLI there is nothing a
//! context would be for.

use serde::Serialize;
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

pub async fn state() -> State {
    match docker(&["context", "show"]).await {
        Ok(current) => State { cli: true, active: current == NAME, current: Some(current) },
        Err(Failure::Missing) => State { cli: false, current: None, active: false },
        Err(Failure::Said(_)) => State { cli: true, current: None, active: false },
    }
}

/// Makes the `pier` context lead to `host` (creating it, or correcting one
/// left by an older version) and selects it.
pub async fn use_pier(host: &str) -> Result<(), String> {
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

/// Selects what docker commands used before Pier: Docker Desktop's context
/// if there is one, else the CLI's built-in default. The `pier` context is
/// left in place for `--context pier`.
pub async fn leave() -> Result<(), String> {
    let names = docker(&["context", "ls", "--format", "{{.Name}}"]).await?;
    let back = if names.lines().any(|n| n.trim() == "desktop-linux") { "desktop-linux" } else { "default" };
    docker(&["context", "use", back]).await?;
    Ok(())
}

//! Exposes the WSL Docker socket as a Windows named pipe so `docker.exe`,
//! Compose and editors on the Windows side can use the engine:
//!
//!   docker context create pier --docker "host=npipe:////./pipe/pier_docker"
//!   docker context use pier

use std::sync::Arc;

use crate::engine::Engine;

pub const PIPE_NAME: &str = r"\\.\pipe\pier_docker";

#[cfg(windows)]
pub async fn run(engine: Arc<Engine>) {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut server = match ServerOptions::new().first_pipe_instance(true).create(PIPE_NAME) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bridge: could not create {PIPE_NAME}: {e}");
            return;
        }
    };

    loop {
        if server.connect().await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }
        let client = server;
        server = match ServerOptions::new().create(PIPE_NAME) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bridge: could not re-create pipe: {e}");
                return;
            }
        };

        let Ok(distro) = engine.distro() else { continue };
        let engine = engine.clone();
        tokio::spawn(async move {
            // A connected tool keeps the engine awake, and wakes it first if
            // Pier had let it go while idle.
            // Registering first means the idle monitor either sees this
            // connection or has already put the engine to sleep, in which
            // case `wake` returns only once it is back.
            // The connection is for `distro` from here to its end, whatever
            // gets selected meanwhile; it counts as use of that distro only.
            engine.connected(&distro);
            // After a deliberate stop the connection is closed unanswered:
            // relaying it would start wsl.exe in the distro, boot it again
            // and undo a "shut down the distro" the user asked for. The tool
            // sees the same thing as with no engine at all.
            if engine.wake(&distro).await {
                relay(&distro, client).await;
            }
            engine.disconnected(&distro);
        });
    }
}

#[cfg(windows)]
async fn relay(distro: &str, client: tokio::net::windows::named_pipe::NamedPipeServer) {
    let Ok(mut relay) = crate::wsl::relay(distro) else { return };
    let (Some(mut from_engine), Some(mut to_engine)) = (relay.stdout.take(), relay.stdin.take()) else {
        return;
    };
    let (mut from_client, mut to_client) = tokio::io::split(client);
    // Named pipes have no half-close, so the connection is over as soon as
    // either side is.
    tokio::select! {
        _ = tokio::io::copy(&mut from_client, &mut to_engine) => {}
        _ = tokio::io::copy(&mut from_engine, &mut to_client) => {}
    }
    drop(relay); // ends wsl.exe and its helper, so nothing is left holding the distro
}

#[cfg(not(windows))]
pub async fn run(_engine: Arc<Engine>) {}

//! A tiny Docker Engine API client.
//!
//! Windows cannot open a unix socket that lives inside WSL2, so each request
//! spawns a relay process (`wsl.exe ... nc -U /var/run/docker.sock`) and
//! speaks plain HTTP/1.1 over its stdin/stdout.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::time::Duration;

use crate::wsl;

const UNREACHABLE: &str = "Docker engine is not reachable";

/// An in-flight response. Dropping it kills the relay process.
pub struct Reply {
    pub response: Response<Incoming>,
    pub relay: wsl::Session,
}

/// How long Docker gets to accept a read and send its response headers.
/// Streams (`/events`, logs) send theirs at once and are then read for as
/// long as they last; this only bounds the wait for them to begin.
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);

/// Opens a request and returns once the response headers are in.
///
/// Reads (GET) give up after `HEADER_TIMEOUT`, which drops and so kills the
/// relay; without that an engine that accepts the connection and then says
/// nothing would hang the event watcher, a start, and everything queued
/// behind it. Writes are not limited: Docker answers a prune or a stop only
/// when it is done.
pub async fn send(
    distro: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Reply, String> {
    if method != Method::GET {
        return open(distro, method, path, body).await;
    }
    tokio::time::timeout(HEADER_TIMEOUT, open(distro, method, path, body))
        .await
        .unwrap_or_else(|_| Err("Docker did not answer in time".into()))
}

async fn open(
    distro: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Reply, String> {
    let mut relay = wsl::relay(distro).map_err(|e| format!("Could not start relay: {e}"))?;
    let stdout = relay.stdout.take().ok_or(UNREACHABLE)?;
    let stdin = relay.stdin.take().ok_or(UNREACHABLE)?;
    let io = TokioIo::new(tokio::io::join(stdout, stdin));

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|_| UNREACHABLE.to_string())?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut req = Request::builder().method(method).uri(path).header("Host", "docker");
    let payload = match body {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Bytes::from(v.to_string())
        }
        None => Bytes::new(),
    };
    let req = req.body(Full::new(payload)).map_err(|e| e.to_string())?;

    let response = sender
        .send_request(req)
        .await
        .map_err(|_| UNREACHABLE.to_string())?;
    Ok(Reply { response, relay })
}

/// How long a read may take before it is given up. Giving up drops the reply,
/// which kills its relay process.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// Several GETs over one connection, answered in order. A request that fails
/// or is refused gives `Null` in its place; the rest still go through.
///
/// One relay process serves all of them. Asked one by one, the resource
/// figures of twenty containers would start twenty `wsl.exe` every few
/// seconds; this starts one.
///
/// The connection can be kept and used again (`Link`): something asked every
/// few seconds then starts no process at all after the first time.
pub struct Link {
    sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    /// Dropping the link drops this, which ends the relay.
    _relay: wsl::Session,
}

impl Link {
    pub async fn open(distro: &str) -> Result<Self, String> {
        let mut relay = wsl::relay(distro).map_err(|e| format!("Could not start relay: {e}"))?;
        let stdout = relay.stdout.take().ok_or(UNREACHABLE)?;
        let stdin = relay.stdin.take().ok_or(UNREACHABLE)?;
        let io = TokioIo::new(tokio::io::join(stdout, stdin));
        let (sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|_| UNREACHABLE.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(Self { sender, _relay: relay })
    }

    /// The answers to `paths`, in order. An error means the link is no
    /// longer usable (closed by the engine, or too slow) and should be
    /// thrown away.
    pub async fn get_each(&mut self, paths: &[String]) -> Result<Vec<Value>, String> {
        tokio::time::timeout(READ_TIMEOUT, async {
            let mut answers = Vec::with_capacity(paths.len());
            for path in paths {
                self.sender.ready().await.map_err(|_| UNREACHABLE.to_string())?;
                let request = Request::builder()
                    .method(Method::GET)
                    .uri(path.as_str())
                    .header("Host", "docker")
                    .body(Full::new(Bytes::new()))
                    .map_err(|e| e.to_string())?;
                let response = self.sender.send_request(request).await.map_err(|_| UNREACHABLE.to_string())?;
                let ok = response.status().is_success();
                let body = response.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
                let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
                answers.push(if ok { parsed } else { Value::Null });
            }
            Ok(answers)
        })
        .await
        .unwrap_or_else(|_| Err("Docker did not answer in time".into()))
    }
}

/// Send a request and return the parsed JSON body (Null when empty).
/// Non-2xx answers become `Err` carrying Docker's own message.
///
/// Reads (GET) time out so a hung engine cannot leave relays piling up.
/// Writes do not: pruning or stopping can legitimately take minutes.
pub async fn json(
    distro: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    if method != Method::GET {
        return exchange(distro, method, path, body).await;
    }
    tokio::time::timeout(READ_TIMEOUT, exchange(distro, method, path, body))
        .await
        .unwrap_or_else(|_| Err("Docker did not answer in time".into()))
}

async fn exchange(
    distro: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let reply = send(distro, method, path, body).await?;
    let status = reply.response.status();
    let bytes = reply
        .response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    if status.is_success() || status.as_u16() == 304 {
        Ok(parsed)
    } else {
        Err(parsed
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Docker returned HTTP {status}")))
    }
}

pub async fn ping(distro: &str) -> bool {
    matches!(send(distro, Method::GET, "/_ping", None).await, Ok(r) if r.response.status().is_success())
}

/// Splits Docker's multiplexed log stream (8-byte header per frame) into
/// text. Containers started with a TTY send raw bytes instead.
pub struct LogDecoder {
    tty: bool,
    buf: Vec<u8>,
}

impl LogDecoder {
    pub fn new(tty: bool) -> Self {
        Self { tty, buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> String {
        if self.tty {
            return String::from_utf8_lossy(chunk).into_owned();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = String::new();
        let mut pos = 0;
        while self.buf.len() - pos >= 8 {
            let len = u32::from_be_bytes([
                self.buf[pos + 4],
                self.buf[pos + 5],
                self.buf[pos + 6],
                self.buf[pos + 7],
            ]) as usize;
            if self.buf.len() - pos - 8 < len {
                break;
            }
            out.push_str(&String::from_utf8_lossy(&self.buf[pos + 8..pos + 8 + len]));
            pos += 8 + len;
        }
        self.buf.drain(..pos);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream: u8, text: &str) -> Vec<u8> {
        let mut f = vec![stream, 0, 0, 0];
        f.extend_from_slice(&(text.len() as u32).to_be_bytes());
        f.extend_from_slice(text.as_bytes());
        f
    }

    #[test]
    fn demuxes_frames_split_across_chunks() {
        let mut bytes = frame(1, "hello\n");
        bytes.extend(frame(2, "oops\n"));
        let mut d = LogDecoder::new(false);
        let (a, b) = bytes.split_at(11);
        assert_eq!(d.push(a), "");
        assert_eq!(d.push(b), "hello\noops\n");
    }

    #[test]
    fn tty_is_passed_through() {
        assert_eq!(LogDecoder::new(true).push(b"raw"), "raw");
    }
}

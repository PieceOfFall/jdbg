//! CLI-side client: connect to or auto-spawn the daemon, send one Request, receive one Response.

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use anyhow::Context;
use interprocess::local_socket::{Stream as LocalStream, prelude::*};

use crate::protocol::{Request, Response};
use crate::registry;

/// Connection timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Poll interval after auto-spawning the daemon.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Send a request and receive a response. This public API is also used by `daemon::stop_daemon`.
pub fn send_request(req: &Request) -> anyhow::Result<Response> {
    match attempt_send(req, connect_or_spawn()?) {
        Ok(resp) => Ok(resp),
        // The daemon closed the connection before responding (broken pipe / reset /
        // EOF). This is a transient race — commonly right after the daemon was
        // (re)spawned, or when a stale socket was accepted then dropped. Reconnect
        // once (re-spawning the daemon if it is now gone) and resend.
        Err(SendError::Transient(first)) => {
            let stream = connect_or_spawn()
                .with_context(|| format!("reconnect after transient daemon IPC error: {first}"))?;
            attempt_send(req, stream).map_err(SendError::into_anyhow)
        }
        Err(SendError::Fatal(e)) => Err(e),
    }
}

/// Send a request to an already-running daemon without auto-spawning another one.
pub fn send_request_to_existing(req: &Request) -> anyhow::Result<Response> {
    let sock_name = registry::socket_name();
    match attempt_send(req, try_connect(&sock_name)?) {
        Ok(resp) => Ok(resp),
        // Retry once on a transient close, but never auto-spawn: this path is used
        // when we deliberately only want to talk to a daemon that already exists.
        Err(SendError::Transient(first)) => {
            let stream = try_connect(&sock_name)
                .with_context(|| format!("reconnect after transient daemon IPC error: {first}"))?;
            attempt_send(req, stream).map_err(SendError::into_anyhow)
        }
        Err(SendError::Fatal(e)) => Err(e),
    }
}

/// Failure mode of a single send attempt, so callers can decide whether to retry.
enum SendError {
    /// The connection broke before a response was received (peer closed the socket).
    /// Safe to retry on a fresh connection — the request was not answered.
    Transient(std::io::Error),
    /// A non-connection failure (serialization or malformed response). Not retryable.
    Fatal(anyhow::Error),
}

impl SendError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            SendError::Transient(e) => anyhow::Error::new(e)
                .context("daemon closed the connection twice while sending the request"),
            SendError::Fatal(e) => e,
        }
    }
}

/// Classify an IO error from the write/read path: connection-close kinds are
/// transient (retryable), everything else is fatal.
fn classify_io(e: std::io::Error) -> SendError {
    match e.kind() {
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::NotConnected
        | std::io::ErrorKind::UnexpectedEof => SendError::Transient(e),
        _ => SendError::Fatal(anyhow::Error::new(e)),
    }
}

fn attempt_send(req: &Request, stream: LocalStream) -> Result<Response, SendError> {
    let mut writer = stream;
    let json = serde_json::to_string(req).map_err(|e| SendError::Fatal(e.into()))?;
    writer.write_all(json.as_bytes()).map_err(classify_io)?;
    writer.write_all(b"\n").map_err(classify_io)?;
    writer.flush().map_err(classify_io)?;

    let mut reader = BufReader::new(&writer);
    let mut line = String::new();
    let read = reader.read_line(&mut line).map_err(classify_io)?;
    if read == 0 || line.trim().is_empty() {
        // Peer closed cleanly before writing a response line — treat like a broken
        // pipe so the caller retries rather than surfacing a confusing parse error.
        return Err(SendError::Transient(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed the connection before responding",
        )));
    }

    let resp: Response =
        serde_json::from_str(line.trim()).map_err(|e| SendError::Fatal(e.into()))?;
    Ok(resp)
}

/// Connect to the daemon socket; if connecting fails, auto-spawn the daemon and retry.
fn connect_or_spawn() -> anyhow::Result<LocalStream> {
    let sock_name = registry::socket_name();

    // Try a direct connection first.
    if let Ok(stream) = try_connect(&sock_name) {
        return Ok(stream);
    }

    // Connection failed: auto-spawn the daemon.
    crate::daemon::spawn_daemon_detached()?;

    // Poll until the daemon is ready, bounded by CONNECT_TIMEOUT.
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not start within {}s (socket: {sock_name})",
                CONNECT_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
        if let Ok(stream) = try_connect(&sock_name) {
            return Ok(stream);
        }
    }
}

/// Try to connect once.
fn try_connect(sock_name: &str) -> Result<LocalStream, std::io::Error> {
    let name = sock_name
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    LocalStream::connect(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn is_transient(kind: ErrorKind) -> bool {
        matches!(
            classify_io(std::io::Error::new(kind, "x")),
            SendError::Transient(_)
        )
    }

    #[test]
    fn connection_close_kinds_are_transient() {
        // A daemon that closes the socket mid-request must be retryable, otherwise a
        // benign IPC race surfaces as a hard "Broken pipe" CLI failure.
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(is_transient(kind), "{kind:?} should be transient");
        }
    }

    #[test]
    fn other_io_errors_are_fatal() {
        for kind in [ErrorKind::PermissionDenied, ErrorKind::InvalidData] {
            assert!(!is_transient(kind), "{kind:?} should be fatal");
        }
    }
}

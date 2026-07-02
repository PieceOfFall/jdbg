//! CLI-side client: connect to or auto-spawn the daemon, send one Request, receive one Response.

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::{Stream as LocalStream, prelude::*};

use crate::protocol::{Request, Response};
use crate::registry;

/// Connection timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Poll interval after auto-spawning the daemon.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Send a request and receive a response. This public API is also used by `daemon::stop_daemon`.
pub fn send_request(req: &Request) -> anyhow::Result<Response> {
    let stream = connect_or_spawn()?;
    send_request_on_stream(req, stream)
}

/// Send a request to an already-running daemon without auto-spawning another one.
pub fn send_request_to_existing(req: &Request) -> anyhow::Result<Response> {
    let stream = try_connect(&registry::socket_name())?;
    send_request_on_stream(req, stream)
}

fn send_request_on_stream(req: &Request, stream: LocalStream) -> anyhow::Result<Response> {
    let mut writer = stream;
    let json = serde_json::to_string(req)?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(&writer);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: Response = serde_json::from_str(line.trim())?;
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

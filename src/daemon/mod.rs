//! Daemon subsystem: IPC listening, session management, and lifecycle.
//!
//! Architecture (§4):
//! - One daemon per user login, kept alive in the background.
//! - Uses `interprocess` LocalSocket (Windows named pipe / Unix abstract socket).
//! - The accept loop spawns one short-lived handler thread per connection.

pub mod handler;
pub mod manager;

use std::io;
use std::sync::Arc;

use interprocess::local_socket::{ListenerOptions, prelude::*};

use crate::registry::{self, DaemonInfo, Registry};
use manager::SessionManager;

/// Daemon main-loop entry point: bind the socket, register on-disk info, then accept connections.
///
/// Idempotent bind: if the socket is already in use because another daemon started first, return `Err`
/// so the caller can exit 0.
pub fn run_daemon() -> anyhow::Result<()> {
    let sock_name = registry::socket_name();
    let name = sock_name
        .clone()
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // Idempotent bind failed: another daemon is already serving, so exit quietly.
            eprintln!("[daemon] socket already in use, another daemon is serving. Exiting.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // Register on-disk daemon info.
    let registry = Registry::open()?;
    let info = DaemonInfo {
        pid: std::process::id(),
        socket_name: sock_name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        started_at: jiff::Zoned::now().to_string(),
    };
    registry.write_daemon(&info)?;

    eprintln!("[daemon] listening on {sock_name} (pid={})", info.pid);

    let mgr = Arc::new(SessionManager::new(registry));

    // Accept loop: spawn one short-lived thread per connection.
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let mgr = Arc::clone(&mgr);
                std::thread::spawn(move || {
                    if let Err(e) = handler::handle_connection(stream, &mgr) {
                        eprintln!("[daemon] connection error: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("[daemon] accept error: {e}");
            }
        }
    }

    // Cleanup. Normally unreachable because incoming() is an infinite iterator.
    mgr.shutdown();
    Ok(())
}

/// Detached spawn helper used by the CLI to auto-start the daemon.
///
/// Windows: `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`，stdio null。
/// Unix: `setsid` via pre_exec，stdio null。
pub fn spawn_daemon_detached() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__daemon");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP (0x200) | DETACHED_PROCESS (0x08)
        cmd.creation_flags(0x0000_0208);
    }

    #[cfg(unix)]
    {
        // setsid would require libc/nix (§9 excludes them). For now, stdio null plus no parent wait gives basic detach.
        // A full controlling-terminal detach can be added here later if needed.
    }

    cmd.spawn()?;
    Ok(())
}

/// Stop the daemon by connecting to the socket and sending `DaemonStop`.
pub fn stop_daemon() -> anyhow::Result<()> {
    use crate::protocol::{Command, Request};
    let req = Request::new(Command::DaemonStop, None);
    let resp = crate::client::send_request(&req)?;
    if resp.ok {
        println!("Daemon stopped.");
    } else if let Some(e) = resp.error {
        eprintln!("Failed to stop daemon: {}", e.message);
    }
    Ok(())
}

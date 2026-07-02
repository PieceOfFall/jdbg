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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use interprocess::local_socket::{ListenerNonblockingMode, ListenerOptions, prelude::*};

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
            // On Unix (macOS, Linux without abstract namespace fallback), a stale socket file may
            // linger after an unclean daemon exit. Probe: if we can connect, a live daemon owns
            // it — exit quietly. If connect fails, the file is orphaned — remove and retry once.
            if probe_socket_alive(&sock_name) {
                eprintln!("[daemon] socket already in use, another daemon is serving. Exiting.");
                return Ok(());
            }
            eprintln!("[daemon] stale socket detected, reclaiming...");
            remove_stale_socket(&sock_name);
            let name = sock_name
                .clone()
                .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;
            ListenerOptions::new().name(name).create_sync()?
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
    let shutdown = Arc::new(AtomicBool::new(false));
    listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

    // Accept loop: spawn one short-lived thread per connection, and poll a shutdown flag so
    // `daemon stop` can return a response before the daemon exits naturally.
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok(stream) => {
                let mgr = Arc::clone(&mgr);
                let shutdown = Arc::clone(&shutdown);
                std::thread::spawn(move || {
                    if let Err(e) = handler::handle_connection(stream, &mgr, &shutdown) {
                        eprintln!("[daemon] connection error: {e}");
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
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
/// Windows uses detached process flags and null stdio. Unix uses `setsid` in `pre_exec`.
pub fn spawn_daemon_detached() -> io::Result<()> {
    #[cfg(windows)]
    detach_std_handles_from_children();

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
        use std::os::unix::process::CommandExt;

        unsafe extern "C" {
            fn setsid() -> i32;
        }

        // SAFETY: this child hook calls only `setsid` before exec and reports OS failure directly.
        unsafe {
            cmd.pre_exec(|| {
                if setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }

    cmd.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn detach_std_handles_from_children() {
    use std::os::windows::io::AsRawHandle;

    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    unsafe extern "system" {
        fn SetHandleInformation(h: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    for raw in [stdout.as_raw_handle(), stderr.as_raw_handle()] {
        if !raw.is_null() {
            unsafe { SetHandleInformation(raw as *mut std::ffi::c_void, HANDLE_FLAG_INHERIT, 0) };
        }
    }
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

/// Probe whether a live daemon is listening on the named socket.
/// Returns true if a connection succeeds (another daemon is alive), false otherwise (stale file).
fn probe_socket_alive(sock_name: &str) -> bool {
    use interprocess::local_socket::{Stream as LocalStream, prelude::*};
    let Ok(name) = sock_name.to_ns_name::<interprocess::local_socket::GenericNamespaced>() else {
        return false;
    };
    LocalStream::connect(name).is_ok()
}

/// Remove the stale socket file on Unix. On macOS/non-Linux Unix, `GenericNamespaced` maps to
/// `SpecialDirUdSocket` which places the socket at `/tmp/<name>`. On Linux with abstract namespace
/// there is no file to remove, so this is a no-op there (bind would not have returned AddrInUse
/// for a dead process in the first place).
#[cfg(unix)]
fn remove_stale_socket(sock_name: &str) {
    let path = format!("/tmp/{sock_name}");
    let _ = std::fs::remove_file(&path);
}

#[cfg(not(unix))]
fn remove_stale_socket(_sock_name: &str) {
    // Windows uses named pipes; stale socket files are not a concern.
}

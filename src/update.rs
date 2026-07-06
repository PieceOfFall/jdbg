//! `jdbg update`: stop the daemon, remove old registrations, install the latest release,
//! then re-register the same coding agents that were configured before.

use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::client;
use crate::protocol::{Command, Request};
use crate::registry::{DaemonInfo, Registry};
use crate::setup;
use crate::update_sidecar;

const REPO: &str = "PieceOfFall/jdbg";
const DAEMON_GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const DAEMON_FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const DAEMON_STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn stop_daemon_before_update() -> Result<()> {
    let registry = Registry::open().ok();
    let initial_info = registry.as_ref().and_then(|r| r.read_daemon());

    let req = Request::new(Command::DaemonStop, None);
    let stop_reached_daemon = client::send_request_to_existing(&req).is_ok();
    if !stop_reached_daemon {
        if let (Some(registry), Some(info)) = (&registry, &initial_info) {
            if !process_is_alive(info.pid) {
                registry.remove_daemon();
            }
        }
        return Ok(());
    }

    if wait_for_daemon_exit(initial_info.as_ref(), DAEMON_GRACEFUL_STOP_TIMEOUT) {
        return Ok(());
    }

    let info = registry
        .as_ref()
        .and_then(|r| r.read_daemon())
        .or(initial_info)
        .context("daemon is still running, but daemon.json has no pid to stop")?;

    ensure!(
        info.pid != std::process::id(),
        "refusing to kill the current update process as daemon pid {}",
        info.pid
    );

    stop_process(info.pid)
        .with_context(|| format!("failed to stop running jdbg daemon process {}", info.pid))?;

    if !wait_for_daemon_exit(Some(&info), DAEMON_FORCE_STOP_TIMEOUT) {
        bail!(
            "jdbg daemon process {} did not exit; stop it manually and run `jdbg update` again",
            info.pid
        );
    }

    if let Some(registry) = registry {
        registry.remove_daemon();
    }
    Ok(())
}

fn wait_for_daemon_exit(info: Option<&DaemonInfo>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let socket_down = daemon_status_to_existing().is_err();
        let pid_down = info
            .map(|i| !process_is_alive(i.pid))
            .unwrap_or(socket_down);
        if socket_down && pid_down {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(DAEMON_STOP_POLL_INTERVAL);
    }
}

fn daemon_status_to_existing() -> Result<()> {
    let req = Request::new(Command::DaemonStatus, None);
    client::send_request_to_existing(&req).map(|_| ())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let rc = unsafe { kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }

    // EPERM means the process exists but this user cannot signal it.
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(unix)]
fn stop_process(pid: u32) -> std::io::Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const SIGKILL: i32 = 9;

    if pid > i32::MAX as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pid is outside the platform pid range",
        ));
    }

    let rc = unsafe { kill(pid as i32, SIGKILL) };
    if rc == 0 || !process_is_alive(pid) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn GetExitCodeProcess(process: *mut c_void, exit_code: *mut u32) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }

    let mut exit_code = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe { CloseHandle(handle) };
    ok && exit_code == STILL_ACTIVE
}

#[cfg(windows)]
fn stop_process(pid: u32) -> std::io::Result<()> {
    use std::ffi::c_void;

    const PROCESS_TERMINATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x00100000;
    const WAIT_OBJECT_0: u32 = 0x00000000;
    const WAIT_TIMEOUT: u32 = 0x00000102;

    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    let terminated = unsafe { TerminateProcess(handle, 1) } != 0;
    let wait = if terminated {
        unsafe { WaitForSingleObject(handle, DAEMON_FORCE_STOP_TIMEOUT.as_millis() as u32) }
    } else {
        WAIT_TIMEOUT
    };
    unsafe { CloseHandle(handle) };

    if terminated && wait == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn install_command() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "powershell".to_string(),
            vec![
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                format!(
                    "irm https://github.com/{REPO}/releases/latest/download/java-agent-debugger-installer.ps1 | iex"
                ),
            ],
        )
    } else {
        (
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!(
                    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/{REPO}/releases/latest/download/java-agent-debugger-installer.sh | sh"
                ),
            ],
        )
    }
}

#[cfg(windows)]
fn move_self_aside() -> Result<Option<std::path::PathBuf>> {
    let current_exe = std::env::current_exe().context("cannot determine current exe path")?;
    let old_path = current_exe.with_extension("exe.old");

    let _ = std::fs::remove_file(&old_path);

    if std::fs::rename(&current_exe, &old_path).is_ok() {
        return Ok(Some(old_path));
    }

    let unique_path = current_exe.with_extension(format!("exe.old.{}", std::process::id()));
    std::fs::rename(&current_exe, &unique_path).with_context(|| {
        format!(
            "cannot rename {} (tried .old and .old.{}; is another jdbg process running? Stop the daemon with `jdbg daemon stop` first)",
            current_exe.display(),
            std::process::id()
        )
    })?;
    Ok(Some(unique_path))
}

#[cfg(not(windows))]
fn move_self_aside() -> Result<Option<std::path::PathBuf>> {
    Ok(None)
}

fn cleanup_old(old_path: Option<std::path::PathBuf>) {
    if let Some(p) = old_path {
        let _ = std::fs::remove_file(p);
    }
}

pub fn run_update() -> Result<()> {
    let targets = setup::configured_targets_or_default()?;
    let backend = setup::configured_backend_or_default()?;
    let target_arg = setup::targets_to_arg(&targets);
    let backend_arg = backend.id();
    let current_exe = std::env::current_exe().context("cannot determine current exe path")?;

    println!("[1/5] Stopping any running jdbg daemon...");
    stop_daemon_before_update()?;

    println!("[2/5] Removing old jdbg registration for configured agents ({target_arg})...");
    setup::run_setup(true, false, Some(&target_arg), true, None)?;

    println!("[3/5] Installing latest jdbg from GitHub releases...");
    let old_path = move_self_aside()?;

    let (program, args) = install_command();
    let status = ProcessCommand::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("failed to run installer: {program}"))?;

    if !status.success() {
        bail!("installer exited with status {status}. Check network and try again.");
    }

    cleanup_old(old_path);

    println!("[4/5] Installing JDI sidecar from the official release archive...");
    let sidecar_path = update_sidecar::install_from_latest_release_next_to(&current_exe)?;
    println!("Installed JDI sidecar at {}.", sidecar_path.display());

    println!("[5/5] Re-registering jdbg for configured agents ({target_arg})...");
    let setup_bin = if cfg!(windows) { "jdbg.exe" } else { "jdbg" };
    let setup_status = ProcessCommand::new(setup_bin)
        .arg("setup")
        .arg("--target")
        .arg(&target_arg)
        .arg("--backend")
        .arg(backend_arg)
        .arg("--yes")
        .status();

    match setup_status {
        Ok(s) if s.success() => {}
        _ => {
            setup::run_setup(false, false, Some(&target_arg), true, Some(backend_arg))?;
        }
    }

    println!(
        "\njdbg updated successfully. Restart or reload the configured agent(s) to use the new version."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::TargetId;

    #[test]
    fn update_target_arg_preserves_detected_targets() {
        assert_eq!(setup::targets_to_arg(&[TargetId::Codex]), "codex");
        assert_eq!(
            setup::targets_to_arg(&[
                TargetId::Claude,
                TargetId::Codex,
                TargetId::Opencode,
                TargetId::Pi
            ]),
            "claude,codex,opencode,pi"
        );
    }
}

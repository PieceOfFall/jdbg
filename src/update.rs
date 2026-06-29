//! `jdbg update`: remove old registrations, install the latest release,
//! then re-register the same coding agents that were configured before.

use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};

use crate::client;
use crate::protocol::{Command, Request};
use crate::setup;

const REPO: &str = "PieceOfFall/jdbg";

fn stop_daemon_if_running() {
    let req = Request::new(Command::DaemonStop, None);
    let _ = client::send_request(&req);
    std::thread::sleep(std::time::Duration::from_millis(300));
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
    let target_arg = setup::targets_to_arg(&targets);

    stop_daemon_if_running();

    println!("[1/3] Removing old jdbg registration for configured agents ({target_arg})...");
    setup::run_setup(true, false, Some(&target_arg), true)?;

    println!("[2/3] Installing latest jdbg from GitHub releases...");
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

    println!("[3/3] Re-registering jdbg for configured agents ({target_arg})...");
    let setup_bin = if cfg!(windows) { "jdbg.exe" } else { "jdbg" };
    let setup_status = ProcessCommand::new(setup_bin)
        .arg("setup")
        .arg("--target")
        .arg(&target_arg)
        .arg("--yes")
        .status();

    match setup_status {
        Ok(s) if s.success() => {}
        _ => {
            setup::run_setup(false, false, Some(&target_arg), true)?;
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
            setup::targets_to_arg(&[TargetId::Claude, TargetId::Codex, TargetId::Pi]),
            "claude,codex,pi"
        );
    }
}

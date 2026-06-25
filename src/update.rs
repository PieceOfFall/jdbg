//! `jdbg update` — 一键更新：卸载旧注册 → 安装最新 release → 重新注册。
//!
//! Windows 特殊处理：正在运行的 exe 无法被覆盖，但可以被**重命名**。
//! 安装前把自身重命名为 `jdbg.exe.old`，installer 就能写入新 `jdbg.exe`。

use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};

use crate::setup;

use crate::client;
use crate::protocol::{Command, Request};

const REPO: &str = "PieceOfFall/jdbg";

/// Best-effort: stop the background daemon so it releases its handle on jdbg.exe.
fn stop_daemon_if_running() {
    let req = Request::new(Command::DaemonStop, None);
    let _ = client::send_request(&req);
    // Give the daemon a moment to exit.
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// 检测当前平台，返回对应的安装命令。
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

/// Windows: rename the running exe out of the way so the installer can write the new one.
/// Returns the old path for post-install cleanup. On non-Windows this is a no-op.
#[cfg(windows)]
fn move_self_aside() -> Result<Option<std::path::PathBuf>> {
    let current_exe = std::env::current_exe().context("cannot determine current exe path")?;
    let old_path = current_exe.with_extension("exe.old");

    // Try removing a leftover .old from a previous update.
    let _ = std::fs::remove_file(&old_path);

    // Attempt the primary rename.
    match std::fs::rename(&current_exe, &old_path) {
        Ok(()) => return Ok(Some(old_path)),
        Err(_) => {}
    }

    // .old may be locked by another process (e.g. daemon). Use a unique suffix.
    let unique_path = current_exe.with_extension(format!("exe.old.{}", std::process::id()));
    std::fs::rename(&current_exe, &unique_path)
        .with_context(|| format!("cannot rename {} (tried .old and .old.{} — is another jdbg process running? Stop the daemon with `jdbg daemon stop` first)",
            current_exe.display(), std::process::id()))?;
    Ok(Some(unique_path))
}

#[cfg(not(windows))]
fn move_self_aside() -> Result<Option<std::path::PathBuf>> {
    Ok(None)
}

/// 安装完成后清理 `.old` 文件（best-effort）。
fn cleanup_old(old_path: Option<std::path::PathBuf>) {
    if let Some(p) = old_path {
        let _ = std::fs::remove_file(p);
    }
}

pub fn run_update() -> Result<()> {
    // Step 0: Stop the daemon if running (its handle on jdbg.exe blocks rename on Windows)
    stop_daemon_if_running();

    // Step 1: Remove old setup
    println!("[1/3] Removing old jdbg registration...");
    setup::run_setup(true, false)?;

    // Step 2: Move self aside (Windows) + Install latest release
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

    // Step 3: Re-register (run the NEW jdbg setup)
    // 此时新的 jdbg 已安装到 PATH 中。直接调用新二进制确保注册的 skill 是最新版。
    println!("[3/3] Re-registering jdbg with Claude Code...");
    let setup_bin = if cfg!(windows) { "jdbg.exe" } else { "jdbg" };
    let setup_status = ProcessCommand::new(setup_bin)
        .args(["setup"])
        .status();

    match setup_status {
        Ok(s) if s.success() => {}
        _ => {
            // fallback: 用当前进程内嵌的 setup（可能是旧版 skill，但总比失败好）
            setup::run_setup(false, false)?;
        }
    }

    println!("\n✓ jdbg updated successfully. Restart Claude Code to use the new version.");
    Ok(())
}

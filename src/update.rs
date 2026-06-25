//! `jdbg update` — 一键更新：卸载旧注册 → 安装最新 release → 重新注册。
//!
//! Windows 特殊处理：正在运行的 exe 无法被覆盖，但可以被**重命名**。
//! 安装前把自身重命名为 `jdbg.exe.old`，installer 就能写入新 `jdbg.exe`。

use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};

use crate::setup;

const REPO: &str = "PieceOfFall/jdbg";

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

/// Windows：把当前正在运行的 exe 重命名为 `.old`，让 installer 能写入新文件。
/// 返回 old path（用于安装后清理）。Non-Windows 上不做任何操作。
#[cfg(windows)]
fn move_self_aside() -> Result<Option<std::path::PathBuf>> {
    let current_exe = std::env::current_exe().context("cannot determine current exe path")?;
    let old_path = current_exe.with_extension("exe.old");
    // 如果上次遗留了 .old，先删掉
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(&current_exe, &old_path)
        .with_context(|| format!("cannot rename {} to {}", current_exe.display(), old_path.display()))?;
    Ok(Some(old_path))
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

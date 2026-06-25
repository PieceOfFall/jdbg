//! `jdbg update` — 一键更新：卸载旧注册 → 安装最新 release → 重新注册。

use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};

use crate::setup;

const REPO: &str = "PieceOfFall/jdbg";

/// 检测当前平台，返回对应的安装命令。
fn install_command() -> Result<(String, Vec<String>)> {
    if cfg!(windows) {
        Ok((
            "powershell".to_string(),
            vec![
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                format!(
                    "irm https://github.com/{REPO}/releases/latest/download/java-agent-debugger-installer.ps1 | iex"
                ),
            ],
        ))
    } else {
        Ok((
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!(
                    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/{REPO}/releases/latest/download/java-agent-debugger-installer.sh | sh"
                ),
            ],
        ))
    }
}

pub fn run_update() -> Result<()> {
    // Step 1: Remove old setup
    println!("[1/3] Removing old jdbg registration...");
    setup::run_setup(true, false)?;

    // Step 2: Install latest release
    println!("[2/3] Installing latest jdbg from GitHub releases...");
    let (program, args) = install_command()?;
    let status = ProcessCommand::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("failed to run installer: {program}"))?;

    if !status.success() {
        bail!("installer exited with status {status}. Check network and try again.");
    }

    // Step 3: Re-register
    println!("[3/3] Re-registering jdbg with Claude Code...");
    setup::run_setup(false, false)?;

    println!("\n✓ jdbg updated successfully. Restart Claude Code to use the new version.");
    Ok(())
}

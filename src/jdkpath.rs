//! 定位 jdb 可执行文件。
//!
//! 发现顺序（§10）：
//! 1. 用户显式指定 `--jdb-path`（调用方传入 `explicit`）
//! 2. `JAVA_HOME/bin/jdb(.exe)`
//! 3. 系统 PATH（`which` / `where`）
//! 4. 常见安装目录（TODO：后续补充）
//!
//! **JAVA_HOME 优先于 PATH**——本机 PATH 解析到 JDK 21，用户要 JDK 8。

use std::path::{Path, PathBuf};

use crate::error::Error;

/// jdb 可执行文件名（含 Windows .exe 后缀）。
#[cfg(windows)]
const JDB_EXE: &str = "jdb.exe";
#[cfg(not(windows))]
const JDB_EXE: &str = "jdb";

/// 按优先级定位 jdb。`explicit` 为用户通过 `--jdb-path` 提供的显式路径。
pub fn find_jdb(explicit: Option<&Path>) -> crate::error::Result<PathBuf> {
    let mut searched: Vec<String> = Vec::new();

    // 1. 显式路径
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        searched.push(format!("--jdb-path: {}", p.display()));
    }

    // 2. JAVA_HOME/bin/jdb(.exe)
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let candidate = PathBuf::from(&java_home).join("bin").join(JDB_EXE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(format!("JAVA_HOME: {}", candidate.display()));
    } else {
        searched.push("JAVA_HOME: (not set)".into());
    }

    // 3. PATH（使用 `which` crate 的逻辑手写，避免额外依赖）
    if let Some(found) = find_in_path() {
        return Ok(found);
    }
    searched.push("PATH: not found".into());

    // 4. 常见安装目录（TODO：后续阶段补充 Windows/Unix 扫描）

    Err(Error::JdbNotFound { searched })
}

/// 在 PATH 环境变量中查找 jdb。
fn find_in_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.to_string_lossy().split(sep) {
        let candidate = PathBuf::from(dir).join(JDB_EXE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

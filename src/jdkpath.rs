//! 定位 jdb 可执行文件。
//!
//! 发现顺序（§10）：
//! 1. 用户显式指定 `--jdb-path`（调用方传入 `explicit`）
//! 2. `JAVA_HOME/bin/jdb(.exe)`
//! 3. 系统 PATH（`which` / `where`）
//! 4. 常见安装目录（`Program Files\Java\*`、`.jdks\*`、`/usr/lib/jvm/*` …）
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
        if let Some(found) = jdb_in_home(Path::new(&java_home)) {
            return Ok(found);
        }
        searched.push(format!("JAVA_HOME: {}/bin/{JDB_EXE}", java_home));
    } else {
        searched.push("JAVA_HOME: (not set)".into());
    }

    // 3. PATH（使用 `which` crate 的逻辑手写，避免额外依赖）
    if let Some(found) = find_in_path() {
        return Ok(found);
    }
    searched.push("PATH: not found".into());

    // 4. 常见安装目录
    if let Some(found) = scan_common_dirs() {
        return Ok(found);
    }
    searched.push("common install dirs: not found".into());

    Err(Error::JdbNotFound { searched })
}

/// 检查 `home/bin/jdb(.exe)` 是否存在，存在则返回其路径。
fn jdb_in_home(home: &Path) -> Option<PathBuf> {
    let candidate = home.join("bin").join(JDB_EXE);
    candidate.is_file().then_some(candidate)
}

/// 扫描各平台常见 JDK 安装目录下的每个 JDK home，查找 jdb。
fn scan_common_dirs() -> Option<PathBuf> {
    for parent in common_jdk_parents() {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            // 直接布局 `<home>/bin` 与 macOS bundle 布局 `<home>/Contents/Home/bin`。
            if let Some(found) = jdb_in_home(&dir).or_else(|| jdb_in_home(&dir.join("Contents").join("Home"))) {
                return Some(found);
            }
        }
    }
    None
}

/// 各平台存放多个 JDK home 的父目录列表。
fn common_jdk_parents() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        dirs.push(PathBuf::from(&home).join(".jdks"));
    }
    #[cfg(windows)]
    {
        dirs.push(PathBuf::from(r"C:\Program Files\Java"));
        dirs.push(PathBuf::from(r"C:\Program Files\Eclipse Adoptium"));
        dirs.push(PathBuf::from(r"C:\Program Files\Microsoft"));
    }
    #[cfg(not(windows))]
    {
        dirs.push(PathBuf::from("/usr/lib/jvm"));
        dirs.push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
    }
    dirs
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

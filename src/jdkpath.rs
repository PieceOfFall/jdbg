//! Locate the jdb executable.
//!
//! Discovery order (§10):
//! 1. User-provided `--jdb-path` (passed in as `explicit`)
//! 2. `JAVA_HOME/bin/jdb(.exe)`
//! 3. System PATH (`which` / `where`)
//! 4. Common install directories (`Program Files\Java\*`, `.jdks\*`, `/usr/lib/jvm/*`, ...)
//!
//! **JAVA_HOME takes priority over PATH**: on this machine PATH resolves to JDK 21, while the user wants JDK 8.

use std::path::{Path, PathBuf};

use crate::error::Error;

/// jdb executable name, including the Windows `.exe` suffix.
#[cfg(windows)]
const JDB_EXE: &str = "jdb.exe";
#[cfg(not(windows))]
const JDB_EXE: &str = "jdb";

/// Locate jdb by priority. `explicit` is the path supplied by the user through `--jdb-path`.
pub fn find_jdb(explicit: Option<&Path>) -> crate::error::Result<PathBuf> {
    let mut searched: Vec<String> = Vec::new();

    // 1. Explicit path.
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

    // 3. PATH. This hand-rolls `which`-style logic to avoid another dependency.
    if let Some(found) = find_in_path() {
        return Ok(found);
    }
    searched.push("PATH: not found".into());

    // 4. Common install directories.
    if let Some(found) = scan_common_dirs() {
        return Ok(found);
    }
    searched.push("common install dirs: not found".into());

    Err(Error::JdbNotFound { searched })
}

/// Return `home/bin/jdb(.exe)` if it exists.
fn jdb_in_home(home: &Path) -> Option<PathBuf> {
    let candidate = home.join("bin").join(JDB_EXE);
    candidate.is_file().then_some(candidate)
}

/// Scan common per-platform JDK parent directories for jdb.
fn scan_common_dirs() -> Option<PathBuf> {
    for parent in common_jdk_parents() {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            // Direct `<home>/bin` layout and macOS bundle `<home>/Contents/Home/bin` layout.
            if let Some(found) =
                jdb_in_home(&dir).or_else(|| jdb_in_home(&dir.join("Contents").join("Home")))
            {
                return Some(found);
            }
        }
    }
    None
}

/// Parent directories that commonly contain multiple JDK homes on each platform.
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

/// Find jdb in the PATH environment variable.
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

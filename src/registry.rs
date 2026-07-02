//! On-disk registry: daemon.json + sessions.json.
//!
//! Paths are located through `directories::ProjectDirs` (§4):
//! - Windows: `%LOCALAPPDATA%\claude\jdbg\data\`
//! - Linux: `$XDG_DATA_HOME/jdbg`
//! - macOS: `~/Library/Application Support/dev.claude.jdbg`
//!
//! **The daemon is the single writer**, using atomic writes (temp+rename). The CLI only reads as an offline fallback.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Daemon liveness information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub socket_name: String,
    pub version: String,
    pub started_at: String,
}

/// One entry in the session list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub target: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jdb_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

fn default_backend() -> String {
    "jdb".into()
}

/// Registry path bundle.
pub struct Registry {
    pub data_dir: PathBuf,
}

impl Registry {
    /// Locate the platform data directory and create it if needed.
    pub fn open() -> Result<Self> {
        let dir = data_dir();
        fs::create_dir_all(&dir)?;
        Ok(Self { data_dir: dir })
    }

    pub fn daemon_path(&self) -> PathBuf {
        self.data_dir.join("daemon.json")
    }

    pub fn sessions_path(&self) -> PathBuf {
        self.data_dir.join("sessions.json")
    }

    /// Read daemon.json. Missing file returns None.
    pub fn read_daemon(&self) -> Option<DaemonInfo> {
        let path = self.daemon_path();
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Atomically write daemon.json using temp+rename.
    pub fn write_daemon(&self, info: &DaemonInfo) -> Result<()> {
        atomic_write(&self.daemon_path(), info)
    }

    /// Delete daemon.json when the daemon stops.
    pub fn remove_daemon(&self) {
        let _ = fs::remove_file(self.daemon_path());
    }

    /// Read sessions.json.
    pub fn read_sessions(&self) -> Vec<SessionRecord> {
        let path = self.sessions_path();
        let Ok(content) = fs::read_to_string(&path) else {
            return vec![];
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Atomically write sessions.json.
    pub fn write_sessions(&self, sessions: &[SessionRecord]) -> Result<()> {
        let content = serde_json::to_string_pretty(sessions).map_err(std::io::Error::other)?;
        let path = self.sessions_path();
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Locate the data directory.
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("JDBG_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(proj) = directories::ProjectDirs::from("dev", "claude", "jdbg") {
        proj.data_local_dir().to_path_buf()
    } else {
        // Fallback: .jdbg/ under the current directory.
        PathBuf::from(".jdbg")
    }
}

/// Atomically write a file: write a temp file in the same directory, then rename to avoid corruption on power loss.
fn atomic_write<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let content = serde_json::to_string_pretty(data).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Get the current username for the socket name.
pub fn current_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into())
}

/// Generate a stable socket name (§4: unique per user, derived from a sanitized username).
pub fn socket_name() -> String {
    let user = current_username()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>();
    format!("jdbg-{user}")
}

//! 磁盘注册表：daemon.json + sessions.json。
//!
//! 路径通过 `directories::ProjectDirs` 定位（§4）：
//! - Windows: `%LOCALAPPDATA%\claude\jdbg\data\`
//! - Linux: `$XDG_DATA_HOME/jdbg`
//! - macOS: `~/Library/Application Support/dev.claude.jdbg`
//!
//! **daemon 是单写者**——原子写（temp+rename）。CLI 只读（离线回退）。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Daemon 存活信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub socket_name: String,
    pub version: String,
    pub started_at: String,
}

/// Sessions 列表中的一条。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: String,
    pub target: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jdb_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 注册表路径集合。
pub struct Registry {
    pub data_dir: PathBuf,
}

impl Registry {
    /// 按平台定位数据目录，如不存在则创建。
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

    /// 读取 daemon.json（可能不存在 → None）。
    pub fn read_daemon(&self) -> Option<DaemonInfo> {
        let path = self.daemon_path();
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 原子写 daemon.json（temp+rename）。
    pub fn write_daemon(&self, info: &DaemonInfo) -> Result<()> {
        atomic_write(&self.daemon_path(), info)
    }

    /// 删除 daemon.json（daemon 停止时）。
    pub fn remove_daemon(&self) {
        let _ = fs::remove_file(self.daemon_path());
    }

    /// 读取 sessions.json。
    pub fn read_sessions(&self) -> Vec<SessionRecord> {
        let path = self.sessions_path();
        let Ok(content) = fs::read_to_string(&path) else { return vec![] };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// 原子写 sessions.json。
    pub fn write_sessions(&self, sessions: &[SessionRecord]) -> Result<()> {
        let content = serde_json::to_string_pretty(sessions)
            .map_err(std::io::Error::other)?;
        let path = self.sessions_path();
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// 定位数据目录。
fn data_dir() -> PathBuf {
    if let Some(proj) = directories::ProjectDirs::from("dev", "claude", "jdbg") {
        proj.data_local_dir().to_path_buf()
    } else {
        // fallback: 当前目录下 .jdbg/
        PathBuf::from(".jdbg")
    }
}

/// 原子写文件：先写 temp（同目录）再 rename，保证断电不损坏。
fn atomic_write<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let content = serde_json::to_string_pretty(data)
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 获取当前用户名（用于 socket name）。
pub fn current_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into())
}

/// 生成固定 socket name（§4：每用户唯一，派生自 sanitized username）。
pub fn socket_name() -> String {
    let user = current_username()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>();
    format!("jdbg-{user}")
}

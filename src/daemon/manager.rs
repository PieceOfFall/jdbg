//! SessionManager: owns all active sessions and provides create/find/list/remove operations.
//!
//! Internally uses `Mutex<HashMap<SessionId, DebugSession>>` for thread safety in the daemon accept loop.
//! The daemon is the single writer and also writes snapshots to sessions.json for offline inspection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::backend::DebugSession;
use crate::error::{Error, Result};
use crate::jdb::process::{AttachConfig, LaunchConfig};
use crate::jdi::session::JdiSession;
use crate::jdkpath;
use crate::protocol::{BackendKind, CommandResult, RunState, SessionInfo};
use crate::registry::{Registry, SessionRecord};
use crate::session::Session;

/// Generate an 8-character random session id.
fn gen_session_id() -> String {
    use rand::Rng;
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(8)
        .map(|b| (b as char).to_ascii_lowercase())
        .collect()
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, DebugSession>>,
    registry: Registry,
}

/// Parameter bundle for `create_launch`.
pub struct LaunchParams {
    pub main_class: String,
    pub backend: BackendKind,
    pub classpath: Vec<String>,
    pub sourcepath: Vec<String>,
    pub app_args: Vec<String>,
    pub jdb_args: Vec<String>,
    pub name: Option<String>,
    pub jdb_path: Option<String>,
}

/// Parameter bundle for `create_attach`.
pub struct AttachParams {
    pub backend: BackendKind,
    pub host: String,
    pub port: u16,
    pub sourcepath: Vec<String>,
    pub name: Option<String>,
    pub jdb_path: Option<String>,
}

impl SessionManager {
    pub fn new(registry: Registry) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            registry,
        }
    }

    /// Create a launch session.
    pub fn create_launch(&self, params: LaunchParams) -> Result<DebugSession> {
        if params.backend != BackendKind::Jdb {
            return Err(Error::UnsupportedBackend {
                backend: "jdi".into(),
                operation: "launch".into(),
            });
        }

        let jdb_path = match params.jdb_path {
            Some(ref p) => {
                let path = PathBuf::from(p);
                jdkpath::find_jdb(Some(&path))?
            }
            None => jdkpath::find_jdb(None)?,
        };

        let config = LaunchConfig {
            main_class: params.main_class,
            classpath: params.classpath.into_iter().map(PathBuf::from).collect(),
            sourcepath: params.sourcepath.into_iter().map(PathBuf::from).collect(),
            app_args: params.app_args,
            jdb_args: params.jdb_args,
        };

        let id = gen_session_id();
        let session = Session::launch(&jdb_path, &config, id.clone(), params.name)?;
        let session = DebugSession::Jdb(Arc::new(session));

        let mut map = self.sessions.lock().expect("sessions mutex poisoned");
        map.insert(id, session.clone());
        drop(map);

        self.persist_sessions();
        Ok(session)
    }

    /// Create an attach session connected to a running JVM's JDWP port.
    ///
    /// Deduplication: if a live session already connects to the same host:port, reject creation and ask the
    /// caller to reuse it or kill it first. Two jdb clients on the same JDWP port interfere with each other
    /// because kill sends resume and can unfreeze the other client's breakpoint.
    pub fn create_attach(&self, params: AttachParams) -> Result<DebugSession> {
        // Normalize the host so deduplication compares consistently (localhost → 127.0.0.1).
        let norm_host = crate::jdb::process::normalize_attach_host(&params.host);
        let target = format!("{}:{}", norm_host, params.port);

        // Deduplication: reject connections to the same target as an existing live session.
        {
            let map = self.sessions.lock().expect("sessions mutex poisoned");
            for s in map.values() {
                if s.target() == target && !matches!(s.state(), RunState::Dead) {
                    return Err(Error::DuplicateTarget {
                        target,
                        existing_id: s.id().to_string(),
                    });
                }
            }
        }

        let id = gen_session_id();
        let session = match params.backend {
            BackendKind::Jdb => {
                let jdb_path = match params.jdb_path {
                    Some(ref p) => {
                        let path = PathBuf::from(p);
                        jdkpath::find_jdb(Some(&path))?
                    }
                    None => jdkpath::find_jdb(None)?,
                };
                let config = AttachConfig {
                    host: params.host,
                    port: params.port,
                    sourcepath: params.sourcepath.into_iter().map(PathBuf::from).collect(),
                };
                DebugSession::Jdb(Arc::new(Session::attach(
                    &jdb_path,
                    &config,
                    id.clone(),
                    params.name,
                )?))
            }
            BackendKind::Jdi => DebugSession::Jdi(Arc::new(JdiSession::attach(
                &params.host,
                params.port,
                &params.sourcepath,
                id.clone(),
                params.name,
            )?)),
        };

        let mut map = self.sessions.lock().expect("sessions mutex poisoned");
        map.insert(id, session.clone());
        drop(map);

        self.persist_sessions();
        Ok(session)
    }

    /// Find a session by explicit id or by default unique live session.
    pub fn get(&self, session_id: Option<&str>) -> Result<DebugSession> {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        match session_id {
            Some(id) => map
                .get(id)
                .cloned()
                .ok_or_else(|| Error::SessionNotFound(id.to_string())),
            None => {
                // Default session: return it only when exactly one live session exists.
                let alive: Vec<_> = map
                    .values()
                    .filter(|s| !matches!(s.state(), RunState::Dead))
                    .collect();
                match alive.len() {
                    0 => Err(Error::SessionNotFound("no active sessions".into())),
                    1 => Ok((*alive[0]).clone()),
                    n => Err(Error::SessionNotFound(format!(
                        "{n} sessions active; specify --session <id>"
                    ))),
                }
            }
        }
    }

    /// List all sessions.
    pub fn list(&self) -> CommandResult {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        let sessions: Vec<SessionInfo> = map
            .values()
            .map(|s| SessionInfo {
                id: s.id().to_string(),
                name: s.name().map(str::to_string),
                mode: s.mode(),
                backend: s.backend(),
                target: s.target().to_string(),
                state: s.state(),
                jdb_pid: s.jdb_pid(),
                created_at: s.created_at().map(str::to_string),
            })
            .collect();
        CommandResult::SessionList { sessions }
    }

    /// Kill and remove one session.
    pub fn kill(&self, session_id: &str) -> Result<()> {
        let session = {
            let mut map = self.sessions.lock().expect("sessions mutex poisoned");
            map.remove(session_id)
                .ok_or_else(|| Error::SessionNotFound(session_id.into()))?
        };
        session.kill()?;
        self.persist_sessions();
        Ok(())
    }

    /// Daemon shutdown: kill all sessions.
    pub fn shutdown(&self) {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        for session in map.values() {
            let _ = session.kill();
        }
        self.registry.remove_daemon();
    }

    /// Persist the current session snapshot to sessions.json.
    fn persist_sessions(&self) {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        let records: Vec<SessionRecord> = map
            .values()
            .map(|s| SessionRecord {
                id: s.id().to_string(),
                name: s.name().map(str::to_string),
                mode: format!("{:?}", s.mode()).to_lowercase(),
                backend: format!("{:?}", s.backend()).to_lowercase(),
                target: s.target().to_string(),
                state: format!("{:?}", s.state()).to_lowercase(),
                jdb_pid: s.jdb_pid(),
                created_at: s.created_at().map(str::to_string),
            })
            .collect();
        let _ = self.registry.write_sessions(&records);
    }
}

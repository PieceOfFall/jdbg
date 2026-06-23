//! SessionManager：持有所有活跃会话，提供创建/查找/列表/删除。
//!
//! 内部用 `Mutex<HashMap<SessionId, Arc<Session>>>`——daemon accept loop 的多线程安全。
//! daemon 是单写者，同时把快照写入 sessions.json 供离线查看。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::jdb::process::{AttachConfig, LaunchConfig};
use crate::jdkpath;
use crate::protocol::{CommandResult, RunState, SessionInfo};
use crate::registry::{Registry, SessionRecord};
use crate::session::Session;

/// 生成 8 字符随机 session id。
fn gen_session_id() -> String {
    use rand::Rng;
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(8)
        .map(|b| (b as char).to_ascii_lowercase())
        .collect()
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    registry: Registry,
}

/// `create_launch` 的参数包。
pub struct LaunchParams {
    pub main_class: String,
    pub classpath: Vec<String>,
    pub sourcepath: Vec<String>,
    pub app_args: Vec<String>,
    pub jdb_args: Vec<String>,
    pub name: Option<String>,
    pub jdb_path: Option<String>,
}

/// `create_attach` 的参数包。
pub struct AttachParams {
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

    /// 创建 launch 会话。
    pub fn create_launch(&self, params: LaunchParams) -> Result<Arc<Session>> {
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
        let session = Arc::new(session);

        let mut map = self.sessions.lock().expect("sessions mutex poisoned");
        map.insert(id, Arc::clone(&session));
        drop(map);

        self.persist_sessions();
        Ok(session)
    }

    /// 创建 attach 会话（连接已运行 JVM 的 JDWP 端口）。
    pub fn create_attach(&self, params: AttachParams) -> Result<Arc<Session>> {
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

        let id = gen_session_id();
        let session = Session::attach(&jdb_path, &config, id.clone(), params.name)?;
        let session = Arc::new(session);

        let mut map = self.sessions.lock().expect("sessions mutex poisoned");
        map.insert(id, Arc::clone(&session));
        drop(map);

        self.persist_sessions();
        Ok(session)
    }

    /// 查找会话：指定 id 或默认（唯一存活会话）。
    pub fn get(&self, session_id: Option<&str>) -> Result<Arc<Session>> {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        match session_id {
            Some(id) => map
                .get(id)
                .cloned()
                .ok_or_else(|| Error::SessionNotFound(id.to_string())),
            None => {
                // 默认会话：如果恰好只有一个存活会话则返回它。
                let alive: Vec<_> = map.values()
                    .filter(|s| !matches!(s.state(), RunState::Dead))
                    .collect();
                match alive.len() {
                    0 => Err(Error::SessionNotFound("no active sessions".into())),
                    1 => Ok(Arc::clone(alive[0])),
                    n => Err(Error::SessionNotFound(format!(
                        "{n} sessions active; specify --session <id>"
                    ))),
                }
            }
        }
    }

    /// 列出所有会话。
    pub fn list(&self) -> CommandResult {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        let sessions: Vec<SessionInfo> = map
            .values()
            .map(|s| SessionInfo {
                id: s.meta.id.clone(),
                name: s.meta.name.clone(),
                mode: s.meta.mode,
                target: s.meta.target.clone(),
                state: s.state(),
                jdb_pid: Some(s.meta.jdb_pid),
                created_at: s.meta.created_at.clone(),
            })
            .collect();
        CommandResult::SessionList { sessions }
    }

    /// 终止并移除一个会话。
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

    /// daemon 关闭：杀掉所有会话。
    pub fn shutdown(&self) {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        for session in map.values() {
            let _ = session.kill();
        }
    }

    /// 把当前会话快照持久化到 sessions.json。
    fn persist_sessions(&self) {
        let map = self.sessions.lock().expect("sessions mutex poisoned");
        let records: Vec<SessionRecord> = map
            .values()
            .map(|s| SessionRecord {
                id: s.meta.id.clone(),
                name: s.meta.name.clone(),
                mode: format!("{:?}", s.meta.mode).to_lowercase(),
                target: s.meta.target.clone(),
                state: format!("{:?}", s.state()).to_lowercase(),
                jdb_pid: Some(s.meta.jdb_pid),
                created_at: s.meta.created_at.clone(),
            })
            .collect();
        let _ = self.registry.write_sessions(&records);
    }
}

//! Backend-neutral session handle used by the daemon.

use std::sync::Arc;

use crate::error::Result;
use crate::jdi::session::JdiSession;
use crate::protocol::{BackendKind, CommandResult, RunState, SessionMode};
use crate::session::Session;

#[derive(Clone)]
pub enum DebugSession {
    Jdb(Arc<Session>),
    Jdi(Arc<JdiSession>),
}

impl DebugSession {
    pub fn id(&self) -> &str {
        match self {
            Self::Jdb(session) => &session.meta.id,
            Self::Jdi(session) => &session.meta.id,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Jdb(session) => session.meta.name.as_deref(),
            Self::Jdi(session) => session.meta.name.as_deref(),
        }
    }

    pub fn mode(&self) -> SessionMode {
        match self {
            Self::Jdb(session) => session.meta.mode,
            Self::Jdi(session) => session.meta.mode,
        }
    }

    pub fn backend(&self) -> BackendKind {
        match self {
            Self::Jdb(session) => session.meta.backend,
            Self::Jdi(session) => session.meta.backend,
        }
    }

    pub fn target(&self) -> &str {
        match self {
            Self::Jdb(session) => &session.meta.target,
            Self::Jdi(session) => &session.meta.target,
        }
    }

    pub fn created_at(&self) -> Option<&str> {
        match self {
            Self::Jdb(session) => session.meta.created_at.as_deref(),
            Self::Jdi(session) => session.meta.created_at.as_deref(),
        }
    }

    pub fn jdb_pid(&self) -> Option<u32> {
        match self {
            Self::Jdb(session) => Some(session.meta.jdb_pid),
            Self::Jdi(_) => None,
        }
    }

    pub fn state(&self) -> RunState {
        match self {
            Self::Jdb(session) => session.state(),
            Self::Jdi(session) => session.state(),
        }
    }

    pub fn status(&self) -> CommandResult {
        match self {
            Self::Jdb(session) => session.status(),
            Self::Jdi(session) => session.status(),
        }
    }

    pub fn kill(&self) -> Result<()> {
        match self {
            Self::Jdb(session) => session.kill(),
            Self::Jdi(session) => session.kill(),
        }
    }

    pub fn as_jdb(&self) -> Option<Arc<Session>> {
        match self {
            Self::Jdb(session) => Some(Arc::clone(session)),
            Self::Jdi(_) => None,
        }
    }

    pub fn as_jdi(&self) -> Option<Arc<JdiSession>> {
        match self {
            Self::Jdb(_) => None,
            Self::Jdi(session) => Some(Arc::clone(session)),
        }
    }
}

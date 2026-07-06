//! Domain error types. `thiserror` defines structured errors, while upper layers add context with `anyhow`.
//!
//! Note: command timeouts are not engine errors. They are non-destructive results returned by `reader`
//! (see §5: do not kill on timeout; return partial output and mark `Running`). `Error::Timeout`
//! is only for degraded cases where reading cannot continue; normal timeouts use `CommandResult::Timeout`.

/// Crate-wide `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The jdb executable could not be found through any discovery path.
    #[error(
        "jdb executable not found (searched: {searched:?}). \
         Install a JDK, set JAVA_HOME, or pass --jdb-path"
    )]
    JdbNotFound { searched: Vec<String> },

    /// Failed to spawn the jdb child process.
    #[error("failed to spawn jdb at {path}: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The jdb child process has exited, or its stdin/stdout pipe is closed.
    #[error("jdb session is not alive: {0}")]
    SessionDead(String),

    /// The specified or default session was not found.
    #[error("session not found: {0}")]
    SessionNotFound(String),

    /// Attempted to attach to the same target (host:port) as an existing live session.
    #[error(
        "a live session '{existing_id}' is already attached to {target}. \
         Reuse it (--session {existing_id}) or kill it first."
    )]
    DuplicateTarget { target: String, existing_id: String },

    /// A backend was selected before its session creation path is available.
    #[error("backend '{backend}' is not supported for {operation} yet")]
    UnsupportedBackend { backend: String, operation: String },

    /// jdb reported a connection or launch error (§5: `Unable to attach`, `java.io.IOException`, `Input stream closed`).
    #[error("jdb connection/launch failed: {0}")]
    Connection(String),

    /// A JDI-backend error surfaced by the sidecar: either a semantic error the
    /// sidecar returned (e.g. `method_not_found`, `source_not_found`,
    /// `method_invocation_not_allowed`) or a sidecar transport/handshake failure.
    /// Kept distinct from [`Error::Connection`] so JDI messages are not mislabeled
    /// "jdb connection/launch failed"; the payload is already self-describing.
    #[error("{0}")]
    Jdi(String),

    /// The JDI backend cannot be started in this local installation/environment.
    /// This is intentionally narrower than all JDI errors: implicit default-JDI
    /// session creation may fall back to jdb for these cases only.
    #[error("{0}")]
    JdiUnavailable(String),

    /// Reading jdb output timed out completely and could not recover.
    #[error("timed out after {secs}s waiting for jdb")]
    Timeout { secs: u64 },

    /// Any other IO error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Map the error to a process exit code for the CLI.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::JdbNotFound { .. } => 3,
            Error::Spawn { .. } => 4,
            Error::SessionDead(_) => 5,
            Error::SessionNotFound(_) => 5,
            Error::DuplicateTarget { .. } => 5,
            Error::UnsupportedBackend { .. } => 5,
            Error::Connection(_) => 6,
            Error::Jdi(_) => 6,
            Error::JdiUnavailable(_) => 6,
            Error::Timeout { .. } => 7,
            Error::Io(_) => 1,
        }
    }

    pub fn is_jdi_unavailable(&self) -> bool {
        matches!(self, Error::JdiUnavailable(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdi_error_is_not_mislabeled_as_jdb_connection() {
        // A JDI sidecar semantic error must render as its own self-describing message,
        // not "jdb connection/launch failed: …" (it is neither jdb nor a connection fault).
        let err = Error::Jdi("JDI sidecar error method_not_found: no such method".into());
        let rendered = err.to_string();
        assert_eq!(
            rendered,
            "JDI sidecar error method_not_found: no such method"
        );
        assert!(!rendered.contains("jdb connection/launch failed"));
        assert_eq!(err.exit_code(), 6);
    }

    #[test]
    fn jdb_connection_error_keeps_its_prefix() {
        assert_eq!(
            Error::Connection("Unable to attach".into()).to_string(),
            "jdb connection/launch failed: Unable to attach"
        );
    }
}

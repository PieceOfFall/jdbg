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
            Error::Timeout { .. } => 7,
            Error::Io(_) => 1,
        }
    }
}

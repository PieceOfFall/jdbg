//! 领域错误类型。`thiserror` 定义结构化错误，上层用 `anyhow` 加 context。
//!
//! 注意：命令「超时」在引擎内部不是错误，而是 `reader` 返回的一种非破坏性结果
//! （见 §5：超时不杀进程，返回部分输出并标记 `Running`）。这里的 `Error::Timeout`
//! 仅用于无法继续读取的退化场景，正常超时走 `CommandResult::Timeout`。

/// crate 统一的 `Result` 别名。
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 在所有发现路径上都找不到 jdb 可执行文件。
    #[error(
        "jdb executable not found (searched: {searched:?}). \
         Install a JDK, set JAVA_HOME, or pass --jdb-path"
    )]
    JdbNotFound { searched: Vec<String> },

    /// 启动 jdb 子进程失败。
    #[error("failed to spawn jdb at {path}: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// jdb 子进程已退出 / stdin 或 stdout 管道关闭。
    #[error("jdb session is not alive: {0}")]
    SessionDead(String),

    /// 找不到指定（或默认）会话。
    #[error("session not found: {0}")]
    SessionNotFound(String),

    /// jdb 报告连接 / 启动错误（§5：`Unable to attach`、`java.io.IOException`、`Input stream closed`）。
    #[error("jdb connection/launch failed: {0}")]
    Connection(String),

    /// 读取 jdb 输出时彻底超时且无法恢复（退化场景）。
    #[error("timed out after {secs}s waiting for jdb")]
    Timeout { secs: u64 },

    /// 其它 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// 映射到进程退出码（后续 CLI 阶段使用）。
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::JdbNotFound { .. } => 3,
            Error::Spawn { .. } => 4,
            Error::SessionDead(_) => 5,
            Error::SessionNotFound(_) => 5,
            Error::Connection(_) => 6,
            Error::Timeout { .. } => 7,
            Error::Io(_) => 1,
        }
    }
}

//! IPC Wire 类型——CLI↔Daemon 的 JSONL 协议（§4）。

use serde::{Deserialize, Serialize};

use super::result::CommandResponse;

// ─── Request ────────────────────────────────────────────────────────────────────

/// CLI→Daemon 的请求（一个连接一条）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// 协议版本。
    pub v: u32,
    /// 唯一请求 id（用于日志关联）。
    pub id: String,
    /// 目标会话 id（None = 默认唯一会话）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 本命令超时（秒），覆盖默认值；None 用默认。对应 CLI 的 `--timeout`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// 具体命令。
    pub cmd: Command,
}

/// Daemon→CLI 的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: String,
    pub ok: bool,
    /// 成功时有 result。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CommandResponse>,
    /// 失败时有 error。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

/// Wire 层错误描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: i32,
    pub message: String,
}

// ─── Command ────────────────────────────────────────────────────────────────────

// ─── Command ────────────────────────────────────────────────────────────────────

/// 命令枚举——镜像 §7 CLI 子命令，internally-tagged。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    // ── Session lifecycle ──
    Launch {
        main_class: String,
        #[serde(default)]
        classpath: Vec<String>,
        #[serde(default)]
        sourcepath: Vec<String>,
        #[serde(default)]
        app_args: Vec<String>,
        #[serde(default)]
        jdb_args: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        jdb_path: Option<String>,
    },
    Attach {
        #[serde(default = "default_host")]
        host: String,
        #[serde(default = "default_port")]
        port: u16,
        #[serde(default)]
        sourcepath: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        jdb_path: Option<String>,
    },
    Status,
    List,
    Kill,

    // ── Breakpoints ──
    BreakAt {
        class: String,
        line: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
    BreakIn {
        class: String,
        method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
    Catch {
        exception: String,
        #[serde(default = "default_catch_mode")]
        mode: String,
    },
    Breakpoints,
    Clear { spec: String },

    // ── Execution control ──
    Run,
    Cont,
    Step,
    Next,
    StepOut,

    // ── Inspection ──
    Where { #[serde(default)] all: bool },
    Locals,
    Print { expr: String },
    Dump { expr: String },
    Eval { expr: String },
    Threads,
    Thread { id: String },
    Frame { direction: String, #[serde(default = "default_one")] n: u32 },
    ListSource { #[serde(skip_serializing_if = "Option::is_none")] line: Option<u32> },
    Inspect { expr: String, #[serde(default = "default_max_elements")] max_elements: u32 },
    Raw { command: String },

    // ── Daemon control ──
    DaemonStatus,
    DaemonStop,
}

fn default_host() -> String { "localhost".into() }
fn default_port() -> u16 { 5005 }
fn default_catch_mode() -> String { "all".into() }
fn default_one() -> u32 { 1 }
fn default_max_elements() -> u32 { 10 }

// ─── impl ───────────────────────────────────────────────────────────────────────

impl Request {
    /// 构造一条请求。
    pub fn new(cmd: Command, session: Option<String>) -> Self {
        use rand::Rng;
        let id: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        Self { v: 1, id, session, timeout: None, cmd }
    }

    /// 设置超时（秒）。
    pub fn with_timeout(mut self, timeout: Option<u64>) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Response {
    /// 构造成功响应。
    pub fn ok(id: &str, result: CommandResponse) -> Self {
        Self { v: 1, id: id.to_string(), ok: true, result: Some(result), error: None }
    }

    /// 构造失败响应。
    pub fn err(id: &str, code: i32, message: impl Into<String>) -> Self {
        Self {
            v: 1,
            id: id.to_string(),
            ok: false,
            result: None,
            error: Some(WireError { code, message: message.into() }),
        }
    }
}

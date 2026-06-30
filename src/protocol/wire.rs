//! IPC wire types for the CLI↔Daemon JSONL protocol (§4).

use serde::{Deserialize, Serialize};

use super::result::CommandResponse;

// ─── Request ────────────────────────────────────────────────────────────────────

/// CLI→Daemon request, one request per connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version.
    pub v: u32,
    /// Unique request id for log correlation.
    pub id: String,
    /// Target session id. None means the default unique session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Command timeout in seconds, overriding the default. None uses the default. Mirrors CLI `--timeout`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// The concrete command.
    pub cmd: Command,
}

/// Daemon→CLI response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: String,
    pub ok: bool,
    /// Present on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CommandResponse>,
    /// Present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

/// Wire-layer error description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: i32,
    pub message: String,
}

// ─── Command ────────────────────────────────────────────────────────────────────

// ─── Command ────────────────────────────────────────────────────────────────────

/// Command enum mirroring §7 CLI subcommands, serialized as internally tagged JSON.
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
        #[serde(skip_serializing_if = "Option::is_none")]
        suspend: Option<String>,
    },
    BreakIn {
        class: String,
        method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        suspend: Option<String>,
    },
    Catch {
        exception: String,
        #[serde(default = "default_catch_mode")]
        mode: String,
    },
    Watch {
        field: String,
        #[serde(default = "default_watch_mode")]
        mode: String,
    },
    Unwatch {
        field: String,
        #[serde(default = "default_watch_mode")]
        mode: String,
    },
    Breakpoints,
    Clear {
        spec: String,
    },

    // ── Execution control ──
    Run,
    Cont,
    Step,
    Next,
    StepOut,

    // ── Class/method search ──
    Classes {
        #[serde(skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    Methods {
        class: String,
    },

    // ── Inspection ──
    Where {
        #[serde(default)]
        all: bool,
    },
    Locals,
    Print {
        expr: String,
    },
    Dump {
        expr: String,
    },
    Eval {
        expr: String,
    },
    Threads {
        #[serde(skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
    },
    Thread {
        id: String,
    },
    Frame {
        direction: String,
        #[serde(default = "default_one")]
        n: u32,
    },
    ListSource {
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
    },
    Inspect {
        expr: String,
        #[serde(default = "default_max_elements")]
        max_elements: u32,
    },
    Raw {
        command: String,
    },

    // ── Thread control / state mutation / locks ──
    Suspend {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    Resume {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    Set {
        lvalue: String,
        value: String,
    },
    Ignore {
        exception: String,
        #[serde(default = "default_catch_mode")]
        mode: String,
    },
    Lock {
        expr: String,
    },
    ThreadLocks {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    // ── Daemon control ──
    DaemonStatus,
    DaemonStop,
}

fn default_host() -> String {
    "localhost".into()
}
fn default_port() -> u16 {
    5005
}
fn default_catch_mode() -> String {
    "all".into()
}
fn default_watch_mode() -> String {
    "modification".into()
}
fn default_one() -> u32 {
    1
}
fn default_max_elements() -> u32 {
    10
}

// ─── impl ───────────────────────────────────────────────────────────────────────

impl Request {
    /// Build a request.
    pub fn new(cmd: Command, session: Option<String>) -> Self {
        use rand::Rng;
        let id: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        Self {
            v: 1,
            id,
            session,
            timeout: None,
            cmd,
        }
    }

    /// Set the timeout in seconds.
    pub fn with_timeout(mut self, timeout: Option<u64>) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Response {
    /// Build a success response.
    pub fn ok(id: &str, result: CommandResponse) -> Self {
        Self {
            v: 1,
            id: id.to_string(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Build a failure response.
    pub fn err(id: &str, code: i32, message: impl Into<String>) -> Self {
        Self {
            v: 1,
            id: id.to_string(),
            ok: false,
            result: None,
            error: Some(WireError {
                code,
                message: message.into(),
            }),
        }
    }
}

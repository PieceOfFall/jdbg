//! Output schema types: parser outputs and CLI/MCP rendering inputs.

use serde::{Deserialize, Serialize};

// ─── Basic Structures ──────────────────────────────────────────────────────────

/// A source/code location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub class: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub line: u32,
}

/// A stack frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrame {
    pub index: u32,
    pub location: Location,
    #[serde(default)]
    pub is_native: bool,
}

/// The call stack for one thread, grouped from `where all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStack {
    pub thread: String,
    pub frames: Vec<StackFrame>,
}

/// A local variable or object field binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarBinding {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    pub value: String,
}

/// Thread information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    /// Hex thread id reported by jdb, such as "0x1a3".
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub state: String,
}

/// A source line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLine {
    pub number: u32,
    pub text: String,
}

// ─── Run State & Events ────────────────────────────────────────────────────────

/// Session run-state machine (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// After launch and before `run`.
    Loaded,
    /// Suspended at a breakpoint, step, or exception.
    Suspended,
    /// The application is running and has not stopped after `cont`/`run`.
    Running,
    /// The application exited normally.
    Exited,
    /// The jdb child process exited unexpectedly or the pipe broke.
    Dead,
}

/// Event types recognized by the reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Breakpoint {
        location: Location,
        thread: String,
    },
    MethodEntry {
        location: Location,
        thread: String,
    },
    MethodExit {
        location: Location,
        thread: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        return_value: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        return_type: Option<String>,
    },
    Step {
        location: Location,
        thread: String,
    },
    Exception {
        exception: String,
        caught: bool,
        location: Option<Location>,
        thread: String,
    },
    FieldWatch {
        field: String,
        access_type: String,
        thread: String,
    },
    VmExit,
}

// ─── CommandResult ──────────────────────────────────────────────────────────────

// %%PLACEHOLDER_RESULT_CMDRESULT%%

/// Command execution result (§8), serialized as internally tagged JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResult {
    // ── session lifecycle ──
    SessionCreated {
        session: String,
        mode: SessionMode,
        backend: BackendKind,
        target: String,
        state: RunState,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    Status {
        session: String,
        backend: BackendKind,
        state: RunState,
        last_event: Option<Event>,
        jdb_alive: bool,
    },

    // ── breakpoints ──
    BreakpointSet {
        spec: String,
        #[serde(rename = "type")]
        bp_kind: BreakpointKind,
        deferred: bool,
    },
    BreakpointList {
        breakpoints: Vec<String>,
    },

    // ── execution outcomes ──
    Stopped {
        event: Event,
        location: Location,
        thread: String,
        /// jdb id of the hit thread, such as "0x1a3" or decimal "18315"; can be passed directly to `thread`.
        /// Filled by enrichment through a reverse lookup in `threads`; None means lookup failed and a WARNING note was added.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        thread_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        frame: Option<StackFrame>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_context: Option<Vec<SourceLine>>,
    },
    ExceptionCaught {
        exception: String,
        caught: bool,
        location: Location,
        thread: String,
        /// jdb id of the hit thread, same semantics as `Stopped.thread_id`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        thread_id: Option<String>,
    },
    VmExited {
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tail: Option<String>,
    },
    Timeout {
        partial_output: String,
        state: RunState,
    },

    // ── inspection outcomes ──
    StackTrace {
        frames: Vec<StackFrame>,
    },
    /// Multi-thread stack trace from `where all`, grouped by thread.
    ThreadStackTrace {
        threads: Vec<ThreadStack>,
    },
    Locals {
        vars: Vec<VarBinding>,
    },
    Value {
        expr: String,
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ty: Option<String>,
    },
    ObjectDump {
        expr: String,
        fields: Vec<VarBinding>,
    },
    Threads {
        threads: Vec<ThreadInfo>,
    },
    Source {
        around_line: u32,
        lines: Vec<SourceLine>,
    },
    /// `inspect` result: collection/array size plus the first N elements.
    Inspection {
        expr: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
        elements: Vec<VarBinding>,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
    },

    // ── class/method search ──
    Classes {
        classes: Vec<String>,
    },
    Methods {
        class: String,
        methods: Vec<String>,
    },

    // ── field watchpoint ──
    WatchSet {
        spec: String,
        mode: String,
        deferred: bool,
    },

    // ── fallback ──
    Raw {
        text: String,
    },
}

// ─── Helper Enums ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Launch,
    Attach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Jdb,
    Jdi,
}

impl Default for BackendKind {
    fn default() -> Self {
        Self::Jdi
    }
}

impl std::str::FromStr for BackendKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "jdb" => Ok(Self::Jdb),
            "jdi" => Ok(Self::Jdi),
            other => Err(format!(
                "unsupported backend '{other}' (expected 'jdb' or 'jdi')"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodEventKind {
    Entry,
    Exit,
    Both,
}

impl Default for MethodEventKind {
    fn default() -> Self {
        Self::Entry
    }
}

impl std::str::FromStr for MethodEventKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "entry" => Ok(Self::Entry),
            "exit" => Ok(Self::Exit),
            "both" => Ok(Self::Both),
            other => Err(format!(
                "unsupported method event '{other}' (expected 'entry', 'exit', or 'both')"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakpointKind {
    Line,
    Method,
    Catch,
}

/// One entry in the session list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: SessionMode,
    pub backend: BackendKind,
    pub target: String,
    pub state: RunState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jdb_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

// ─── Full Response with Side Bands ─────────────────────────────────────────────

/// Full command response, including side bands such as stderr output and notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResponse {
    #[serde(flatten)]
    pub result: CommandResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::BackendKind;

    #[test]
    fn backend_default_is_jdi() {
        assert_eq!(BackendKind::default(), BackendKind::Jdi);
    }
}

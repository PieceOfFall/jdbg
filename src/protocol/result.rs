//! 输出 schema 类型——解析器的产出物、CLI/MCP 的渲染输入。

use serde::{Deserialize, Serialize};

// ─── 基础结构 ───────────────────────────────────────────────────────────────────

/// 代码位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub class: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub line: u32,
}

/// 栈帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrame {
    pub index: u32,
    pub location: Location,
    #[serde(default)]
    pub is_native: bool,
}

/// 单个线程的调用栈（`where all` 按线程分组）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStack {
    pub thread: String,
    pub frames: Vec<StackFrame>,
}

/// 局部变量 / 对象字段绑定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarBinding {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    pub value: String,
}

/// 线程信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    /// jdb 给出的十六进制 id（如 "0x1a3"）。
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub state: String,
}

/// 源代码行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLine {
    pub number: u32,
    pub text: String,
}

// ─── 运行状态 & 事件 ────────────────────────────────────────────────────────────

/// Session 的运行状态机（§5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// launch 后、`run` 前。
    Loaded,
    /// 停在断点 / step / exception。
    Suspended,
    /// 应用正在执行中（`cont`/`run` 后尚未停下）。
    Running,
    /// 应用正常退出。
    Exited,
    /// jdb 子进程意外退出 / 管道断裂。
    Dead,
}

/// reader 识别到的事件类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Breakpoint { location: Location, thread: String },
    Step { location: Location, thread: String },
    Exception { exception: String, caught: bool, location: Option<Location>, thread: String },
    FieldWatch { field: String, access_type: String, thread: String },
    VmExit,
}

// ─── CommandResult ──────────────────────────────────────────────────────────────

// %%PLACEHOLDER_RESULT_CMDRESULT%%

/// 命令执行结果（§8）。internally-tagged JSON。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResult {
    // ── session lifecycle ──
    SessionCreated {
        session: String,
        mode: SessionMode,
        target: String,
        state: RunState,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    Status {
        session: String,
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
        /// 命中线程的 jdb id（如 "0x1a3" 或十进制 "18315"）——可直接传给 `thread` 工具切换。
        /// 命中后由 enrichment 反查 `threads` 回填；查不到则为 None（附 WARNING note）。
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
        /// 命中线程的 jdb id（同 `Stopped.thread_id`）。
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
    /// `where all` 的多线程栈，按线程分组。
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
    /// `inspect` 结果：集合/数组的 size + 前 N 个元素。
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

// ─── 辅助 enum ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Launch,
    Attach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakpointKind {
    Line,
    Method,
    Catch,
}

/// session list 里每条信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: SessionMode,
    pub target: String,
    pub state: RunState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jdb_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

// ─── 带 side bands 的完整响应 ────────────────────────────────────────────────────

/// 完整的命令响应，包含 side bands（stderr 输出、note 提示）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResponse {
    #[serde(flatten)]
    pub result: CommandResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

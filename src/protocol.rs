//! Wire / output 类型——解析器的产出物、CLI 的渲染输入。
//!
//! 对应 CLAUDE.md §8。本阶段先实现解析器需要的变体；IPC 的 Request/Response 留到 daemon 阶段。

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
    VmExit,
}

// ─── CommandResult ──────────────────────────────────────────────────────────────

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
        #[serde(skip_serializing_if = "Option::is_none")]
        frame: Option<StackFrame>,
    },
    ExceptionCaught {
        exception: String,
        caught: bool,
        location: Location,
        thread: String,
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

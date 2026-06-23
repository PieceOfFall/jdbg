//! 调试会话：绑定 jdb 子进程与 reader/stderr 线程，驱动 RunState 状态机。
//!
//! `Session` 是引擎的核心协调层（roadmap 3）：
//! - 拥有 [`JdbProcess`] + [`PromptReader`] + stderr drain 线程（§5 三线程模型）。
//! - 每会话单命令锁（内部 `Mutex`）：同一时刻只有一条命令在飞。
//! - [`Session::execute`] 把 `ReadOutcome` + parser → [`CommandResponse`]
//!   （event → `Stopped` / `ExceptionCaught`，VM 退出 → `VmExited`，超时 → `Timeout`）。
//! - 语义便捷方法（[`Session::run`]、[`Session::stop_at`]…）封装 jdb 命令字符串，
//!   使上层（CLI）无需了解 jdb 语法——低耦合。
//!
//! `Session` 通过内部可变性（`Mutex<SessionInner>`）做到 `&self` 即可执行命令，
//! 便于以 `Arc<Session>` 在 daemon 的多线程间共享。

use std::process::ChildStderr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::jdb::parser::{CommandHint, classify_output};
use crate::jdb::process::{JdbProcess, LaunchConfig, spawn_launch};
use crate::jdb::reader::{DetectedEvent, PromptReader, ReadMode, ReadOutcome};
use crate::protocol::*;

/// 普通命令默认超时。
const TIMEOUT_NORMAL: Duration = Duration::from_secs(15);
/// 阻塞命令（run/cont/step…）默认超时。
const TIMEOUT_BLOCKING: Duration = Duration::from_secs(30);

// ─── 会话元信息（不变）──────────────────────────────────────────────────────────

/// 会话的不变元信息。
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: Option<String>,
    pub mode: SessionMode,
    pub target: String,
    pub jdb_pid: u32,
    /// 创建时间戳（后续阶段接 `jiff`，本阶段留 None）。
    pub created_at: Option<String>,
}

// ─── 命令执行特性 ────────────────────────────────────────────────────────────────

/// 描述一条 jdb 命令如何执行：读取模式、解析提示、超时。
#[derive(Debug, Clone, Copy)]
pub struct CommandKind {
    pub mode: ReadMode,
    pub hint: CommandHint,
    pub timeout: Duration,
}

impl CommandKind {
    /// 普通命令（locals/where/print…）：任何 prompt 即完成，小超时。
    pub fn normal(hint: CommandHint) -> Self {
        Self { mode: ReadMode::Normal, hint, timeout: TIMEOUT_NORMAL }
    }

    /// 阻塞命令（run/cont/step/next/step-out）：等事件 / thread-prompt / VM 退出，大超时。
    pub fn blocking(hint: CommandHint) -> Self {
        Self { mode: ReadMode::Blocking, hint, timeout: TIMEOUT_BLOCKING }
    }

    /// 覆盖超时（对应 CLI 的 `--timeout`）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

// ─── Session ────────────────────────────────────────────────────────────────────

/// 一个后台调试会话：一个 jdb 子进程 + 一个被调试的 JVM。
pub struct Session {
    pub meta: SessionMeta,
    inner: Mutex<SessionInner>,
    /// stderr drain 线程累积的内容（每次 execute 后取出作为 side band）。
    stderr: Arc<Mutex<String>>,
    _stderr_handle: JoinHandle<()>,
}

/// 受命令锁保护的可变状态。
struct SessionInner {
    process: JdbProcess,
    reader: PromptReader,
    state: RunState,
    last_event: Option<Event>,
}

impl Session {
    /// 以 launch 模式启动会话：spawn jdb、起读取线程、读掉初始 prompt，状态 `Loaded`。
    pub fn launch(
        jdb_path: &std::path::Path,
        config: &LaunchConfig,
        id: String,
        name: Option<String>,
    ) -> Result<Session> {
        let spawned = spawn_launch(jdb_path, config)?;
        let jdb_pid = spawned.process.pid();
        let mut reader = PromptReader::new(spawned.stdout);
        let (stderr, stderr_handle) = spawn_stderr_drain(spawned.stderr);

        // launch 后 jdb 立刻输出一个初始 `> ` prompt（VM 尚未启动）。
        let _ = reader.read_until_prompt(TIMEOUT_NORMAL, ReadMode::Normal);

        let inner = SessionInner {
            process: spawned.process,
            reader,
            state: RunState::Loaded,
            last_event: None,
        };
        let meta = SessionMeta {
            id,
            name,
            mode: SessionMode::Launch,
            target: config.main_class.clone(),
            jdb_pid,
            created_at: None,
        };
        Ok(Session {
            meta,
            inner: Mutex::new(inner),
            stderr,
            _stderr_handle: stderr_handle,
        })
    }

    /// 执行一条原始 jdb 命令并映射为结构化响应。
    ///
    /// 持有命令锁直到读到 prompt——保证每会话同时只有一条命令在飞（§5）。
    pub fn execute(&self, raw: &str, kind: CommandKind) -> Result<CommandResponse> {
        let mut inner = self.inner.lock().expect("session mutex poisoned");

        // 已终止的会话拒绝新命令。
        if matches!(inner.state, RunState::Exited | RunState::Dead) {
            return Err(Error::SessionDead(format!(
                "session {} is {:?}",
                self.meta.id, inner.state
            )));
        }

        inner.process.write_command(raw)?;
        let outcome = inner.reader.read_until_prompt(kind.timeout, kind.mode);

        // jdb 致命错误 → 失败（会话标记 Dead）。
        if let ReadOutcome::Fatal { message } = &outcome {
            inner.state = RunState::Dead;
            return Err(Error::Connection(message.clone()));
        }

        let stderr = self.take_stderr();
        Ok(inner.map_outcome(outcome, kind.hint, stderr))
    }

    /// 当前运行状态。
    pub fn state(&self) -> RunState {
        self.inner.lock().expect("session mutex poisoned").state
    }

    /// 会话状态报告（不向 jdb 发命令）。
    pub fn status(&self) -> CommandResult {
        let mut inner = self.inner.lock().expect("session mutex poisoned");
        let jdb_alive = inner.process.is_alive();
        CommandResult::Status {
            session: self.meta.id.clone(),
            state: inner.state,
            last_event: inner.last_event.clone(),
            jdb_alive,
        }
    }

    /// 结束会话：发 `quit`、杀掉 jdb、标记 `Dead`。
    pub fn kill(&self) -> Result<()> {
        let mut inner = self.inner.lock().expect("session mutex poisoned");
        let _ = inner.process.write_command("quit");
        inner.process.kill()?;
        inner.state = RunState::Dead;
        Ok(())
    }

    /// 取出 stderr drain 累积的内容并清空。
    fn take_stderr(&self) -> Option<String> {
        let mut s = self.stderr.lock().expect("stderr mutex poisoned");
        if s.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *s))
        }
    }

    // ── 语义便捷方法（封装 jdb 命令字符串，§7 命令面）──────────────────────────

    /// `stop at Class:line`
    pub fn stop_at(&self, class: &str, line: u32) -> Result<CommandResponse> {
        self.execute(&format!("stop at {class}:{line}"), CommandKind::normal(CommandHint::BreakpointSet))
    }

    /// `stop in Class.method`（可选签名以区分重载）
    pub fn stop_in(&self, class: &str, method: &str, args: Option<&str>) -> Result<CommandResponse> {
        let spec = match args {
            Some(a) => format!("stop in {class}.{method}({a})"),
            None => format!("stop in {class}.{method}"),
        };
        self.execute(&spec, CommandKind::normal(CommandHint::BreakpointSet))
    }

    /// `run`（仅 launch 模式）
    pub fn run(&self) -> Result<CommandResponse> {
        self.execute("run", CommandKind::blocking(CommandHint::Run))
    }

    /// `cont`
    pub fn cont(&self) -> Result<CommandResponse> {
        self.execute("cont", CommandKind::blocking(CommandHint::Cont))
    }

    /// `step`（step into）
    pub fn step(&self) -> Result<CommandResponse> {
        self.execute("step", CommandKind::blocking(CommandHint::Step))
    }

    /// `next`（step over）
    pub fn next(&self) -> Result<CommandResponse> {
        self.execute("next", CommandKind::blocking(CommandHint::Next))
    }

    /// `step up`（run until method returns）
    pub fn step_out(&self) -> Result<CommandResponse> {
        self.execute("step up", CommandKind::blocking(CommandHint::StepOut))
    }

    /// `where`
    pub fn stack(&self) -> Result<CommandResponse> {
        self.execute("where", CommandKind::normal(CommandHint::Where))
    }

    /// `locals`
    pub fn locals(&self) -> Result<CommandResponse> {
        self.execute("locals", CommandKind::normal(CommandHint::Locals))
    }

    /// `print <expr>`
    pub fn print(&self, expr: &str) -> Result<CommandResponse> {
        self.execute(&format!("print {expr}"), CommandKind::normal(CommandHint::Print))
    }

    /// `threads`
    pub fn threads(&self) -> Result<CommandResponse> {
        self.execute("threads", CommandKind::normal(CommandHint::Threads))
    }

    /// `list [line]`
    pub fn list_source(&self, line: Option<u32>) -> Result<CommandResponse> {
        let cmd = match line {
            Some(l) => format!("list {l}"),
            None => "list".to_string(),
        };
        self.execute(&cmd, CommandKind::normal(CommandHint::ListSource))
    }

    /// 透传任意 jdb 命令（escape hatch）。
    pub fn raw(&self, cmd: &str) -> Result<CommandResponse> {
        self.execute(cmd, CommandKind::normal(CommandHint::Other))
    }
}

impl SessionInner {
    /// 把 reader 的 `ReadOutcome` 映射为 `CommandResponse`，并推进状态机。
    fn map_outcome(
        &mut self,
        outcome: ReadOutcome,
        hint: CommandHint,
        stderr: Option<String>,
    ) -> CommandResponse {
        let (result, note) = match outcome {
            ReadOutcome::Prompt { output, event } => match event {
                // 事件（断点/单步/异常）→ Suspended，产出 Stopped/ExceptionCaught。
                Some(ev) => {
                    let (result, evt) = event_to_result(ev);
                    self.state = RunState::Suspended;
                    self.last_event = Some(evt);
                    (result, None)
                }
                // 普通命令 → 用 parser 分类，状态不变。
                None => classify_output(&output, hint),
            },
            ReadOutcome::VmExit { output } => {
                self.state = RunState::Exited;
                self.last_event = Some(Event::VmExit);
                (
                    CommandResult::VmExited { exit_code: None, tail: Some(output) },
                    None,
                )
            }
            ReadOutcome::Timeout { partial } => {
                // 应用可能死锁/长循环——非破坏性，保留会话存活并标 Running（§5）。
                self.state = RunState::Running;
                (
                    CommandResult::Timeout { partial_output: partial, state: RunState::Running },
                    None,
                )
            }
            ReadOutcome::Eof { output } => {
                self.state = RunState::Dead;
                (
                    CommandResult::VmExited { exit_code: None, tail: Some(output) },
                    None,
                )
            }
            // Fatal 已在 execute 中拦截转为 Err，这里兜底。
            ReadOutcome::Fatal { message } => {
                self.state = RunState::Dead;
                (CommandResult::Raw { text: message }, None)
            }
        };
        CommandResponse { result, stderr, note }
    }
}

/// 把 reader 的 `DetectedEvent` 转为 `CommandResult` + 记录用的 `Event`。
fn event_to_result(ev: DetectedEvent) -> (CommandResult, Event) {
    match ev {
        DetectedEvent::Breakpoint { thread, class, method, line } => {
            let loc = Location { class, method, file: None, line };
            let event = Event::Breakpoint { location: loc.clone(), thread: thread.clone() };
            (
                CommandResult::Stopped { event: event.clone(), location: loc, thread, frame: None },
                event,
            )
        }
        DetectedEvent::Step { thread, class, method, line } => {
            let loc = Location { class, method, file: None, line };
            let event = Event::Step { location: loc.clone(), thread: thread.clone() };
            (
                CommandResult::Stopped { event: event.clone(), location: loc, thread, frame: None },
                event,
            )
        }
        DetectedEvent::Exception { thread, exception, caught } => {
            // 异常 banner 不含位置；后续可由 `where` 补全。这里用空 location 占位。
            let loc = Location { class: String::new(), method: String::new(), file: None, line: 0 };
            let event = Event::Exception {
                exception: exception.clone(),
                caught,
                location: Some(loc.clone()),
                thread: thread.clone(),
            };
            (
                CommandResult::ExceptionCaught { exception, caught, location: loc, thread },
                event,
            )
        }
    }
}

/// 起一个后台线程把 jdb stderr 逐行 drain 到共享缓冲。
fn spawn_stderr_drain(stderr: ChildStderr) -> (Arc<Mutex<String>>, JoinHandle<()>) {
    use std::io::{BufRead, BufReader};
    let buf = Arc::new(Mutex::new(String::new()));
    let buf2 = Arc::clone(&buf);
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if let Ok(mut b) = buf2.lock() {
                        b.push_str(&line);
                    }
                }
                Err(_) => break,
            }
        }
    });
    (buf, handle)
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

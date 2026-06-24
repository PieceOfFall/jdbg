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
use crate::jdb::process::{AttachConfig, JdbProcess, LaunchConfig, spawn_attach, spawn_launch};
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

    /// 按可选秒值覆盖超时；None 保持默认。
    pub fn with_timeout_secs(self, secs: Option<u64>) -> Self {
        match secs {
            Some(s) => self.with_timeout(Duration::from_secs(s)),
            None => self,
        }
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
            created_at: Some(jiff::Zoned::now().to_string()),
        };
        Ok(Session {
            meta,
            inner: Mutex::new(inner),
            stderr,
            _stderr_handle: stderr_handle,
        })
    }

    /// 以 attach 模式连接已运行的 JVM：spawn `jdb -attach`、起读取线程、读掉连接握手输出。
    ///
    /// 初始状态设为 `Suspended`：DESIGN §10 推荐目标 JVM 用 `suspend=y` 启动，
    /// attach 后线程处于挂起、调试器掌控（典型流程：attach → 断点 → cont）。
    /// attach 模式没有 `run`（VM 已在运行），见 [`Session::run`]。
    pub fn attach(
        jdb_path: &std::path::Path,
        config: &AttachConfig,
        id: String,
        name: Option<String>,
    ) -> Result<Session> {
        let spawned = spawn_attach(jdb_path, config)?;
        let mut process = spawned.process;
        let jdb_pid = process.pid();
        let mut reader = PromptReader::new(spawned.stdout);
        let (stderr, stderr_handle) = spawn_stderr_drain(spawned.stderr);

        // attach 握手：jdb 连上 JVM 后输出初始化信息并给出 prompt。
        let outcome = reader.read_until_prompt(TIMEOUT_NORMAL, ReadMode::Normal);
        // 连接失败：jdb 报致命错误（stdout）或把错误写到 stderr 后直接退出（→ Eof）。
        // 两种都杀掉 jdb 并报错，避免悬挂子进程。
        match outcome {
            ReadOutcome::Fatal { message } => {
                let _ = process.kill();
                return Err(Error::Connection(message));
            }
            ReadOutcome::Eof { .. } => {
                let _ = process.kill();
                let detail = drain_buf(&stderr)
                    .unwrap_or_else(|| "jdb exited during attach handshake".into());
                return Err(Error::Connection(format!("attach failed: {}", detail.trim())));
            }
            _ => {}
        }

        // attach 到 suspend=y 的 VM 后，jdb 会异步追加 `VM Started` 事件 banner + 额外 prompt。
        // 排空这些待处理输出，否则后续命令会读到上一个 prompt 的滞后内容（attach 特有的双 prompt）。
        for _ in 0..3 {
            match reader.read_until_prompt(Duration::from_millis(800), ReadMode::Normal) {
                ReadOutcome::Prompt { .. } => continue,
                _ => break,
            }
        }

        let inner = SessionInner {
            process,
            reader,
            state: RunState::Suspended,
            last_event: None,
        };
        let meta = SessionMeta {
            id,
            name,
            mode: SessionMode::Attach,
            target: format!("{}:{}", config.host, config.port),
            jdb_pid,
            created_at: Some(jiff::Zoned::now().to_string()),
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

        // Normal 命令在 Suspended 态发出前，清掉 channel 里可能残留的 stale bare-prompt
        // （上一条 blocking 命令命中事件后 jdb 迟到补发的 `> `）。此时缓冲本应为空，
        // 任何残留都是陈旧数据，清掉它避免本次命令读到空响应而错位。
        if kind.mode == ReadMode::Normal && inner.state == RunState::Suspended {
            inner.reader.purge_pending();
        }

        inner.process.write_command(raw)?;
        let outcome = inner.reader.read_until_prompt(kind.timeout, kind.mode);

        // jdb 致命错误 → 失败（会话标记 Dead）。
        if let ReadOutcome::Fatal { message } = &outcome {
            inner.state = RunState::Dead;
            return Err(Error::Connection(message.clone()));
        }

        // Blocking 命令命中事件（断点/异常/单步）后，jdb 可能在 reader 匹配 prompt 之后的极短
        // 窗口里继续 flush 尾部输出（源码行、追加 prompt 等）。排空这些残留，否则下一条命令会
        // 读到错位的滞后内容（attach+suspend=n 高并发场景下尤其明显）。
        if kind.mode == ReadMode::Blocking
            && matches!(&outcome, ReadOutcome::Prompt { event: Some(_), .. })
        {
            inner.reader.drain_stale();
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

    /// 结束会话：先 resume（确保 VM 不卡在 SUSPEND_ALL）、发 `quit`、等 jdb 退出、标记 `Dead`。
    pub fn kill(&self) -> Result<()> {
        let mut inner = self.inner.lock().expect("session mutex poisoned");
        // resume 确保 VM 所有线程恢复执行——否则 jdb detach 后 VM 永久卡死。
        let _ = inner.process.write_command("resume");
        let _ = inner.process.write_command("quit");
        // 给 jdb 短暂的时间处理 resume+quit 并正常退出，之后再强杀兜底。
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if !inner.process.is_alive() {
                break;
            }
        }
        let _ = inner.process.kill();
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
    pub fn stop_at(&self, class: &str, line: u32, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(&format!("stop at {class}:{line}"), CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(timeout))
    }

    /// `stop in Class.method`（可选签名以区分重载）
    pub fn stop_in(&self, class: &str, method: &str, args: Option<&str>, timeout: Option<u64>) -> Result<CommandResponse> {
        let spec = match args {
            Some(a) => format!("stop in {class}.{method}({a})"),
            None => format!("stop in {class}.{method}"),
        };
        self.execute(&spec, CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(timeout))
    }

    /// `run`（仅 launch 模式）
    pub fn run(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        if self.meta.mode == SessionMode::Attach {
            return Err(Error::SessionDead(
                "`run` is launch-mode only; an attached JVM is already running (use `cont`)".into(),
            ));
        }
        self.execute("run", CommandKind::blocking(CommandHint::Run).with_timeout_secs(timeout))
    }

    /// `cont`
    pub fn cont(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("cont", CommandKind::blocking(CommandHint::Cont).with_timeout_secs(timeout))
    }

    /// `step`（step into）
    pub fn step(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("step", CommandKind::blocking(CommandHint::Step).with_timeout_secs(timeout))
    }

    /// `next`（step over）
    pub fn next(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("next", CommandKind::blocking(CommandHint::Next).with_timeout_secs(timeout))
    }

    /// `step up`（run until method returns）
    pub fn step_out(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("step up", CommandKind::blocking(CommandHint::StepOut).with_timeout_secs(timeout))
    }

    /// `where`
    pub fn stack(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("where", CommandKind::normal(CommandHint::Where).with_timeout_secs(timeout))
    }

    /// `locals`
    pub fn locals(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("locals", CommandKind::normal(CommandHint::Locals).with_timeout_secs(timeout))
    }

    /// `print <expr>`
    pub fn print(&self, expr: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(&format!("print {expr}"), CommandKind::normal(CommandHint::Print).with_timeout_secs(timeout))
    }

    /// `threads`
    pub fn threads(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute("threads", CommandKind::normal(CommandHint::Threads).with_timeout_secs(timeout))
    }

    /// `list [line]`
    pub fn list_source(&self, line: Option<u32>, timeout: Option<u64>) -> Result<CommandResponse> {
        let cmd = match line {
            Some(l) => format!("list {l}"),
            None => "list".to_string(),
        };
        self.execute(&cmd, CommandKind::normal(CommandHint::ListSource).with_timeout_secs(timeout))
    }

    /// 透传任意 jdb 命令（escape hatch）。
    pub fn raw(&self, cmd: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(timeout))
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

/// 取出并清空一个共享文本缓冲（用于 attach 失败时读 stderr 线程已捕获的错误）。
fn drain_buf(buf: &Arc<Mutex<String>>) -> Option<String> {
    let mut b = buf.lock().ok()?;
    if b.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut *b))
    }
}

#[cfg(test)]
mod tests {
    //! session 层映射逻辑的单元测试——聚焦 `event_to_result`（纯函数）。
    use super::{CommandKind, event_to_result};
    use crate::jdb::reader::DetectedEvent;
    use crate::protocol::*;

    #[test]
    fn breakpoint_event_maps_to_stopped() {
        let ev = DetectedEvent::Breakpoint {
            thread: "main".into(),
            class: "Main".into(),
            method: "main".into(),
            line: 9,
        };
        let (result, event) = event_to_result(ev);

        let CommandResult::Stopped { location, thread, event: inner_event, frame } = result else {
            panic!("expected Stopped, got {result:?}");
        };
        assert_eq!(thread, "main");
        assert_eq!(location.class, "Main");
        assert_eq!(location.method, "main");
        assert_eq!(location.line, 9);
        assert!(frame.is_none());
        assert!(matches!(inner_event, Event::Breakpoint { .. }));
        assert!(matches!(event, Event::Breakpoint { .. }));
    }

    #[test]
    fn step_event_maps_to_stopped() {
        let ev = DetectedEvent::Step {
            thread: "main".into(),
            class: "Main".into(),
            method: "main".into(),
            line: 10,
        };
        let (result, event) = event_to_result(ev);

        let CommandResult::Stopped { location, event: inner_event, .. } = result else {
            panic!("expected Stopped, got {result:?}");
        };
        assert_eq!(location.line, 10);
        assert!(matches!(inner_event, Event::Step { .. }));
        assert!(matches!(event, Event::Step { .. }));
    }

    #[test]
    fn exception_event_maps_to_exception_caught() {
        let ev = DetectedEvent::Exception {
            thread: "main".into(),
            exception: "java.lang.NullPointerException".into(),
            caught: false,
        };
        let (result, event) = event_to_result(ev);

        let CommandResult::ExceptionCaught { exception, caught, thread, .. } = result else {
            panic!("expected ExceptionCaught, got {result:?}");
        };
        assert_eq!(exception, "java.lang.NullPointerException");
        assert!(!caught);
        assert_eq!(thread, "main");
        assert!(matches!(event, Event::Exception { caught: false, .. }));
    }

    #[test]
    fn command_kind_defaults() {
        use crate::jdb::parser::CommandHint;

        let n = CommandKind::normal(CommandHint::Locals);
        assert_eq!(n.timeout.as_secs(), 15);

        let b = CommandKind::blocking(CommandHint::Run);
        assert_eq!(b.timeout.as_secs(), 30);

        let custom = n.with_timeout(std::time::Duration::from_secs(5));
        assert_eq!(custom.timeout.as_secs(), 5);
    }
}

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
use crate::jdb::parser::{CommandHint, classify_output, parse_threads, parse_where};
use crate::jdb::process::{AttachConfig, JdbProcess, LaunchConfig, normalize_attach_host, spawn_attach, spawn_launch};
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
    /// 最近一次 `break_at` 的目标（class, line），用于命中时比对行号偏差。
    last_break_target: Mutex<Option<(String, u32)>>,
    /// 活跃的条件断点：key = "Class:line" 或 "Class.method"，value = condition expr。
    conditions: Mutex<Vec<(String, String)>>,
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
            last_break_target: Mutex::new(None),
            conditions: Mutex::new(Vec::new()),
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
        // localhost 在双栈机器上常被解析到 IPv6 `::1`，而 JDWP 默认（`address=5005` / `*:5005`）
        // 多数只在 IPv4 `0.0.0.0` 监听 → probe_tcp 与 jdb SocketAttach 都会连 `::1` 被拒。
        // 入口处一次性规范化为 127.0.0.1，使探测、spawn、meta.target 全部用真实可达地址。
        let normalized = AttachConfig {
            host: normalize_attach_host(&config.host),
            ..config.clone()
        };
        let config = &normalized;

        // TCP 探测：快速检查端口可达性，避免 jdb 长时间挂起后才报晦涩错误。
        probe_tcp(&config.host, config.port)?;

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
            last_break_target: Mutex::new(None),
            conditions: Mutex::new(Vec::new()),
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

        // Normal 命令发出前，清掉 channel 里可能残留的 stale bare-prompt 或迟到字节。
        // purge_pending 是 try_recv（非阻塞），无论当前 state 如何都应执行：
        // - Suspended: 上一条 blocking 命令遗留的迟到 bare-prompt `> `
        // - Running: timeout 后 channel 中迟到的事件 banner / prompt
        // Blocking 命令不 purge，因为它需要读到后续的事件 banner。
        if kind.mode == ReadMode::Normal {
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
        let mut response = inner.map_outcome(outcome, kind.hint, stderr);

        // PartialStop 补全：截断 banner（SUSPEND_THREAD）命中后 thread/location 全未知，
        // 在同一锁内用 threads→thread<id>→where 自动填充。
        if is_partial_stopped(&response) {
            enrich_partial_stop_inner(&mut inner, &mut response);
        }

        Ok(response)
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

    /// 记录最近一次 `break_at` 的目标，供命中时比对行号偏差。
    pub fn record_break_target(&self, class: &str, line: u32) {
        *self.last_break_target.lock().expect("break_target mutex poisoned") =
            Some((class.to_string(), line));
    }

    /// 取出并清空最近的 break target（one-shot 消费）。
    pub fn take_break_target(&self) -> Option<(String, u32)> {
        self.last_break_target.lock().expect("break_target mutex poisoned").take()
    }

    /// 注册条件断点：命中 spec 时只有 condition 为 true 才真正停下。
    pub fn add_condition(&self, spec: &str, condition: &str) {
        let mut conds = self.conditions.lock().expect("conditions mutex poisoned");
        conds.retain(|(s, _)| s != spec);
        conds.push((spec.to_string(), condition.to_string()));
    }

    /// 查询 spec 对应的条件表达式。
    pub fn get_condition(&self, spec: &str) -> Option<String> {
        self.conditions.lock().expect("conditions mutex poisoned")
            .iter()
            .find(|(s, _)| s == spec)
            .map(|(_, c)| c.clone())
    }

    // ── 语义便捷方法（封装 jdb 命令字符串，§7 命令面）──────────────────────────

    /// `stop at Class:line`；`suspend == Some("thread")` 时改用 `stop thread at`
    /// （jdb 原生 SUSPEND_THREAD policy：命中时只挂起触发线程，VM 其余线程继续运行）。
    pub fn stop_at(&self, class: &str, line: u32, suspend: Option<&str>, timeout: Option<u64>) -> Result<CommandResponse> {
        // 条件断点由 handler 层实现（命中时 eval + 自动 cont），不依赖 jdb 的 if 语法（JDK 8 不支持）。
        let cmd = if suspend == Some("thread") {
            format!("stop thread at {class}:{line}")
        } else {
            format!("stop at {class}:{line}")
        };
        self.execute(&cmd, CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(timeout))
    }

    /// `stop in Class.method`（可选签名以区分重载）；`suspend == Some("thread")` 时
    /// 改用 `stop thread in`（SUSPEND_THREAD policy：命中时只挂起触发线程）。
    pub fn stop_in(&self, class: &str, method: &str, args: Option<&str>, suspend: Option<&str>, timeout: Option<u64>) -> Result<CommandResponse> {
        // 条件断点由 handler 层实现。
        let kw = if suspend == Some("thread") { "stop thread in" } else { "stop in" };
        let spec = match args {
            Some(a) => format!("{kw} {class}.{method}({a})"),
            None => format!("{kw} {class}.{method}"),
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
    /// 在持锁状态下发一条普通（Normal 模式）jdb 命令并返回其 prompt 之前的原始输出文本。
    /// 仅用于 PartialStop 补全（threads/thread/where）——不推进状态机、不做 classify。
    /// 任何非 Prompt 结局（超时/EOF/VM退出/致命）都返回 None，由调用方写 WARNING 兜底。
    fn run_query(&mut self, raw: &str) -> Option<String> {
        if self.process.write_command(raw).is_err() {
            return None;
        }
        match self.reader.read_until_prompt(Duration::from_secs(5), ReadMode::Normal) {
            ReadOutcome::Prompt { output, .. } => Some(output),
            _ => None,
        }
    }

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
                CommandResult::Stopped {
                    event: event.clone(), location: loc, thread, frame: None, source_context: None,
                },
                event,
            )
        }
        DetectedEvent::Step { thread, class, method, line } => {
            let loc = Location { class, method, file: None, line };
            let event = Event::Step { location: loc.clone(), thread: thread.clone() };
            (
                CommandResult::Stopped {
                    event: event.clone(), location: loc, thread, frame: None, source_context: None,
                },
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
        DetectedEvent::FieldWatch { thread, field, access_type, class, method, line } => {
            let loc = Location { class: class.clone(), method: method.clone(), file: None, line };
            let event = Event::FieldWatch {
                field: field.clone(),
                access_type: access_type.clone(),
                thread: thread.clone(),
            };
            (
                CommandResult::Stopped {
                    event: event.clone(), location: loc, thread, frame: None, source_context: None,
                },
                event,
            )
        }
        // JDK 8 SUSPEND_THREAD 截断 banner：thread/location 全未知，先用空占位，
        // 由 `enrich_partial_stop`（threads→thread<id>→where）补全。
        DetectedEvent::PartialStop { is_step } => {
            let loc = Location { class: String::new(), method: String::new(), file: None, line: 0 };
            let event = if is_step {
                Event::Step { location: loc.clone(), thread: String::new() }
            } else {
                Event::Breakpoint { location: loc.clone(), thread: String::new() }
            };
            (
                CommandResult::Stopped {
                    event: event.clone(), location: loc, thread: String::new(), frame: None, source_context: None,
                },
                event,
            )
        }
    }
}

/// PartialStop 标志：thread 与 location 全空的 `Stopped`（截断 banner 经 `event_to_result` 后的形态）。
fn is_partial_stopped(resp: &CommandResponse) -> bool {
    matches!(
        &resp.result,
        CommandResult::Stopped { thread, location, .. }
            if thread.is_empty() && location.class.is_empty() && location.line == 0
    )
}

/// 把 WARNING 追加到 response 的 note（与 handler 层 `append_note` 同语义：绝不静默 fallback）。
fn append_warning(resp: &mut CommandResponse, msg: &str) {
    match &mut resp.note {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(msg);
        }
        None => resp.note = Some(msg.to_string()),
    }
}

/// 补全 PartialStop（JDK 8 SUSPEND_THREAD 截断 banner）的 thread/location：
/// `threads`（找 `(at breakpoint)` 线程）→ `thread <id>`（切当前线程）→ `where`（取栈顶帧）。
/// 在持有命令锁的 `SessionInner` 上执行（与命中是同一次 execute）。失败任一步即写 WARNING 并保留空字段。
fn enrich_partial_stop_inner(inner: &mut SessionInner, resp: &mut CommandResponse) {
    // 1. threads → 找命中线程（state 含 "at breakpoint"）。
    let Some(threads_out) = inner.run_query("threads") else {
        append_warning(resp, "WARNING: thread breakpoint hit, but `threads` query failed; thread/location unknown. Run `threads` manually.");
        return;
    };
    let hit = match parse_threads(&threads_out) {
        CommandResult::Threads { threads } => {
            threads.into_iter().find(|t| t.state.contains("at breakpoint"))
        }
        _ => None,
    };
    let Some(hit) = hit else {
        append_warning(resp, "WARNING: thread breakpoint hit, but no thread is marked `(at breakpoint)` in `threads` output; location unknown.");
        return;
    };

    // 回填线程名（即便 where 失败也能给出触发线程）。
    if let CommandResult::Stopped { thread, event, .. } = &mut resp.result {
        *thread = hit.name.clone();
        set_event_thread(event, &hit.name);
    }

    // 2. thread <id> → 切到命中线程（否则 where 报 "No thread specified."）。
    if inner.run_query(&format!("thread {}", hit.id)).is_none() {
        append_warning(resp, "WARNING: failed to select the hit thread (`thread <id>`); location unknown.");
        return;
    }

    // 3. where → 栈顶帧给出 class/method/file/line。
    let Some(where_out) = inner.run_query("where") else {
        append_warning(resp, "WARNING: `where` query failed after selecting the hit thread; location unknown.");
        return;
    };
    let top = match parse_where(&where_out) {
        CommandResult::StackTrace { frames } => frames.into_iter().next(),
        _ => None,
    };
    let Some(top) = top else {
        append_warning(resp, "WARNING: could not parse the hit thread's stack (`where`); location unknown.");
        return;
    };

    // 回填 location + frame（frame 直接复用，省得 handler 的 enrich_stopped 再查一次 where）。
    if let CommandResult::Stopped { location, frame, event, .. } = &mut resp.result {
        *location = top.location.clone();
        set_event_location(event, &top.location);
        *frame = Some(top);
    }
}

/// 把补全到的线程名写回事件（Breakpoint/Step 的 thread 字段）。
fn set_event_thread(event: &mut Event, thread: &str) {
    match event {
        Event::Breakpoint { thread: t, .. } | Event::Step { thread: t, .. } => {
            *t = thread.to_string();
        }
        _ => {}
    }
}

/// 把补全到的位置写回事件（Breakpoint/Step 的 location 字段）。
fn set_event_location(event: &mut Event, loc: &Location) {
    match event {
        Event::Breakpoint { location, .. } | Event::Step { location, .. } => {
            *location = loc.clone();
        }
        _ => {}
    }
}


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

/// TCP 探测：尝试连接 host:port，超时 3 秒。失败时返回清晰的诊断信息。
fn probe_tcp(host: &str, port: u16) -> Result<()> {
    use std::net::{TcpStream, ToSocketAddrs};

    let addr = format!("{host}:{port}");
    let sock_addr = addr
        .to_socket_addrs()
        .map_err(|e| Error::Connection(format!("cannot resolve {addr}: {e}")))?
        .next()
        .ok_or_else(|| Error::Connection(format!("cannot resolve {addr}: no addresses")))?;

    TcpStream::connect_timeout(&sock_addr, Duration::from_secs(3)).map_err(|e| {
        Error::Connection(format!(
            "port {port} on {host} is not reachable ({e}). \
             Check: is the target JVM running? Is JDWP enabled with server=y and the correct port?"
        ))
    })?;
    Ok(())
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

        let CommandResult::Stopped { location, thread, event: inner_event, frame, source_context } = result else {
            panic!("expected Stopped, got {result:?}");
        };
        assert_eq!(thread, "main");
        assert_eq!(location.class, "Main");
        assert_eq!(location.method, "main");
        assert_eq!(location.line, 9);
        assert!(frame.is_none());
        assert!(source_context.is_none());
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

    // ─── execute behavior tests would require a real jdb process (see integration tests) ───
}

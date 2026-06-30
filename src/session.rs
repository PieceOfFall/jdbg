//! Debug session: bind the jdb child process and reader/stderr threads, and drive the RunState machine.
//!
//! `Session` is the engine's core coordination layer (roadmap 3):
//! - Owns [`JdbProcess`] + [`PromptReader`] + stderr drain thread (§5 three-thread model).
//! - Uses a per-session command lock (internal `Mutex`) so only one command is in flight at a time.
//! - [`Session::execute`] maps `ReadOutcome` + parser → [`CommandResponse`]
//!   (event → `Stopped` / `ExceptionCaught`, VM exit → `VmExited`, timeout → `Timeout`).
//! - Semantic convenience methods ([`Session::run`], [`Session::stop_at`], ...) wrap jdb command strings,
//!   so upper layers like the CLI do not need to know jdb syntax.
//!
//! `Session` uses interior mutability (`Mutex<SessionInner>`) so commands can execute through `&self`,
//! making it easy to share as `Arc<Session>` across daemon threads.

use std::process::ChildStderr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::jdb::parser::{CommandHint, classify_output, parse_threads, parse_where};
use crate::jdb::process::{
    AttachConfig, JdbProcess, LaunchConfig, normalize_attach_host, spawn_attach, spawn_launch,
};
use crate::jdb::reader::{DetectedEvent, PromptReader, ReadMode, ReadOutcome};
use crate::protocol::*;

/// Default timeout for normal commands.
const TIMEOUT_NORMAL: Duration = Duration::from_secs(15);
/// Default timeout for blocking commands (run/cont/step...).
const TIMEOUT_BLOCKING: Duration = Duration::from_secs(30);

// ─── Session Metadata (Immutable) ──────────────────────────────────────────────

/// Immutable metadata for a session.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: Option<String>,
    pub mode: SessionMode,
    pub target: String,
    pub jdb_pid: u32,
    /// Creation timestamp. Later phases wire this to `jiff`; this phase leaves it as None.
    pub created_at: Option<String>,
}

// ─── Command Execution Traits ─────────────────────────────────────────────────

/// Describes how a jdb command executes: read mode, parser hint, and timeout.
#[derive(Debug, Clone, Copy)]
pub struct CommandKind {
    pub mode: ReadMode,
    pub hint: CommandHint,
    pub timeout: Duration,
}

impl CommandKind {
    /// Normal command (locals/where/print...): any prompt completes it; short timeout.
    pub fn normal(hint: CommandHint) -> Self {
        Self {
            mode: ReadMode::Normal,
            hint,
            timeout: TIMEOUT_NORMAL,
        }
    }

    /// Blocking command (run/cont/step/next/step-out): wait for event / thread-prompt / VM exit; long timeout.
    pub fn blocking(hint: CommandHint) -> Self {
        Self {
            mode: ReadMode::Blocking,
            hint,
            timeout: TIMEOUT_BLOCKING,
        }
    }

    /// Timeout override matching CLI `--timeout`.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override timeout with an optional seconds value; None keeps the default.
    pub fn with_timeout_secs(self, secs: Option<u64>) -> Self {
        match secs {
            Some(s) => self.with_timeout(Duration::from_secs(s)),
            None => self,
        }
    }
}

// ─── Session ────────────────────────────────────────────────────────────────────

/// One background debug session: one jdb child process plus one debugged JVM.
pub struct Session {
    pub meta: SessionMeta,
    inner: Mutex<SessionInner>,
    /// Content accumulated by the stderr drain thread, taken after each execute as a side band.
    stderr: Arc<Mutex<String>>,
    _stderr_handle: JoinHandle<()>,
    /// Most recent `break_at` target (class, line), used to compare hit-line drift.
    last_break_target: Mutex<Option<(String, u32)>>,
    /// Active conditional breakpoints: key = "Class:line" or "Class.method", value = condition expr.
    conditions: Mutex<Vec<(String, String)>>,
}

/// Mutable state protected by the command lock.
struct SessionInner {
    process: JdbProcess,
    reader: PromptReader,
    state: RunState,
    last_event: Option<Event>,
}

impl Session {
    /// Start a launch-mode session: spawn jdb, start reader threads, consume the initial prompt, state `Loaded`.
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

        // After launch, jdb immediately prints an initial `> ` prompt before the VM has started.
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

    /// Connect to a running JVM in attach mode: spawn attach-mode jdb, start reader threads, consume handshake output.
    ///
    /// Initial state is `Suspended`: DESIGN §10 recommends starting the target JVM with `suspend=y`, so after
    /// attach the threads are suspended and debugger-controlled (typical flow: attach → breakpoints → cont).
    /// Attach mode has no `run` because the VM is already running; see [`Session::run`].
    pub fn attach(
        jdb_path: &std::path::Path,
        config: &AttachConfig,
        id: String,
        name: Option<String>,
    ) -> Result<Session> {
        // On dual-stack machines, localhost often resolves to IPv6 `::1`, while JDWP defaults
        // (`address=5005` / `*:5005`) usually listen only on IPv4 `0.0.0.0`. Then probe_tcp and jdb
        // SocketAttach both connect to `::1` and get refused. Normalize once at the entry point to
        // 127.0.0.1 so probing, spawn, and meta.target all use the reachable address.
        let normalized = AttachConfig {
            host: normalize_attach_host(&config.host),
            ..config.clone()
        };
        let config = &normalized;

        // TCP probe: quickly check port reachability so jdb does not hang for a long time and report a vague error.
        probe_tcp(&config.host, config.port)?;

        let spawned = spawn_attach(jdb_path, config)?;
        let mut process = spawned.process;
        let jdb_pid = process.pid();
        let mut reader = PromptReader::new(spawned.stdout);
        let (stderr, stderr_handle) = spawn_stderr_drain(spawned.stderr);

        // Attach handshake: after jdb connects to the JVM, it prints initialization info and a prompt.
        let outcome = reader.read_until_prompt(TIMEOUT_NORMAL, ReadMode::Normal);
        // Connection failure: jdb either reports a fatal error on stdout or writes to stderr and exits (→ Eof).
        // In both cases, kill jdb and return an error to avoid a dangling child process.
        match outcome {
            ReadOutcome::Fatal { message } => {
                let _ = process.kill();
                return Err(Error::Connection(message));
            }
            ReadOutcome::Eof { .. } => {
                let _ = process.kill();
                let detail = drain_buf(&stderr)
                    .unwrap_or_else(|| "jdb exited during attach handshake".into());
                return Err(Error::Connection(format!(
                    "attach failed: {}",
                    detail.trim()
                )));
            }
            _ => {}
        }

        // After attaching to a suspend=y VM, jdb asynchronously appends a `VM Started` event banner plus an
        // extra prompt. Drain these pending bytes or later commands will read stale output from the previous
        // prompt, an attach-specific double-prompt behavior.
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

    /// Execute one raw jdb command and map it into a structured response.
    ///
    /// Hold the command lock until the prompt is read, ensuring only one command per session is in flight (§5).
    pub fn execute(&self, raw: &str, kind: CommandKind) -> Result<CommandResponse> {
        let mut inner = self.inner.lock().expect("session mutex poisoned");

        // Reject new commands for terminated sessions.
        if matches!(inner.state, RunState::Exited | RunState::Dead) {
            return Err(Error::SessionDead(format!(
                "session {} is {:?}",
                self.meta.id, inner.state
            )));
        }

        // Before Normal commands, clear possible stale bare-prompts or late bytes from the channel.
        // purge_pending uses try_recv (non-blocking) and must run regardless of current state:
        // - Suspended: late bare-prompt `> ` left by the previous blocking command
        // - Running: late event banner / prompt after timeout
        // Blocking commands do not purge because they need to read subsequent event banners.
        if kind.mode == ReadMode::Normal {
            inner.reader.purge_pending();
        }

        inner.process.write_command(raw)?;
        let outcome = inner.reader.read_until_prompt(kind.timeout, kind.mode);

        // jdb fatal error: fail and mark the session Dead.
        if let ReadOutcome::Fatal { message } = &outcome {
            inner.state = RunState::Dead;
            return Err(Error::Connection(message.clone()));
        }

        // After a Blocking command hits an event (breakpoint/exception/step), jdb may keep flushing tail output
        // for a tiny window after the reader matched the prompt (source lines, extra prompt, etc.). Drain that
        // residue or the next command may read misaligned stale content, especially in attach+suspend=n cases.
        if kind.mode == ReadMode::Blocking
            && matches!(&outcome, ReadOutcome::Prompt { event: Some(_), .. })
        {
            inner.reader.drain_stale();
        }

        let stderr = self.take_stderr();
        let mut response = inner.map_outcome(outcome, kind.hint, stderr);

        // PartialStop enrichment: after a truncated SUSPEND_THREAD banner, thread/location are unknown.
        // Fill them under the same lock via threads→thread<id>→where.
        if is_partial_stopped(&response) {
            enrich_partial_stop_inner(&mut inner, &mut response);
        }

        Ok(response)
    }

    /// Current run state.
    pub fn state(&self) -> RunState {
        self.inner.lock().expect("session mutex poisoned").state
    }

    /// Session status report without sending a command to jdb.
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

    /// End the session: resume first so the VM is not stuck in SUSPEND_ALL, send `quit`, wait for jdb exit, mark `Dead`.
    pub fn kill(&self) -> Result<()> {
        let mut inner = self.inner.lock().expect("session mutex poisoned");
        // resume ensures all VM threads resume; otherwise the VM can remain stuck forever after jdb detaches.
        let _ = inner.process.write_command("resume");
        let _ = inner.process.write_command("quit");
        // Give jdb a short window to process resume+quit and exit normally, then force-kill as fallback.
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

    /// Take and clear content accumulated by the stderr drain.
    fn take_stderr(&self) -> Option<String> {
        let mut s = self.stderr.lock().expect("stderr mutex poisoned");
        if s.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *s))
        }
    }

    /// Record the latest `break_at` target for hit-line drift comparison.
    pub fn record_break_target(&self, class: &str, line: u32) {
        *self
            .last_break_target
            .lock()
            .expect("break_target mutex poisoned") = Some((class.to_string(), line));
    }

    /// Take and clear the latest break target as a one-shot value.
    pub fn take_break_target(&self) -> Option<(String, u32)> {
        self.last_break_target
            .lock()
            .expect("break_target mutex poisoned")
            .take()
    }

    /// Register a conditional breakpoint: when the spec is hit, stop only if condition evaluates true.
    pub fn add_condition(&self, spec: &str, condition: &str) {
        let mut conds = self.conditions.lock().expect("conditions mutex poisoned");
        conds.retain(|(s, _)| s != spec);
        conds.push((spec.to_string(), condition.to_string()));
    }

    /// Whether this session has any active conditional breakpoints.
    pub fn has_conditions(&self) -> bool {
        !self
            .conditions
            .lock()
            .expect("conditions mutex poisoned")
            .is_empty()
    }

    /// Look up the condition expression for a spec.
    pub fn get_condition(&self, spec: &str) -> Option<String> {
        self.conditions
            .lock()
            .expect("conditions mutex poisoned")
            .iter()
            .find(|(s, _)| s == spec)
            .map(|(_, c)| c.clone())
    }

    /// Find a condition whose spec starts with the given prefix (used for overloaded method breakpoints).
    pub fn find_condition_by_prefix(&self, prefix: &str) -> Option<String> {
        self.conditions
            .lock()
            .expect("conditions mutex poisoned")
            .iter()
            .find(|(s, _)| s.starts_with(prefix))
            .map(|(_, c)| c.clone())
    }

    /// Find the condition for a hit breakpoint location.
    pub fn condition_for_hit(&self, class: &str, line: u32, method: &str) -> Option<String> {
        let conds = self.conditions.lock().expect("conditions mutex poisoned");
        find_condition_for_hit(&conds, class, line, method)
    }

    /// Find a condition for a line breakpoint with tolerance for JVM line rounding.
    /// Looks for stored specs matching "class:N" where N is within ±`tolerance` of `hit_line`.
    pub fn find_condition_nearby(
        &self,
        class: &str,
        hit_line: u32,
        tolerance: u32,
    ) -> Option<String> {
        let conds = self.conditions.lock().expect("conditions mutex poisoned");
        find_line_condition(&conds, class, hit_line, tolerance)
    }

    // ── Semantic Convenience Methods (wrap jdb command strings, §7 CLI surface) ─

    /// `stop at Class:line`; when `suspend == Some("thread")`, use `stop thread at` instead.
    /// Native jdb SUSPEND_THREAD policy suspends only the triggering thread while the rest of the VM keeps running.
    pub fn stop_at(
        &self,
        class: &str,
        line: u32,
        suspend: Option<&str>,
        timeout: Option<u64>,
    ) -> Result<CommandResponse> {
        // Conditional breakpoints are implemented in handler (eval on hit + auto-cont), not with jdb `if` syntax,
        // which JDK 8 does not support.
        let cmd = if suspend == Some("thread") {
            format!("stop thread at {class}:{line}")
        } else {
            format!("stop at {class}:{line}")
        };
        self.execute(
            &cmd,
            CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(timeout),
        )
    }

    /// `stop in Class.method` with optional signature to disambiguate overloads. When `suspend == Some("thread")`,
    /// use `stop thread in` (SUSPEND_THREAD policy suspends only the triggering thread).
    pub fn stop_in(
        &self,
        class: &str,
        method: &str,
        args: Option<&str>,
        suspend: Option<&str>,
        timeout: Option<u64>,
    ) -> Result<CommandResponse> {
        // Conditional breakpoints are implemented in the handler layer.
        let kw = if suspend == Some("thread") {
            "stop thread in"
        } else {
            "stop in"
        };
        let spec = match args {
            Some(a) => format!("{kw} {class}.{method}({a})"),
            None => format!("{kw} {class}.{method}"),
        };
        self.execute(
            &spec,
            CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(timeout),
        )
    }

    /// `run`, launch mode only.
    pub fn run(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        if self.meta.mode == SessionMode::Attach {
            return Err(Error::SessionDead(
                "`run` is launch-mode only; an attached JVM is already running (use `cont`)".into(),
            ));
        }
        self.execute(
            "run",
            CommandKind::blocking(CommandHint::Run).with_timeout_secs(timeout),
        )
    }

    /// `cont`
    pub fn cont(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "cont",
            CommandKind::blocking(CommandHint::Cont).with_timeout_secs(timeout),
        )
    }

    /// `step`（step into）
    pub fn step(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "step",
            CommandKind::blocking(CommandHint::Step).with_timeout_secs(timeout),
        )
    }

    /// `next`（step over）
    pub fn next(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "next",
            CommandKind::blocking(CommandHint::Next).with_timeout_secs(timeout),
        )
    }

    /// `step up`（run until method returns）
    pub fn step_out(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "step up",
            CommandKind::blocking(CommandHint::StepOut).with_timeout_secs(timeout),
        )
    }

    /// `where`
    pub fn stack(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "where",
            CommandKind::normal(CommandHint::Where).with_timeout_secs(timeout),
        )
    }

    /// `locals`
    pub fn locals(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "locals",
            CommandKind::normal(CommandHint::Locals).with_timeout_secs(timeout),
        )
    }

    /// `print <expr>`
    pub fn print(&self, expr: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            &format!("print {expr}"),
            CommandKind::normal(CommandHint::Print).with_timeout_secs(timeout),
        )
    }

    /// `threads`
    pub fn threads(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            "threads",
            CommandKind::normal(CommandHint::Threads).with_timeout_secs(timeout),
        )
    }

    /// `list [line]`
    pub fn list_source(&self, line: Option<u32>, timeout: Option<u64>) -> Result<CommandResponse> {
        let cmd = match line {
            Some(l) => format!("list {l}"),
            None => "list".to_string(),
        };
        self.execute(
            &cmd,
            CommandKind::normal(CommandHint::ListSource).with_timeout_secs(timeout),
        )
    }

    /// Pass through any jdb command as an escape hatch.
    pub fn raw(&self, cmd: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        self.execute(
            cmd,
            CommandKind::normal(CommandHint::Other).with_timeout_secs(timeout),
        )
    }
}

fn find_condition_for_hit(
    conditions: &[(String, String)],
    hit_class: &str,
    hit_line: u32,
    hit_method: &str,
) -> Option<String> {
    find_line_condition(conditions, hit_class, hit_line, 0)
        .or_else(|| find_line_condition(conditions, hit_class, hit_line, 5))
        .or_else(|| find_method_condition(conditions, hit_class, hit_method))
}

fn find_line_condition(
    conditions: &[(String, String)],
    hit_class: &str,
    hit_line: u32,
    tolerance: u32,
) -> Option<String> {
    conditions.iter().find_map(|(spec, condition)| {
        let (stored_class, stored_line) = parse_line_condition_spec(spec)?;
        (class_spec_matches(stored_class, hit_class) && stored_line.abs_diff(hit_line) <= tolerance)
            .then(|| condition.clone())
    })
}

fn find_method_condition(
    conditions: &[(String, String)],
    hit_class: &str,
    hit_method: &str,
) -> Option<String> {
    conditions.iter().find_map(|(spec, condition)| {
        let (stored_class, stored_method) = parse_method_condition_spec(spec)?;
        (class_spec_matches(stored_class, hit_class) && stored_method == hit_method)
            .then(|| condition.clone())
    })
}

fn parse_line_condition_spec(spec: &str) -> Option<(&str, u32)> {
    let (class, line) = spec.rsplit_once(':')?;
    Some((class, line.parse().ok()?))
}

fn parse_method_condition_spec(spec: &str) -> Option<(&str, &str)> {
    let prefix = spec.split_once('(').map_or(spec, |(prefix, _)| prefix);
    let (class, method) = prefix.rsplit_once('.')?;
    (!class.is_empty() && !method.is_empty()).then_some((class, method))
}

fn class_spec_matches(stored: &str, hit: &str) -> bool {
    stored == hit
        || hit
            .strip_suffix(stored)
            .is_some_and(|prefix| prefix.ends_with('.'))
        || stored
            .strip_suffix(hit)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

impl SessionInner {
    /// Send one Normal-mode jdb command while holding the lock and return raw output before the prompt.
    /// Used only for PartialStop enrichment (threads/thread/where); does not advance state or classify.
    /// Any non-Prompt outcome (timeout/EOF/VM exit/fatal) returns None, and the caller writes a WARNING fallback.
    fn run_query(&mut self, raw: &str) -> Option<String> {
        if self.process.write_command(raw).is_err() {
            return None;
        }
        match self
            .reader
            .read_until_prompt(Duration::from_secs(5), ReadMode::Normal)
        {
            ReadOutcome::Prompt { output, .. } => Some(output),
            _ => None,
        }
    }

    /// Map the reader's `ReadOutcome` into `CommandResponse` and advance the state machine.
    fn map_outcome(
        &mut self,
        outcome: ReadOutcome,
        hint: CommandHint,
        stderr: Option<String>,
    ) -> CommandResponse {
        let (result, note) = match outcome {
            ReadOutcome::Prompt { output, event } => match event {
                // Event (breakpoint/step/exception): Suspended, yielding Stopped/ExceptionCaught.
                Some(ev) => {
                    let (result, evt) = event_to_result(ev);
                    self.state = RunState::Suspended;
                    self.last_event = Some(evt);
                    (result, None)
                }
                // Normal command: classify with parser, state unchanged.
                None => classify_output(&output, hint),
            },
            ReadOutcome::VmExit { output } => {
                self.state = RunState::Exited;
                self.last_event = Some(Event::VmExit);
                (
                    CommandResult::VmExited {
                        exit_code: None,
                        tail: Some(output),
                    },
                    None,
                )
            }
            ReadOutcome::Timeout { partial } => {
                // The app may be deadlocked or in a long loop. Non-destructive: keep session alive and mark Running (§5).
                self.state = RunState::Running;
                (
                    CommandResult::Timeout {
                        partial_output: partial,
                        state: RunState::Running,
                    },
                    None,
                )
            }
            ReadOutcome::Eof { output } => {
                self.state = RunState::Dead;
                (
                    CommandResult::VmExited {
                        exit_code: None,
                        tail: Some(output),
                    },
                    None,
                )
            }
            // Fatal is intercepted in execute and turned into Err; this is a fallback.
            ReadOutcome::Fatal { message } => {
                self.state = RunState::Dead;
                (CommandResult::Raw { text: message }, None)
            }
        };
        CommandResponse {
            result,
            stderr,
            note,
        }
    }
}

/// Convert reader `DetectedEvent` into `CommandResult` plus the recorded `Event`.
fn event_to_result(ev: DetectedEvent) -> (CommandResult, Event) {
    match ev {
        DetectedEvent::Breakpoint {
            thread,
            class,
            method,
            line,
        } => {
            let loc = Location {
                class,
                method,
                file: None,
                line,
            };
            let event = Event::Breakpoint {
                location: loc.clone(),
                thread: thread.clone(),
            };
            (
                CommandResult::Stopped {
                    event: event.clone(),
                    location: loc,
                    thread,
                    thread_id: None,
                    frame: None,
                    source_context: None,
                },
                event,
            )
        }
        DetectedEvent::Step {
            thread,
            class,
            method,
            line,
        } => {
            let loc = Location {
                class,
                method,
                file: None,
                line,
            };
            let event = Event::Step {
                location: loc.clone(),
                thread: thread.clone(),
            };
            (
                CommandResult::Stopped {
                    event: event.clone(),
                    location: loc,
                    thread,
                    thread_id: None,
                    frame: None,
                    source_context: None,
                },
                event,
            )
        }
        DetectedEvent::Exception {
            thread,
            exception,
            caught,
        } => {
            // Exception banners do not include location; `where` may fill it later. Use an empty placeholder here.
            let loc = Location {
                class: String::new(),
                method: String::new(),
                file: None,
                line: 0,
            };
            let event = Event::Exception {
                exception: exception.clone(),
                caught,
                location: Some(loc.clone()),
                thread: thread.clone(),
            };
            (
                CommandResult::ExceptionCaught {
                    exception,
                    caught,
                    location: loc,
                    thread,
                    thread_id: None,
                },
                event,
            )
        }
        DetectedEvent::FieldWatch {
            thread,
            field,
            access_type,
            class,
            method,
            line,
        } => {
            let loc = Location {
                class: class.clone(),
                method: method.clone(),
                file: None,
                line,
            };
            let event = Event::FieldWatch {
                field: field.clone(),
                access_type: access_type.clone(),
                thread: thread.clone(),
            };
            (
                CommandResult::Stopped {
                    event: event.clone(),
                    location: loc,
                    thread,
                    thread_id: None,
                    frame: None,
                    source_context: None,
                },
                event,
            )
        }
        // JDK 8 SUSPEND_THREAD truncated banner: thread/location are unknown, so start with empty placeholders.
        // `enrich_partial_stop` fills them through threads→thread<id>→where.
        DetectedEvent::PartialStop { is_step } => {
            let loc = Location {
                class: String::new(),
                method: String::new(),
                file: None,
                line: 0,
            };
            let event = if is_step {
                Event::Step {
                    location: loc.clone(),
                    thread: String::new(),
                }
            } else {
                Event::Breakpoint {
                    location: loc.clone(),
                    thread: String::new(),
                }
            };
            (
                CommandResult::Stopped {
                    event: event.clone(),
                    location: loc,
                    thread: String::new(),
                    thread_id: None,
                    frame: None,
                    source_context: None,
                },
                event,
            )
        }
    }
}

/// PartialStop marker: a `Stopped` whose thread and location are empty after `event_to_result` handles a truncated banner.
fn is_partial_stopped(resp: &CommandResponse) -> bool {
    matches!(
        &resp.result,
        CommandResult::Stopped { thread, location, .. }
            if thread.is_empty() && location.class.is_empty() && location.line == 0
    )
}

/// Append a WARNING to response.note, matching handler-layer `append_note` semantics: never silently fallback.
fn append_warning(resp: &mut CommandResponse, msg: &str) {
    match &mut resp.note {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(msg);
        }
        None => resp.note = Some(msg.to_string()),
    }
}

/// Enrich PartialStop (JDK 8 SUSPEND_THREAD truncated banner) with thread/location:
/// `threads` (find `(at breakpoint)` thread) → `thread <id>` (select current thread) → `where` (top frame).
/// Runs on `SessionInner` while the command lock is held, during the same execute as the hit. Any failure writes
/// a WARNING and leaves empty fields in place.
fn enrich_partial_stop_inner(inner: &mut SessionInner, resp: &mut CommandResponse) {
    // 1. threads: find the hit thread whose state contains "at breakpoint".
    let Some(threads_out) = inner.run_query("threads") else {
        append_warning(
            resp,
            "WARNING: thread breakpoint hit, but `threads` query failed; thread/location unknown. Run `threads` manually.",
        );
        return;
    };
    let hit = match parse_threads(&threads_out) {
        CommandResult::Threads { threads } => threads
            .into_iter()
            .find(|t| t.state.contains("at breakpoint")),
        _ => None,
    };
    let Some(hit) = hit else {
        append_warning(
            resp,
            "WARNING: thread breakpoint hit, but no thread is marked `(at breakpoint)` in `threads` output; location unknown.",
        );
        return;
    };

    // Backfill thread name + id, so even if where fails the triggering thread and id are available for `thread <id>`.
    if let CommandResult::Stopped {
        thread,
        thread_id,
        event,
        ..
    } = &mut resp.result
    {
        *thread = hit.name.clone();
        *thread_id = Some(hit.id.clone());
        set_event_thread(event, &hit.name);
    }

    // 2. thread <id>: switch to the hit thread, otherwise where reports "No thread specified."
    if inner.run_query(&format!("thread {}", hit.id)).is_none() {
        append_warning(
            resp,
            "WARNING: failed to select the hit thread (`thread <id>`); location unknown.",
        );
        return;
    }

    // 3. where: top frame gives class/method/file/line.
    let Some(where_out) = inner.run_query("where") else {
        append_warning(
            resp,
            "WARNING: `where` query failed after selecting the hit thread; location unknown.",
        );
        return;
    };
    let top = match parse_where(&where_out) {
        CommandResult::StackTrace { frames } => frames.into_iter().next(),
        _ => None,
    };
    let Some(top) = top else {
        append_warning(
            resp,
            "WARNING: could not parse the hit thread's stack (`where`); location unknown.",
        );
        return;
    };

    // Backfill location + frame. Reuse the frame so handler enrich_stopped does not need another where query.
    if let CommandResult::Stopped {
        location,
        frame,
        event,
        ..
    } = &mut resp.result
    {
        *location = top.location.clone();
        set_event_location(event, &top.location);
        *frame = Some(top);
    }
}

/// Write the enriched thread name back into the event (Breakpoint/Step thread field).
fn set_event_thread(event: &mut Event, thread: &str) {
    match event {
        Event::Breakpoint { thread: t, .. } | Event::Step { thread: t, .. } => {
            *t = thread.to_string();
        }
        _ => {}
    }
}

/// Write the enriched location back into the event (Breakpoint/Step location field).
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

/// Take and clear a shared text buffer, used for stderr captured by the reader thread during attach failures.
fn drain_buf(buf: &Arc<Mutex<String>>) -> Option<String> {
    let mut b = buf.lock().ok()?;
    if b.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut *b))
    }
}

/// TCP probe: try to connect to host:port with a 3-second timeout. On failure, return a clear diagnostic.
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
    //! Unit tests for session-layer mapping logic, focused on pure function `event_to_result`.
    use super::{CommandKind, class_spec_matches, event_to_result, find_condition_for_hit};
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

        let CommandResult::Stopped {
            location,
            thread,
            event: inner_event,
            frame,
            source_context,
            ..
        } = result
        else {
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

        let CommandResult::Stopped {
            location,
            event: inner_event,
            ..
        } = result
        else {
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

        let CommandResult::ExceptionCaught {
            exception,
            caught,
            thread,
            ..
        } = result
        else {
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

    #[test]
    fn condition_hit_matches_short_class_against_fully_qualified_hit() {
        let conditions = vec![(
            "RequestFacade:3956".to_string(),
            "name.equals(\"userToken\")".to_string(),
        )];

        assert_eq!(
            find_condition_for_hit(
                &conditions,
                "org.apache.catalina.connector.RequestFacade",
                3956,
                "getHeader",
            )
            .as_deref(),
            Some("name.equals(\"userToken\")")
        );
    }

    #[test]
    fn condition_hit_matches_fully_qualified_spec_against_short_hit() {
        let conditions = vec![(
            "org.apache.catalina.connector.RequestFacade:3956".to_string(),
            "name.equals(\"userToken\")".to_string(),
        )];

        assert_eq!(
            find_condition_for_hit(&conditions, "RequestFacade", 3956, "getHeader").as_deref(),
            Some("name.equals(\"userToken\")")
        );
    }

    #[test]
    fn condition_hit_allows_nearby_line_rounding() {
        let conditions = vec![("CartService:42".to_string(), "userId == 123".to_string())];

        assert_eq!(
            find_condition_for_hit(&conditions, "com.example.CartService", 45, "add").as_deref(),
            Some("userId == 123")
        );
    }

    #[test]
    fn condition_hit_matches_method_with_overload_args() {
        let conditions = vec![(
            "com.example.CartService.add(java.lang.String,int)".to_string(),
            "qty > 1".to_string(),
        )];

        assert_eq!(
            find_condition_for_hit(&conditions, "CartService", 0, "add").as_deref(),
            Some("qty > 1")
        );
    }

    #[test]
    fn condition_hit_does_not_match_partial_class_name() {
        assert!(!class_spec_matches("Facade", "RequestFacade"));
        assert!(!class_spec_matches("RequestFacade", "OtherRequestFacade"));
    }

    // ─── execute behavior tests would require a real jdb process (see integration tests) ───
}

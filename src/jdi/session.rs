//! JDI backend session backed by the Java sidecar.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::jdb::process::normalize_attach_host;
use crate::jdi::lifecycle::{
    LaunchedSidecar, launch_sidecar, resolve_sidecar_paths, resolve_sidecar_paths_for_jdb,
};
use crate::jdi::transport::{SidecarEvent, SidecarTransportError};
use crate::protocol::*;

const SIDECAR_START_TIMEOUT: Duration = Duration::from_secs(10);
const SIDECAR_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SIDECAR_BLOCKING_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT_EVENT_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_STACK_FRAMES: u32 = 24;
const ALL_STACK_FRAMES: u32 = 64;

#[derive(Debug, Clone)]
pub struct JdiSessionMeta {
    pub id: String,
    pub name: Option<String>,
    pub mode: SessionMode,
    pub backend: BackendKind,
    pub target: String,
    pub sidecar_pid: u32,
    pub created_at: Option<String>,
}

pub struct JdiSession {
    pub meta: JdiSessionMeta,
    sidecar: LaunchedSidecar,
    command_lock: Mutex<()>,
    inner: Arc<Mutex<JdiSessionInner>>,
}

#[derive(Debug)]
struct JdiSessionInner {
    state: RunState,
    last_event: Option<Event>,
    delivered_stop: Option<Event>,
    delivered_stop_ids: VecDeque<String>,
    pending_stops: VecDeque<StopPayload>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopDelivery {
    Direct,
    Async,
}

impl JdiSession {
    pub fn attach(
        host: &str,
        port: u16,
        sourcepath: &[String],
        id: String,
        name: Option<String>,
    ) -> Result<Self> {
        Self::attach_with_jdb_path(host, port, sourcepath, id, name, None)
    }

    pub fn attach_with_jdb_path(
        host: &str,
        port: u16,
        sourcepath: &[String],
        id: String,
        name: Option<String>,
        jdb_path: Option<&Path>,
    ) -> Result<Self> {
        let host = normalize_attach_host(host);
        let paths = match jdb_path {
            Some(path) => resolve_sidecar_paths_for_jdb(Some(path))?,
            None => resolve_sidecar_paths()?,
        };
        let sidecar = launch_sidecar(paths, SIDECAR_START_TIMEOUT)?;
        let target = format!("{host}:{port}");
        let params = json!({
            "session": id,
            "host": host,
            "port": port,
            "sourcepath": sourcepath,
        });
        request(&sidecar, "attach", params, SIDECAR_REQUEST_TIMEOUT)?;
        let sidecar_pid = sidecar.pid();
        let inner = Arc::new(Mutex::new(JdiSessionInner {
            state: RunState::Running,
            last_event: None,
            delivered_stop: None,
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::new(),
        }));
        register_event_sink(&sidecar, Arc::clone(&inner), id.clone());

        Ok(Self {
            meta: JdiSessionMeta {
                id,
                name,
                mode: SessionMode::Attach,
                backend: BackendKind::Jdi,
                target,
                sidecar_pid,
                created_at: Some(jiff::Zoned::now().to_string()),
            },
            sidecar,
            command_lock: Mutex::new(()),
            inner,
        })
    }

    pub fn launch(
        main_class: &str,
        classpath: &[String],
        sourcepath: &[String],
        app_args: &[String],
        id: String,
        name: Option<String>,
    ) -> Result<Self> {
        Self::launch_with_jdb_path(main_class, classpath, sourcepath, app_args, id, name, None)
    }

    pub fn launch_with_jdb_path(
        main_class: &str,
        classpath: &[String],
        sourcepath: &[String],
        app_args: &[String],
        id: String,
        name: Option<String>,
        jdb_path: Option<&Path>,
    ) -> Result<Self> {
        let paths = match jdb_path {
            Some(path) => resolve_sidecar_paths_for_jdb(Some(path))?,
            None => resolve_sidecar_paths()?,
        };
        let sidecar = launch_sidecar(paths, SIDECAR_START_TIMEOUT)?;
        let params = json!({
            "session": id,
            "mainClass": main_class,
            "classpath": classpath,
            "sourcepath": sourcepath,
            "appArgs": app_args,
        });
        request(&sidecar, "launch", params, SIDECAR_REQUEST_TIMEOUT)?;
        let sidecar_pid = sidecar.pid();
        let inner = Arc::new(Mutex::new(JdiSessionInner {
            state: RunState::Loaded,
            last_event: None,
            delivered_stop: None,
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::new(),
        }));
        register_event_sink(&sidecar, Arc::clone(&inner), id.clone());

        Ok(Self {
            meta: JdiSessionMeta {
                id,
                name,
                mode: SessionMode::Launch,
                backend: BackendKind::Jdi,
                target: main_class.to_string(),
                sidecar_pid,
                created_at: Some(jiff::Zoned::now().to_string()),
            },
            sidecar,
            command_lock: Mutex::new(()),
            inner,
        })
    }

    pub fn state(&self) -> RunState {
        self.drain_events();
        self.inner.lock().expect("jdi session mutex poisoned").state
    }

    pub fn status(&self) -> CommandResult {
        self.drain_events();
        let inner = self.inner.lock().expect("jdi session mutex poisoned");
        CommandResult::Status {
            session: self.meta.id.clone(),
            backend: self.meta.backend,
            state: inner.state,
            last_event: inner.last_event.clone(),
            pending_stops: inner.pending_stops.len(),
            jdb_alive: self.sidecar.is_alive(),
        }
    }

    pub fn kill(&self) -> Result<()> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        let method = match self.meta.mode {
            SessionMode::Launch => "terminate",
            SessionMode::Attach => "detach",
        };
        let _ = request(
            &self.sidecar,
            method,
            json!({ "session": self.meta.id }),
            SIDECAR_REQUEST_TIMEOUT,
        );
        self.sidecar.shutdown(Duration::from_secs(3))?;
        self.inner.lock().expect("jdi session mutex poisoned").state = RunState::Dead;
        Ok(())
    }

    pub fn threads(&self, filter: Option<&str>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "threads",
            json!({ "session": self.meta.id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let mut payload: ThreadsPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI threads response: {e}")))?;
        if let Some(filter) = filter.filter(|f| !f.is_empty()) {
            let needle = filter.to_lowercase();
            payload
                .threads
                .retain(|t| t.name.to_lowercase().contains(&needle));
        }
        Ok(CommandResponse {
            result: CommandResult::Threads {
                threads: payload.threads,
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn stack(&self, all: bool) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        if all {
            let value = request(
                &self.sidecar,
                "stacks",
                json!({ "session": self.meta.id, "maxFrames": ALL_STACK_FRAMES }),
                SIDECAR_REQUEST_TIMEOUT,
            )?;
            let payload: StacksPayload = serde_json::from_value(value)
                .map_err(|e| Error::Jdi(format!("invalid JDI stacks response: {e}")))?;
            return Ok(CommandResponse {
                result: CommandResult::ThreadStackTrace {
                    threads: payload.threads,
                },
                stderr: self.sidecar.take_stderr(),
                note: payload.note,
            });
        }

        let value = request(
            &self.sidecar,
            "stack",
            json!({ "session": self.meta.id, "maxFrames": DEFAULT_STACK_FRAMES }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: StackPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI stack response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::StackTrace {
                frames: payload.frames,
            },
            stderr: self.sidecar.take_stderr(),
            note: payload.note,
        })
    }

    pub fn locals(&self) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "locals",
            json!({ "session": self.meta.id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: LocalsPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI locals response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Locals { vars: payload.vars },
            stderr: self.sidecar.take_stderr(),
            note: payload.note,
        })
    }

    pub fn stop_at(
        &self,
        class: &str,
        line: u32,
        condition: Option<&str>,
        suspend: Option<&str>,
    ) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "setBreakpoint",
            json!({
                "session": self.meta.id,
                "class": class,
                "line": line,
                "condition": condition,
                "suspend": suspend.unwrap_or("all"),
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: BreakpointPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI breakpoint response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::BreakpointSet {
                spec: payload.spec,
                bp_kind: BreakpointKind::Line,
                deferred: payload.deferred,
            },
            stderr: self.sidecar.take_stderr(),
            note: payload.note,
        })
    }

    pub fn break_in(
        &self,
        class: &str,
        method: &str,
        args: Option<&str>,
        event: MethodEventKind,
        condition: Option<&str>,
        suspend: Option<&str>,
    ) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "setMethodEvent",
            json!({
                "session": self.meta.id,
                "class": class,
                "method": method,
                "args": args,
                "event": event,
                "condition": condition,
                "suspend": suspend.unwrap_or("all"),
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: BreakpointPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI method event response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::BreakpointSet {
                spec: payload.spec,
                bp_kind: BreakpointKind::Method,
                deferred: payload.deferred,
            },
            stderr: self.sidecar.take_stderr(),
            note: payload.note,
        })
    }

    pub fn watch(&self, field: &str, mode: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "setWatchpoint",
            json!({
                "session": self.meta.id,
                "field": field,
                "mode": mode,
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: WatchpointPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI watchpoint response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::WatchSet {
                spec: payload.spec,
                mode: payload.mode,
                deferred: payload.deferred,
            },
            stderr: self.sidecar.take_stderr(),
            note: payload.note,
        })
    }

    pub fn unwatch(&self, field: &str, mode: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        request(
            &self.sidecar,
            "clearWatchpoint",
            json!({
                "session": self.meta.id,
                "field": field,
                "mode": mode,
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!("Watch removed ({mode}): {field}"),
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn breakpoints(&self) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "breakpoints",
            json!({ "session": self.meta.id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: BreakpointsPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI breakpoints response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::BreakpointList {
                breakpoints: payload.breakpoints,
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn clear(&self, spec: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "clearBreakpoint",
            json!({ "session": self.meta.id, "spec": spec }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: RemovedPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI clear response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!(
                    "Removed {} breakpoint(s): {}",
                    payload.removed, payload.spec
                ),
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn catch_exception(&self, exception: &str, mode: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "catchException",
            json!({ "session": self.meta.id, "exception": exception, "mode": mode }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: BreakpointPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI catch response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::BreakpointSet {
                spec: payload.spec,
                bp_kind: BreakpointKind::Catch,
                deferred: payload.deferred,
            },
            stderr: self.sidecar.take_stderr(),
            note: payload.note,
        })
    }

    pub fn ignore_exception(&self, exception: &str, mode: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "ignoreException",
            json!({ "session": self.meta.id, "exception": exception, "mode": mode }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: RemovedPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI ignore response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!(
                    "Ignored {} catchpoint(s): {}",
                    payload.removed, payload.spec
                ),
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn cont(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.resume_like("continue", timeout)
    }

    pub fn run(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        if self.meta.mode != SessionMode::Launch {
            return Err(Error::UnsupportedBackend {
                backend: "jdi".into(),
                operation: "run on attach session".into(),
            });
        }
        {
            let inner = self.inner.lock().expect("jdi session mutex poisoned");
            if inner.state != RunState::Loaded {
                return Err(Error::Jdi(format!(
                    "JDI run is only valid before a launched VM starts; current state is {:?}",
                    inner.state
                )));
            }
        }
        self.resume_like("continue", timeout)
    }

    pub fn next(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.resume_like("stepOver", timeout)
    }

    pub fn step(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.resume_like("stepInto", timeout)
    }

    pub fn step_out(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.resume_like("stepOut", timeout)
    }

    pub fn classes(&self, pattern: Option<&str>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "classes",
            json!({ "session": self.meta.id, "pattern": pattern }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: ClassesPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI classes response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Classes {
                classes: payload.classes,
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn methods(&self, class: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "methods",
            json!({ "session": self.meta.id, "class": class }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: MethodsPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI methods response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Methods {
                class: payload.class,
                methods: payload.methods,
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn select_thread(&self, id: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        request(
            &self.sidecar,
            "selectThread",
            json!({ "session": self.meta.id, "threadId": id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!("Current JDI thread set to {id}"),
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn frame(&self, direction: &str, count: u32) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_suspended("select frame")?;
        let value = request(
            &self.sidecar,
            "selectFrame",
            json!({ "session": self.meta.id, "direction": direction, "count": count }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: TextPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI frame response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw { text: payload.text },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn list_source(&self, line: Option<u32>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_suspended("list source")?;
        let value = request(
            &self.sidecar,
            "listSource",
            json!({ "session": self.meta.id, "line": line }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: SourcePayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI source response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Source {
                around_line: payload.around_line,
                lines: payload.lines,
            },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn inspect(&self, expr: &str, max_elements: u32) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "inspect",
            json!({
                "session": self.meta.id,
                "expr": expr,
                "limits": {
                    "maxDepth": 3,
                    "maxFields": 100,
                    "maxArrayLength": max_elements.min(50),
                    "maxStringLength": 4096,
                    "maxTotalBytes": 8 * 1024 * 1024,
                    "maxObjects": 1000
                }
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Ok(CommandResponse {
            result: CommandResult::Raw { text },
            stderr: self.sidecar.take_stderr(),
            note: Some(
                "JDI inspect is read-only (structured JSON; getters/methods are not invoked). \
                 For method calls like obj.getX() use print/eval instead."
                    .into(),
            ),
        })
    }

    pub fn evaluate(&self, expr: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_event_suspended("evaluate expression")?;
        let value = request(
            &self.sidecar,
            "evaluateExpression",
            json!({
                "session": self.meta.id,
                "expr": expr,
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: EvalPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI evaluate response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Value {
                expr: payload.expr,
                value: payload.value,
                ty: payload.ty,
            },
            stderr: self.sidecar.take_stderr(),
            note: Some("JDI print/eval may invoke methods and mutate target state.".into()),
        })
    }

    pub fn dump(&self, expr: &str, max_elements: u32) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_event_suspended("dump expression")?;
        let value = request(
            &self.sidecar,
            "renderExpression",
            json!({
                "session": self.meta.id,
                "expr": expr,
                "limits": {
                    "maxDepth": 3,
                    "maxFields": 100,
                    "maxArrayLength": max_elements.min(50),
                    "maxStringLength": 4096,
                    "maxTotalBytes": 8 * 1024 * 1024,
                    "maxObjects": 1000
                }
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Ok(CommandResponse {
            result: CommandResult::Raw { text },
            stderr: self.sidecar.take_stderr(),
            note: Some("JDI dump evaluates the expression before rendering fields.".into()),
        })
    }

    pub fn set_value(&self, lvalue: &str, value_expr: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_event_suspended("set value")?;
        let value = request(
            &self.sidecar,
            "setValue",
            json!({
                "session": self.meta.id,
                "lvalue": lvalue,
                "value": value_expr,
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: SetPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI setValue response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!("{} = {}", payload.lvalue, payload.value),
            },
            stderr: self.sidecar.take_stderr(),
            note: payload
                .ty
                .map(|ty| format!("JDI setValue evaluated and assigned a {ty} value.")),
        })
    }

    pub fn force_return(&self, value_expr: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_event_suspended("force return")?;
        let value = request(
            &self.sidecar,
            "forceReturn",
            json!({
                "session": self.meta.id,
                "value": value_expr,
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: ForceReturnPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI forceReturn response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!("Forced current method to return {}", payload.value),
            },
            stderr: self.sidecar.take_stderr(),
            note: Some(
                "Current JDI frame/value references are invalid after force_return; subsequent commands re-read stop state."
                    .into(),
            ),
        })
    }

    pub fn suspend(&self, id: Option<&str>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "suspend",
            json!({ "session": self.meta.id, "threadId": id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: TextPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI suspend response: {e}")))?;
        if id.is_none() {
            self.inner.lock().expect("jdi session mutex poisoned").state = RunState::Suspended;
        }
        Ok(CommandResponse {
            result: CommandResult::Raw { text: payload.text },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn resume(&self, id: Option<&str>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.drain_events();
        let value = request(
            &self.sidecar,
            "resume",
            json!({ "session": self.meta.id, "threadId": id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: ResumePayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI resume response: {e}")))?;
        if id.is_none() {
            let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
            discard_stops_after_resume_all(&mut inner, &payload.discarded_stop_ids);
        }
        Ok(CommandResponse {
            result: CommandResult::Raw { text: payload.text },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn lock(&self, expr: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_suspended("inspect lock")?;
        let value = request(
            &self.sidecar,
            "lockInfo",
            json!({ "session": self.meta.id, "expr": expr }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: TextPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI lock response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw { text: payload.text },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn threadlocks(&self, id: Option<&str>) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_suspended("inspect thread locks")?;
        let value = request(
            &self.sidecar,
            "threadLocks",
            json!({ "session": self.meta.id, "threadId": id }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: TextPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI threadLocks response: {e}")))?;
        Ok(CommandResponse {
            result: CommandResult::Raw { text: payload.text },
            stderr: self.sidecar.take_stderr(),
            note: None,
        })
    }

    pub fn raw(&self, command: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        let command = command.trim();
        if command.is_empty() || command == "help" {
            return Ok(CommandResponse {
                result: CommandResult::Raw {
                    text: "JDI raw supports jdb-style aliases: classes, methods, where, locals, threads, print, eval, dump, clear, catch, ignore, list, step, next, step up, cont, run, suspend, resume, lock, threadlocks.".into(),
                },
                stderr: self.sidecar.take_stderr(),
                note: Some("JDI has no literal jdb command stream; raw dispatches supported aliases through the sidecar.".into()),
            });
        }

        if command == "classes" {
            return self.classes(None);
        }
        if let Some(rest) = command.strip_prefix("classes ") {
            return self.classes(Some(rest.trim()));
        }
        if let Some(rest) = command.strip_prefix("methods ") {
            return self.methods(rest.trim());
        }
        if command == "where" {
            return self.stack(false);
        }
        if command == "where all" {
            return self.stack(true);
        }
        if command == "locals" {
            return self.locals();
        }
        if command == "threads" {
            return self.threads(None);
        }
        if let Some(rest) = command.strip_prefix("thread ") {
            return self.select_thread(rest.trim());
        }
        if let Some(rest) = command.strip_prefix("print ") {
            return self.evaluate(rest.trim());
        }
        if let Some(rest) = command.strip_prefix("eval ") {
            return self.evaluate(rest.trim());
        }
        if let Some(rest) = command.strip_prefix("dump ") {
            return self.dump(rest.trim(), 10);
        }
        if let Some(rest) = command.strip_prefix("clear ") {
            return self.clear(rest.trim());
        }
        if let Some(rest) = command.strip_prefix("catch ") {
            let (mode, exception) = parse_mode_prefixed(rest.trim());
            return self.catch_exception(exception, mode);
        }
        if let Some(rest) = command.strip_prefix("ignore ") {
            let (mode, exception) = parse_mode_prefixed(rest.trim());
            return self.ignore_exception(exception, mode);
        }
        if command == "step" {
            return self.step(timeout);
        }
        if command == "next" {
            return self.next(timeout);
        }
        if command == "step up" {
            return self.step_out(timeout);
        }
        if command == "cont" {
            return self.cont(timeout);
        }
        if command == "run" {
            return self.run(timeout);
        }
        if command == "suspend" {
            return self.suspend(None);
        }
        if let Some(rest) = command.strip_prefix("suspend ") {
            return self.suspend(Some(rest.trim()));
        }
        if command == "resume" {
            return self.resume(None);
        }
        if let Some(rest) = command.strip_prefix("resume ") {
            return self.resume(Some(rest.trim()));
        }
        if let Some(rest) = command.strip_prefix("lock ") {
            return self.lock(rest.trim());
        }
        if command == "threadlocks" {
            return self.threadlocks(None);
        }
        if let Some(rest) = command.strip_prefix("threadlocks ") {
            return self.threadlocks(Some(rest.trim()));
        }
        if command == "list" {
            return self.list_source(None);
        }
        if let Some(rest) = command.strip_prefix("list ") {
            let line = rest.trim().parse::<u32>().ok();
            return self.list_source(line);
        }

        Ok(CommandResponse {
            result: CommandResult::Raw {
                text: format!("JDI raw cannot execute literal jdb command: {command}"),
            },
            stderr: self.sidecar.take_stderr(),
            note: Some("Use a first-class jdbg command or a supported JDI raw alias.".into()),
        })
    }

    fn resume_like(&self, method: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        if let Some(payload) = self.drain_events_and_take_stop_payload() {
            return self.command_response_from_stop_payload(payload, StopDelivery::Async);
        }
        {
            let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
            if matches!(inner.state, RunState::Dead | RunState::Exited) {
                return Err(Error::SessionDead(format!(
                    "session {} is {:?}",
                    self.meta.id, inner.state
                )));
            }
            inner.state = RunState::Running;
            inner.delivered_stop = None;
        }

        let timeout = timeout
            .map(Duration::from_secs)
            .unwrap_or(SIDECAR_BLOCKING_TIMEOUT);
        let value = request(
            &self.sidecar,
            method,
            json!({ "session": self.meta.id, "timeoutMs": timeout.as_millis() as u64 }),
            timeout + Duration::from_secs(2),
        )?;
        let response = self.stop_response_from_value(value, timeout)?;
        Ok(response)
    }

    fn require_suspended(&self, operation: &str) -> Result<()> {
        self.drain_events();
        let state = self.inner.lock().expect("jdi session mutex poisoned").state;
        match state {
            RunState::Suspended => Ok(()),
            RunState::Dead | RunState::Exited => Err(Error::SessionDead(format!(
                "session {} is {:?}",
                self.meta.id, state
            ))),
            other => Err(Error::Jdi(format!(
                "JDI {operation} requires a suspended stop site; current state is {other:?}"
            ))),
        }
    }

    /// JDI only permits method invocation on a thread stopped by a debugger event.
    /// `VirtualMachine.suspend()` and `ThreadReference.suspend()` make a thread look
    /// suspended, but cannot safely serve as an evaluation site.
    fn require_event_suspended(&self, operation: &str) -> Result<()> {
        self.drain_events();
        let inner = self.inner.lock().expect("jdi session mutex poisoned");
        match inner.state {
            RunState::Dead | RunState::Exited => Err(Error::SessionDead(format!(
                "session {} is {:?}",
                self.meta.id, inner.state
            ))),
            RunState::Suspended
                if inner.delivered_stop.is_some() || !inner.pending_stops.is_empty() =>
            {
                Ok(())
            }
            _ => Err(Error::Jdi(format!(
                "JDI {operation} requires an event-suspended stop; manual suspend cannot run executable expressions. Set a breakpoint in the target method and wait for Stopped."
            ))),
        }
    }

    fn stop_response_from_value(&self, value: Value, timeout: Duration) -> Result<CommandResponse> {
        if value
            .get("timedOut")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if let Some(payload) =
                self.wait_for_events_and_take_stop_payload(timeout_event_grace(timeout))
            {
                return self.command_response_from_stop_payload(payload, StopDelivery::Async);
            }
            {
                let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
                inner.state = RunState::Running;
            }
            return Ok(CommandResponse {
                result: CommandResult::Timeout {
                    partial_output: value
                        .get("partialOutput")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    state: RunState::Running,
                },
                stderr: self.sidecar.take_stderr(),
                note: None,
            });
        }

        let payload: StopPayload = serde_json::from_value(value)
            .map_err(|e| Error::Jdi(format!("invalid JDI stop response: {e}")))?;
        self.command_response_from_stop_payload(payload, StopDelivery::Direct)
    }

    fn command_response_from_stop_payload(
        &self,
        payload: StopPayload,
        delivery: StopDelivery,
    ) -> Result<CommandResponse> {
        let (event, state) = event_from_stop_payload(&payload)?;
        let ack_note = if delivery == StopDelivery::Async && !matches!(event, Event::VmExit) {
            self.acknowledge_stop_payload(&payload)
                .err()
                .map(|e| format!("WARNING: failed to acknowledge queued JDI stop: {e}"))
        } else {
            None
        };
        let note = append_optional_note(payload.note.clone(), ack_note);
        {
            let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
            finalize_delivered_stop(&mut inner, &payload, &event, state);
        }
        if matches!(event, Event::VmExit) {
            return Ok(CommandResponse {
                result: CommandResult::VmExited {
                    exit_code: None,
                    tail: payload.message,
                },
                stderr: self.sidecar.take_stderr(),
                note,
            });
        }
        if let Event::Exception {
            exception,
            caught,
            thread,
            ..
        } = event.clone()
        {
            return Ok(CommandResponse {
                result: CommandResult::ExceptionCaught {
                    exception,
                    caught,
                    location: payload.location,
                    thread,
                    thread_id: payload.thread_id,
                },
                stderr: self.sidecar.take_stderr(),
                note,
            });
        }
        Ok(CommandResponse {
            result: CommandResult::Stopped {
                event,
                location: payload.location,
                thread: payload.thread,
                thread_id: payload.thread_id,
                frame: payload.frame,
                source_context: None,
            },
            stderr: self.sidecar.take_stderr(),
            note,
        })
    }

    fn acknowledge_stop_payload(&self, payload: &StopPayload) -> Result<()> {
        request(
            &self.sidecar,
            "ackStop",
            json!({
                "session": self.meta.id,
                "event": &payload.event,
                "stopId": &payload.stop_id,
                "thread": &payload.thread,
                "threadId": &payload.thread_id,
                "location": &payload.location,
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )
        .map(|_| ())
    }

    fn drain_events(&self) {
        self.drain_transport_events();
    }

    fn drain_events_and_take_stop_payload(&self) -> Option<StopPayload> {
        self.drain_transport_events();
        let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
        inner.pending_stops.pop_front()
    }

    fn wait_for_events_and_take_stop_payload(&self, timeout: Duration) -> Option<StopPayload> {
        let events = self.sidecar.transport().wait_for_events(timeout);
        self.apply_transport_events(events);
        let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
        inner.pending_stops.pop_front()
    }

    fn drain_transport_events(&self) {
        let events = self.sidecar.transport().drain_events();
        self.apply_transport_events(events);
    }

    fn apply_transport_events(&self, events: Vec<SidecarEvent>) {
        let sidecar_alive = self.sidecar.is_alive();
        if events.is_empty() && sidecar_alive {
            return;
        }
        let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
        apply_sidecar_events(&mut inner, events, &self.meta.id, sidecar_alive);
    }
}

#[derive(Debug, Deserialize)]
struct ThreadsPayload {
    threads: Vec<ThreadInfo>,
}

#[derive(Debug, Deserialize)]
struct StackPayload {
    frames: Vec<StackFrame>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StacksPayload {
    threads: Vec<ThreadStack>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LocalsPayload {
    vars: Vec<VarBinding>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BreakpointsPayload {
    breakpoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BreakpointPayload {
    spec: String,
    deferred: bool,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WatchpointPayload {
    spec: String,
    mode: String,
    deferred: bool,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemovedPayload {
    removed: u32,
    spec: String,
}

#[derive(Debug, Deserialize)]
struct ClassesPayload {
    classes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MethodsPayload {
    class: String,
    methods: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourcePayload {
    around_line: u32,
    lines: Vec<SourceLine>,
}

#[derive(Debug, Deserialize)]
struct TextPayload {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumePayload {
    text: String,
    #[serde(default)]
    discarded_stop_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EvalPayload {
    expr: String,
    value: String,
    #[serde(default, rename = "type")]
    ty: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetPayload {
    lvalue: String,
    value: String,
    #[serde(default, rename = "type")]
    ty: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForceReturnPayload {
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopPayload {
    event: String,
    #[serde(default)]
    stop_id: Option<String>,
    location: Location,
    thread: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    frame: Option<StackFrame>,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    access_type: Option<String>,
    #[serde(default)]
    return_value: Option<String>,
    #[serde(default)]
    return_type: Option<String>,
    #[serde(default)]
    exception: Option<String>,
    #[serde(default)]
    caught: Option<bool>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

fn event_from_stop_payload(payload: &StopPayload) -> Result<(Event, RunState)> {
    match payload.event.as_str() {
        "breakpoint" => Ok((
            Event::Breakpoint {
                location: payload.location.clone(),
                thread: payload.thread.clone(),
            },
            RunState::Suspended,
        )),
        "methodEntry" => Ok((
            Event::MethodEntry {
                location: payload.location.clone(),
                thread: payload.thread.clone(),
            },
            RunState::Suspended,
        )),
        "methodExit" => Ok((
            Event::MethodExit {
                location: payload.location.clone(),
                thread: payload.thread.clone(),
                return_value: payload.return_value.clone(),
                return_type: payload.return_type.clone(),
            },
            RunState::Suspended,
        )),
        "step" => Ok((
            Event::Step {
                location: payload.location.clone(),
                thread: payload.thread.clone(),
            },
            RunState::Suspended,
        )),
        "fieldWatch" => Ok((
            Event::FieldWatch {
                field: payload.field.clone().unwrap_or_default(),
                access_type: payload.access_type.clone().unwrap_or_default(),
                thread: payload.thread.clone(),
            },
            RunState::Suspended,
        )),
        "exception" => Ok((
            Event::Exception {
                exception: payload.exception.clone().unwrap_or_default(),
                caught: payload.caught.unwrap_or(false),
                location: Some(payload.location.clone()),
                thread: payload.thread.clone(),
            },
            RunState::Suspended,
        )),
        "vmDisconnected" => Ok((Event::VmExit, RunState::Exited)),
        other => Err(Error::Jdi(format!("unsupported JDI stop event '{other}'"))),
    }
}

fn register_event_sink(
    sidecar: &LaunchedSidecar,
    inner: Arc<Mutex<JdiSessionInner>>,
    session_id: String,
) {
    sidecar.transport().set_event_sink(move |event| {
        let mut inner = inner.lock().expect("jdi session mutex poisoned");
        apply_sidecar_event(&mut inner, event, &session_id);
    });
}

fn apply_sidecar_event(inner: &mut JdiSessionInner, event: SidecarEvent, session_id: &str) {
    if event.session != session_id {
        return;
    }
    if event.event == "vmDisconnected" {
        if !matches!(inner.state, RunState::Dead) {
            inner.state = RunState::Exited;
            inner.last_event = Some(Event::VmExit);
        }
        return;
    }
    if !is_stop_event(&event.event) || matches!(inner.state, RunState::Dead | RunState::Exited) {
        return;
    }
    let Ok(payload) = serde_json::from_value::<StopPayload>(event.payload) else {
        return;
    };
    let Ok((next_event, state)) = event_from_stop_payload(&payload) else {
        return;
    };
    if is_delivered_stop(inner, &payload, &next_event)
        || inner
            .pending_stops
            .iter()
            .any(|pending| is_same_stop_payload(pending, &payload, &next_event))
    {
        return;
    }
    inner.pending_stops.push_back(payload);
    inner.state = state;
    inner.last_event = Some(next_event);
}

fn apply_sidecar_events(
    inner: &mut JdiSessionInner,
    events: Vec<SidecarEvent>,
    session_id: &str,
    sidecar_alive: bool,
) {
    for event in events {
        apply_sidecar_event(inner, event, session_id);
    }
    if !sidecar_alive && !matches!(inner.state, RunState::Dead | RunState::Exited) {
        inner.state = RunState::Dead;
    }
}

fn is_stop_event(event: &str) -> bool {
    matches!(
        event,
        "breakpoint" | "methodEntry" | "methodExit" | "step" | "fieldWatch" | "exception"
    )
}

fn is_delivered_stop(inner: &JdiSessionInner, payload: &StopPayload, event: &Event) -> bool {
    match payload.stop_id.as_deref() {
        Some(stop_id) => inner.delivered_stop_ids.iter().any(|id| id == stop_id),
        None => inner.delivered_stop.as_ref() == Some(event),
    }
}

fn is_same_stop_payload(
    existing: &StopPayload,
    incoming: &StopPayload,
    incoming_event: &Event,
) -> bool {
    if let (Some(existing_id), Some(incoming_id)) =
        (existing.stop_id.as_deref(), incoming.stop_id.as_deref())
    {
        return existing_id == incoming_id;
    }
    event_from_stop_payload(existing)
        .map(|(existing_event, _)| existing_event == *incoming_event)
        .unwrap_or(false)
}

/// Record a just-delivered stop and drop its async twin from the pending queue.
///
/// A stop is delivered on two channels sharing one `stopId`: the blocking response to
/// `continue`/step, and an async event. When the async twin is drained (by a concurrent
/// `state()` call from another client's `create_attach` dedup sweep or `persist_sessions`)
/// before we remember the stopId, it lands in `pending_stops` and would be replayed by the
/// next resume as a phantom duplicate stop. Remembering the id and purging the twin under a
/// single lock closes the race: a twin queued earlier is removed here; one queued later is
/// rejected by `is_delivered_stop`.
fn finalize_delivered_stop(
    inner: &mut JdiSessionInner,
    payload: &StopPayload,
    event: &Event,
    state: RunState,
) {
    inner.state = state;
    inner.last_event = Some(event.clone());
    inner.delivered_stop = Some(event.clone());
    remember_delivered_stop_id(inner, payload.stop_id.as_deref());
    inner
        .pending_stops
        .retain(|pending| !is_same_stop_payload(pending, payload, event));
}

fn remember_delivered_stop_id(inner: &mut JdiSessionInner, stop_id: Option<&str>) {
    let Some(stop_id) = stop_id else {
        return;
    };
    if inner.delivered_stop_ids.iter().any(|id| id == stop_id) {
        return;
    }
    inner.delivered_stop_ids.push_back(stop_id.to_string());
    while inner.delivered_stop_ids.len() > 32 {
        inner.delivered_stop_ids.pop_front();
    }
}

/// `resume` without a thread id is an explicit acknowledgement that all outstanding stop
/// notifications are no longer actionable. Preserve their ids so an event already in transit
/// cannot revive a stale pending stop after the VM has resumed.
fn discard_stops_after_resume_all(inner: &mut JdiSessionInner, discarded_stop_ids: &[String]) {
    let pending_stop_ids: Vec<_> = inner
        .pending_stops
        .drain(..)
        .filter_map(|payload| payload.stop_id)
        .collect();
    for stop_id in pending_stop_ids {
        remember_delivered_stop_id(inner, Some(&stop_id));
    }
    for stop_id in discarded_stop_ids {
        remember_delivered_stop_id(inner, Some(stop_id));
    }
    inner.state = RunState::Running;
    inner.last_event = None;
    inner.delivered_stop = None;
}

#[cfg(test)]
fn payload_to_event_for_test(payload: &StopPayload) -> Result<Event> {
    event_from_stop_payload(payload).map(|(event, _)| event)
}

fn parse_mode_prefixed(input: &str) -> (&str, &str) {
    for mode in ["caught", "uncaught", "all"] {
        if input == mode {
            return (mode, "");
        }
        if let Some(rest) = input.strip_prefix(&format!("{mode} ")) {
            return (mode, rest.trim());
        }
    }
    ("all", input)
}

fn request(
    sidecar: &LaunchedSidecar,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    sidecar
        .transport()
        .request(method, params, timeout)
        .map_err(sidecar_error)
}

fn sidecar_error(error: SidecarTransportError) -> Error {
    match error {
        SidecarTransportError::Remote { code, message } => {
            Error::Jdi(format!("JDI sidecar error {code}: {message}"))
        }
        other => Error::Jdi(format!("JDI sidecar transport failed: {other}")),
    }
}

fn append_optional_note(mut note: Option<String>, extra: Option<String>) -> Option<String> {
    if let Some(extra) = extra {
        match &mut note {
            Some(existing) => {
                existing.push('\n');
                existing.push_str(&extra);
            }
            None => note = Some(extra),
        }
    }
    note
}

fn timeout_event_grace(timeout: Duration) -> Duration {
    let tenth = timeout / 10;
    if tenth > MAX_TIMEOUT_EVENT_GRACE {
        MAX_TIMEOUT_EVENT_GRACE
    } else {
        tenth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdi_stop_payload_maps_to_existing_stopped_result() {
        let value = json!({
            "event": "breakpoint",
            "thread": "main",
            "threadId": "1",
            "location": {
                "class": "Main",
                "method": "main",
                "file": "Main.java",
                "line": 12
            },
            "frame": {
                "index": 0,
                "location": {
                    "class": "Main",
                    "method": "main",
                    "file": "Main.java",
                    "line": 12
                },
                "is_native": false
            }
        });

        let payload: StopPayload = serde_json::from_value(value).unwrap();

        assert_eq!(payload.event, "breakpoint");
        assert_eq!(payload.thread_id.as_deref(), Some("1"));
        assert_eq!(payload.location.line, 12);
        assert!(payload.frame.is_some());
    }

    #[test]
    fn drained_sidecar_stop_event_is_returned_and_updates_state() {
        let event = SidecarEvent {
            session: "s1".into(),
            seq: 1,
            event: "breakpoint".into(),
            payload: json!({
                "event": "breakpoint",
                "thread": "http-nio-8085-exec-1",
                "threadId": "42",
                "location": {
                    "class": "com.example.HomeController",
                    "method": "content",
                    "file": "HomeController.java",
                    "line": 32
                }
            }),
        };
        let mut inner = JdiSessionInner {
            state: RunState::Running,
            last_event: None,
            delivered_stop: None,
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::new(),
        };

        apply_sidecar_events(&mut inner, vec![event], "s1", true);
        let payload = inner
            .pending_stops
            .pop_front()
            .expect("stop payload should be queued");

        assert_eq!(inner.state, RunState::Suspended);
        assert_eq!(payload.thread, "http-nio-8085-exec-1");
        assert_eq!(payload.thread_id.as_deref(), Some("42"));
        assert!(matches!(
            inner.last_event,
            Some(Event::Breakpoint { ref thread, .. }) if thread == "http-nio-8085-exec-1"
        ));
    }

    #[test]
    fn delivered_sidecar_stop_event_is_not_returned_again() {
        let delivered = Event::Breakpoint {
            location: Location {
                class: "StructuredInspectTest".into(),
                method: "main".into(),
                file: Some("StructuredInspectTest.java".into()),
                line: 35,
            },
            thread: "main".into(),
        };
        let event = SidecarEvent {
            session: "s1".into(),
            seq: 2,
            event: "breakpoint".into(),
            payload: json!({
                "event": "breakpoint",
                "thread": "main",
                "threadId": "1",
                "location": {
                    "class": "StructuredInspectTest",
                    "method": "main",
                    "file": "StructuredInspectTest.java",
                    "line": 35
                }
            }),
        };
        let mut inner = JdiSessionInner {
            state: RunState::Suspended,
            last_event: Some(delivered.clone()),
            delivered_stop: Some(delivered),
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::new(),
        };

        apply_sidecar_events(&mut inner, vec![event], "s1", true);

        assert!(
            inner.pending_stops.is_empty(),
            "already delivered async stop should be treated as a duplicate"
        );
        assert_eq!(inner.state, RunState::Suspended);
        assert!(matches!(
            inner.last_event,
            Some(Event::Breakpoint { ref thread, .. }) if thread == "main"
        ));
    }

    #[test]
    fn delivered_sidecar_stop_id_is_not_returned_again() {
        let event = SidecarEvent {
            session: "s1".into(),
            seq: 2,
            event: "breakpoint".into(),
            payload: json!({
                "event": "breakpoint",
                "stopId": "stop-7",
                "thread": "main",
                "threadId": "1",
                "location": {
                    "class": "StructuredInspectTest",
                    "method": "main",
                    "file": "StructuredInspectTest.java",
                    "line": 35
                }
            }),
        };
        let mut delivered_stop_ids = VecDeque::new();
        delivered_stop_ids.push_back("stop-7".into());
        let mut inner = JdiSessionInner {
            state: RunState::Suspended,
            last_event: None,
            delivered_stop: None,
            delivered_stop_ids,
            pending_stops: VecDeque::new(),
        };

        apply_sidecar_events(&mut inner, vec![event], "s1", true);

        assert!(
            inner.pending_stops.is_empty(),
            "already delivered stopId should be treated as a duplicate"
        );
    }

    #[test]
    fn finalize_delivered_stop_purges_concurrently_queued_twin() {
        // Reproduces the four-client race at the logic layer: a concurrent state()
        // drain queued the async twin of a stop into pending_stops before the owning
        // cont could remember its stopId. Finalizing the direct delivery must drop the
        // twin so the next resume does not replay it as a phantom breakpoint.
        let payload: StopPayload = serde_json::from_value(json!({
            "event": "breakpoint",
            "stopId": "1",
            "thread": "main",
            "threadId": "1",
            "location": {
                "class": "JdiLaunchTest",
                "method": "main",
                "file": "JdiLaunchTest.java",
                "line": 9
            }
        }))
        .unwrap();
        let (event, state) = event_from_stop_payload(&payload).unwrap();

        let mut inner = JdiSessionInner {
            state: RunState::Running,
            last_event: None,
            delivered_stop: None,
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::from([payload.clone()]),
        };

        finalize_delivered_stop(&mut inner, &payload, &event, state);

        assert!(
            inner.pending_stops.is_empty(),
            "async twin of the delivered stop must be purged from pending_stops"
        );
        assert!(inner.delivered_stop_ids.iter().any(|id| id == "1"));

        // A late-arriving twin (drained after finalize) is now rejected by stopId dedup.
        let late_twin = SidecarEvent {
            session: "s1".into(),
            seq: 9,
            event: "breakpoint".into(),
            payload: json!({
                "event": "breakpoint",
                "stopId": "1",
                "thread": "main",
                "threadId": "1",
                "location": {
                    "class": "JdiLaunchTest",
                    "method": "main",
                    "file": "JdiLaunchTest.java",
                    "line": 9
                }
            }),
        };
        apply_sidecar_events(&mut inner, vec![late_twin], "s1", true);
        assert!(
            inner.pending_stops.is_empty(),
            "late twin with the delivered stopId must not be re-queued"
        );
    }

    #[test]
    fn jdi_method_exit_payload_carries_rendered_return_value() {
        let value = json!({
            "event": "methodExit",
            "thread": "main",
            "threadId": "1",
            "location": {
                "class": "Main",
                "method": "compute",
                "file": "Main.java",
                "line": 12
            },
            "returnValue": "42",
            "returnType": "int"
        });

        let payload: StopPayload = serde_json::from_value(value).unwrap();
        let event = payload_to_event_for_test(&payload).unwrap();

        match event {
            Event::MethodExit {
                return_value,
                return_type,
                ..
            } => {
                assert_eq!(return_value.as_deref(), Some("42"));
                assert_eq!(return_type.as_deref(), Some("int"));
            }
            other => panic!("expected MethodExit, got {other:?}"),
        }
    }

    #[test]
    fn jdi_async_stop_event_updates_state_and_last_event() {
        let mut inner = JdiSessionInner {
            state: RunState::Running,
            last_event: Some(Event::Breakpoint {
                location: Location {
                    class: "Old".into(),
                    method: "main".into(),
                    file: Some("Old.java".into()),
                    line: 10,
                },
                thread: "main".into(),
            }),
            delivered_stop: None,
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::new(),
        };

        apply_sidecar_event(
            &mut inner,
            SidecarEvent {
                session: "s1".into(),
                seq: 7,
                event: "breakpoint".into(),
                payload: json!({
                    "event": "breakpoint",
                    "thread": "http-nio-8085-exec-1",
                    "threadId": "42",
                    "location": {
                        "class": "HomeController",
                        "method": "content",
                        "file": "HomeController.java",
                        "line": 32
                    }
                }),
            },
            "s1",
        );

        assert_eq!(inner.state, RunState::Suspended);
        assert_eq!(
            inner.pending_stops.len(),
            1,
            "an async stop must become pending as soon as the transport reader delivers it"
        );
        match inner.last_event {
            Some(Event::Breakpoint { location, thread }) => {
                assert_eq!(location.class, "HomeController");
                assert_eq!(location.line, 32);
                assert_eq!(thread, "http-nio-8085-exec-1");
            }
            other => panic!("expected async breakpoint event, got {other:?}"),
        }
    }

    #[test]
    fn resume_all_discards_pending_stops_and_clears_last_event() {
        let raw_payload = json!({
            "event": "breakpoint",
            "stopId": "stale-stop",
            "thread": "worker",
            "threadId": "7",
            "location": {
                "class": "Controller",
                "method": "handle",
                "file": "Controller.java",
                "line": 42
            }
        });
        let payload: StopPayload = serde_json::from_value(raw_payload.clone()).unwrap();
        let (event, _) = event_from_stop_payload(&payload).unwrap();
        let mut inner = JdiSessionInner {
            state: RunState::Suspended,
            last_event: Some(event.clone()),
            delivered_stop: Some(event),
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::from([payload.clone()]),
        };

        discard_stops_after_resume_all(&mut inner, &["sidecar-only-stop".into()]);

        assert_eq!(inner.state, RunState::Running);
        assert!(inner.last_event.is_none());
        assert!(inner.delivered_stop.is_none());
        assert!(inner.pending_stops.is_empty());
        assert!(inner.delivered_stop_ids.iter().any(|id| id == "stale-stop"));
        assert!(
            inner
                .delivered_stop_ids
                .iter()
                .any(|id| id == "sidecar-only-stop")
        );

        apply_sidecar_event(
            &mut inner,
            SidecarEvent {
                session: "s1".into(),
                seq: 9,
                event: "breakpoint".into(),
                payload: raw_payload,
            },
            "s1",
        );
        assert!(
            inner.pending_stops.is_empty(),
            "a late discarded event must stay ignored"
        );
        assert!(inner.last_event.is_none());
    }

    #[test]
    fn jdi_async_stop_event_ignores_other_sessions() {
        let mut inner = JdiSessionInner {
            state: RunState::Running,
            last_event: None,
            delivered_stop: None,
            delivered_stop_ids: VecDeque::new(),
            pending_stops: VecDeque::new(),
        };

        apply_sidecar_event(
            &mut inner,
            SidecarEvent {
                session: "other".into(),
                seq: 1,
                event: "breakpoint".into(),
                payload: json!({
                    "event": "breakpoint",
                    "thread": "main",
                    "threadId": "1",
                    "location": {
                        "class": "Main",
                        "method": "main",
                        "file": "Main.java",
                        "line": 12
                    }
                }),
            },
            "s1",
        );

        assert_eq!(inner.state, RunState::Running);
        assert!(inner.last_event.is_none());
    }
}

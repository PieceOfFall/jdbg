//! JDI backend session backed by the Java sidecar.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::jdb::process::normalize_attach_host;
use crate::jdi::lifecycle::{LaunchedSidecar, launch_sidecar, resolve_sidecar_paths};
use crate::jdi::transport::SidecarTransportError;
use crate::protocol::*;

const SIDECAR_START_TIMEOUT: Duration = Duration::from_secs(10);
const SIDECAR_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SIDECAR_BLOCKING_TIMEOUT: Duration = Duration::from_secs(30);

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
    inner: Mutex<JdiSessionInner>,
}

#[derive(Debug)]
struct JdiSessionInner {
    state: RunState,
    last_event: Option<Event>,
}

impl JdiSession {
    pub fn attach(
        host: &str,
        port: u16,
        sourcepath: &[String],
        id: String,
        name: Option<String>,
    ) -> Result<Self> {
        let host = normalize_attach_host(host);
        let sidecar = launch_sidecar(resolve_sidecar_paths()?, SIDECAR_START_TIMEOUT)?;
        let target = format!("{host}:{port}");
        let params = json!({
            "session": id,
            "host": host,
            "port": port,
            "sourcepath": sourcepath,
        });
        request(&sidecar, "attach", params, SIDECAR_REQUEST_TIMEOUT)?;
        let sidecar_pid = sidecar.pid();

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
            inner: Mutex::new(JdiSessionInner {
                state: RunState::Running,
                last_event: None,
            }),
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
        let sidecar = launch_sidecar(resolve_sidecar_paths()?, SIDECAR_START_TIMEOUT)?;
        let params = json!({
            "session": id,
            "mainClass": main_class,
            "classpath": classpath,
            "sourcepath": sourcepath,
            "appArgs": app_args,
        });
        request(&sidecar, "launch", params, SIDECAR_REQUEST_TIMEOUT)?;
        let sidecar_pid = sidecar.pid();

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
            inner: Mutex::new(JdiSessionInner {
                state: RunState::Loaded,
                last_event: None,
            }),
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
            .map_err(|e| Error::Connection(format!("invalid JDI threads response: {e}")))?;
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
                json!({ "session": self.meta.id, "maxFrames": 64 }),
                SIDECAR_REQUEST_TIMEOUT,
            )?;
            let payload: StacksPayload = serde_json::from_value(value)
                .map_err(|e| Error::Connection(format!("invalid JDI stacks response: {e}")))?;
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
            json!({ "session": self.meta.id, "maxFrames": 64 }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: StackPayload = serde_json::from_value(value)
            .map_err(|e| Error::Connection(format!("invalid JDI stack response: {e}")))?;
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
            .map_err(|e| Error::Connection(format!("invalid JDI locals response: {e}")))?;
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
                "suspend": suspend.unwrap_or("all"),
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: BreakpointPayload = serde_json::from_value(value)
            .map_err(|e| Error::Connection(format!("invalid JDI breakpoint response: {e}")))?;
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
                "suspend": suspend.unwrap_or("all"),
            }),
            SIDECAR_REQUEST_TIMEOUT,
        )?;
        let payload: BreakpointPayload = serde_json::from_value(value)
            .map_err(|e| Error::Connection(format!("invalid JDI method event response: {e}")))?;
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
            .map_err(|e| Error::Connection(format!("invalid JDI watchpoint response: {e}")))?;
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
                return Err(Error::Connection(format!(
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
                "JDI inspect returns a structured JSON value; getters are not invoked.".into(),
            ),
        })
    }

    pub fn evaluate(&self, expr: &str) -> Result<CommandResponse> {
        let _guard = self
            .command_lock
            .lock()
            .expect("jdi command mutex poisoned");
        self.require_suspended("evaluate expression")?;
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
            .map_err(|e| Error::Connection(format!("invalid JDI evaluate response: {e}")))?;
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
        self.require_suspended("dump expression")?;
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
        self.require_suspended("set value")?;
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
            .map_err(|e| Error::Connection(format!("invalid JDI setValue response: {e}")))?;
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
        self.require_suspended("force return")?;
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
            .map_err(|e| Error::Connection(format!("invalid JDI forceReturn response: {e}")))?;
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

    pub fn unsupported(&self, operation: &str) -> Error {
        Error::UnsupportedBackend {
            backend: "jdi".into(),
            operation: operation.into(),
        }
    }

    fn resume_like(&self, method: &str, timeout: Option<u64>) -> Result<CommandResponse> {
        self.drain_events();
        {
            let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
            if matches!(inner.state, RunState::Dead | RunState::Exited) {
                return Err(Error::SessionDead(format!(
                    "session {} is {:?}",
                    self.meta.id, inner.state
                )));
            }
            inner.state = RunState::Running;
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
        let response = self.stop_response_from_value(value)?;
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
            other => Err(Error::Connection(format!(
                "JDI {operation} requires a suspended stop site; current state is {other:?}"
            ))),
        }
    }

    fn stop_response_from_value(&self, value: Value) -> Result<CommandResponse> {
        if value
            .get("timedOut")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
            inner.state = RunState::Running;
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
            .map_err(|e| Error::Connection(format!("invalid JDI stop response: {e}")))?;
        let (event, state) = event_from_stop_payload(&payload)?;
        {
            let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
            inner.state = state;
            inner.last_event = Some(event.clone());
        }
        if matches!(event, Event::VmExit) {
            return Ok(CommandResponse {
                result: CommandResult::VmExited {
                    exit_code: None,
                    tail: payload.message,
                },
                stderr: self.sidecar.take_stderr(),
                note: None,
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
            note: payload.note,
        })
    }

    fn drain_events(&self) {
        let events = self.sidecar.transport().drain_events();
        let sidecar_alive = self.sidecar.is_alive();
        if events.is_empty() && sidecar_alive {
            return;
        }
        let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
        for event in events {
            if event.event == "vmDisconnected"
                && event.session == self.meta.id
                && !matches!(inner.state, RunState::Dead)
            {
                inner.state = RunState::Exited;
                inner.last_event = Some(Event::VmExit);
            }
        }
        if !sidecar_alive && !matches!(inner.state, RunState::Dead | RunState::Exited) {
            inner.state = RunState::Dead;
        }
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopPayload {
    event: String,
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
        "vmDisconnected" => Ok((Event::VmExit, RunState::Exited)),
        other => Err(Error::Connection(format!(
            "unsupported JDI stop event '{other}'"
        ))),
    }
}

#[cfg(test)]
fn payload_to_event_for_test(payload: &StopPayload) -> Result<Event> {
    event_from_stop_payload(payload).map(|(event, _)| event)
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
            Error::Connection(format!("JDI sidecar error {code}: {message}"))
        }
        other => Error::Connection(other.to_string()),
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
}

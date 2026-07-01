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
            inner: Mutex::new(JdiSessionInner {
                state: RunState::Running,
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
        let _ = request(
            &self.sidecar,
            "detach",
            json!({ "session": self.meta.id }),
            SIDECAR_REQUEST_TIMEOUT,
        );
        self.sidecar.shutdown(Duration::from_secs(3))?;
        self.inner.lock().expect("jdi session mutex poisoned").state = RunState::Dead;
        Ok(())
    }

    pub fn threads(&self, filter: Option<&str>) -> Result<CommandResponse> {
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

    pub fn cont(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.resume_like("continue", timeout)
    }

    pub fn next(&self, timeout: Option<u64>) -> Result<CommandResponse> {
        self.resume_like("stepOver", timeout)
    }

    pub fn select_thread(&self, id: &str) -> Result<CommandResponse> {
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
        let (event, state) = match payload.event.as_str() {
            "breakpoint" => (
                Event::Breakpoint {
                    location: payload.location.clone(),
                    thread: payload.thread.clone(),
                },
                RunState::Suspended,
            ),
            "step" => (
                Event::Step {
                    location: payload.location.clone(),
                    thread: payload.thread.clone(),
                },
                RunState::Suspended,
            ),
            "vmDisconnected" => (Event::VmExit, RunState::Exited),
            other => {
                return Err(Error::Connection(format!(
                    "unsupported JDI stop event '{other}'"
                )));
            }
        };
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
        if events.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().expect("jdi session mutex poisoned");
        for event in events {
            if event.event == "vmDisconnected" && event.session == self.meta.id {
                inner.state = RunState::Exited;
                inner.last_event = Some(Event::VmExit);
            }
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
    message: Option<String>,
    #[serde(default)]
    note: Option<String>,
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
}

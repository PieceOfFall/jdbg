//! Tool catalog and translation layer: the sole mapping point between MCP `tools/call` and [`crate::protocol::Command`].
//!
//! Each jdbg subcommand maps to one MCP tool (fine-grained 1:1). [`tool_specs`] is used by `tools/list`;
//! [`dispatch_tool`] translates (tool name, arguments) into a [`Request`], matching the existing
//! `main.rs::build_command` conversions (for example `classpath` string→`Vec`, single-string `raw`,
//! and default values).

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::jsonrpc::{INVALID_PARAMS, JsonRpcError, METHOD_NOT_FOUND};
use crate::path_args::{classpath_or_current, sourcepath_or_current};
use crate::protocol::{BackendKind, Command, MethodEventKind, Request};

/// Public description of a tool, serialized into `tools/list`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Specs for all 37 tools, used by `tools/list`.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        // ── Sessions ──
        tool(
            "launch",
            "Launch a Java program under the debugger. Returns a session in state 'loaded' \
             (JVM not started yet) — set breakpoints, then call `run`.",
            json!({
                "main_class": {"type": "string", "description": "Fully-qualified main class, e.g. com.example.Main."},
                "backend": {"type": "string", "enum": ["jdb", "jdi"], "description": "Debug backend (default jdi; falls back to jdb only when default JDI is unavailable locally). Backend is selected only at session creation."},
                "classpath": {"type": "string", "description": "Classpath (OS-separated entries)."},
                "sourcepath": {"type": "string", "description": "Source path; enables `list_source`/`locals` line info."},
                "app_args": {"type": "array", "items": {"type": "string"}, "description": "Arguments passed to the program's main()."},
                "jdb_args": {"type": "array", "items": {"type": "string"}, "description": "Extra raw arguments passed to jdb."},
                "name": {"type": "string", "description": "Optional session display name."},
                "jdb_path": {"type": "string", "description": "Override path to the jdb executable. For JDI, this also selects the sidecar JDK."}
            }),
            &["main_class"],
            false,
            false,
        ),
        tool(
            "attach",
            "Attach to an already-running JVM started with JDWP. The jdb backend returns state 'suspended'; \
             the JDI backend returns state 'running'. Either way, set breakpoints, then call `cont` \
             (attach has no `run`).",
            json!({
                "host": {"type": "string", "description": "Target host (default localhost). 'localhost' is auto-normalized to 127.0.0.1 (IPv4 loopback) because on dual-stack hosts it may resolve to IPv6 [::1] while JDWP listens only on IPv4; pass '::1' to force IPv6."},
                "port": {"type": "integer", "description": "Target JDWP port (default 5005)."},
                "backend": {"type": "string", "enum": ["jdb", "jdi"], "description": "Debug backend (default jdi; falls back to jdb only when default JDI is unavailable locally). Backend is selected only at session creation."},
                "sourcepath": {"type": "string", "description": "Source path for line info."},
                "name": {"type": "string", "description": "Optional session display name."},
                "jdb_path": {"type": "string", "description": "Override path to the jdb executable. For JDI, this also selects the sidecar JDK."}
            }),
            &[],
            false,
            false,
        ),
        tool(
            "status",
            "Report a session's run state, mode, target, and last event (sends no jdb command).",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "list",
            "List all active debug sessions.",
            json!({}),
            &[],
            false,
            false,
        ),
        tool(
            "kill",
            "End a debug session (defaults to the sole session if exactly one exists).",
            json!({}),
            &[],
            true,
            false,
        ),
        // ── Breakpoints ──
        tool(
            "break_at",
            "Set a line breakpoint at Class:line. Execution stops BEFORE this line runs (the line \
             has not yet executed). The actual hit line may differ if the requested line has no \
             executable bytecode (JVM rounds to the nearest valid line). Breakpoints set before \
             the class loads are deferred and bind on run/cont. Use `condition` to only stop when \
             a boolean expression is true. Use `suspend: \"thread\"` to only suspend the hit thread \
             (like IDEA's thread breakpoint — keeps ZK/heartbeat threads alive).",
            json!({
                "class": {"type": "string", "description": "Class name, e.g. com.example.Main."},
                "line": {"type": "integer", "description": "Source line number (must hold executable code)."},
                "condition": {"type": "string", "description": "Optional boolean expression — breakpoint only fires when true (e.g. \"i > 5\", \"name.equals(\\\"test\\\")\")."},
                "suspend": {"type": "string", "enum": ["all", "thread"], "description": "Suspend policy: 'all' freezes the entire JVM (default), 'thread' only suspends the hit thread (keeps heartbeat/ZK alive — use when debugging a live server)."}
            }),
            &["class", "line"],
            true,
            false,
        ),
        tool(
            "break_in",
            "Set a method breakpoint at Class.method. Use event='entry' (default), 'exit', or 'both'. \
             Use `args` (comma-separated param types) to disambiguate overloads. Use `condition` to only stop when a boolean expression is true. \
             Use `suspend: \"thread\"` to only suspend the hit thread (keeps heartbeat threads alive).",
            json!({
                "class": {"type": "string", "description": "Class name."},
                "method": {"type": "string", "description": "Method name."},
                "event": {"type": "string", "enum": ["entry", "exit", "both"], "description": "Method event to stop on (default entry). JDI supports entry, exit, and both; jdb supports entry only."},
                "args": {"type": "string", "description": "Parameter types for overload disambiguation, e.g. 'int,java.lang.String'."},
                "condition": {"type": "string", "description": "Optional boolean expression — breakpoint only fires when true."},
                "suspend": {"type": "string", "enum": ["all", "thread"], "description": "Suspend policy: 'all' freezes the entire JVM (default), 'thread' only suspends the hit thread."}
            }),
            &["class", "method"],
            true,
            false,
        ),
        tool(
            "catch",
            "Break when an exception is thrown.",
            json!({
                "exception": {"type": "string", "description": "Exception class, e.g. java.lang.NullPointerException."},
                "mode": {"type": "string", "enum": ["caught", "uncaught", "all"], "description": "Which throws to catch (default all)."}
            }),
            &["exception"],
            true,
            false,
        ),
        tool(
            "watch",
            "Set a field watchpoint — execution stops when the field is accessed or modified. \
             Use mode 'modification' (default) to catch writes, 'access' for reads, 'all' for both. \
             The field spec is Class.field (fully-qualified). Use `classes` to find the exact class name first.",
            json!({
                "field": {"type": "string", "description": "Field spec: fully-qualified Class.field, e.g. com.example.Service.name."},
                "mode": {"type": "string", "enum": ["access", "modification", "all"], "description": "Watch mode (default: modification)."}
            }),
            &["field"],
            true,
            false,
        ),
        tool(
            "unwatch",
            "Remove a field watchpoint. The mode must match how it was set: \
             'modification' (default) removes a write watchpoint, 'access' removes a read watchpoint, \
             'all' removes a combined watchpoint.",
            json!({
                "field": {"type": "string", "description": "Field spec to unwatch, e.g. com.example.Service.name."},
                "mode": {"type": "string", "enum": ["access", "modification", "all"], "description": "Watch mode to remove (default: modification). Must match the mode used when setting the watchpoint."}
            }),
            &["field"],
            true,
            false,
        ),
        tool(
            "breakpoints",
            "List the currently set breakpoints.",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "clear",
            "Remove a breakpoint by spec (Class:line or Class.method).",
            json!({"spec": {"type": "string", "description": "Breakpoint spec to clear, e.g. Main:42 or Main.foo."}}),
            &["spec"],
            true,
            false,
        ),
        // ── Execution control (blocking, larger default timeout) ──
        tool(
            "run",
            "Start the debugged application (launch mode only). Blocks until a breakpoint, exception, or exit. Returns the stop location with source context and top stack frame when available.",
            json!({}),
            &[],
            true,
            true,
        ),
        tool(
            "cont",
            "Continue execution until the next stop (breakpoint, exception, or program exit). Returns the stop location with source context and top stack frame when available.",
            json!({}),
            &[],
            true,
            true,
        ),
        tool(
            "step",
            "Step into the next line, entering called methods. Returns the stop location with source context and top stack frame when available.",
            json!({}),
            &[],
            true,
            true,
        ),
        tool(
            "next",
            "Step over the next line, without entering called methods. Returns the stop location with source context and top stack frame when available.",
            json!({}),
            &[],
            true,
            true,
        ),
        tool(
            "step_out",
            "Run until the current method returns. Returns the stop location with source context and top stack frame when available.",
            json!({}),
            &[],
            true,
            true,
        ),
        // ── Inspection (fast) ──
        tool(
            "where",
            "Show the current thread's call stack. Set all=true for every thread's stack.",
            json!({"all": {"type": "boolean", "description": "Show the stack of every thread."}}),
            &[],
            true,
            false,
        ),
        tool(
            "locals",
            "Show local variables in the current frame (requires classes compiled with `javac -g`).",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "print",
            "Evaluate an expression and show its value (can call methods on live objects).",
            json!({"expr": {"type": "string", "description": "Expression to evaluate."}}),
            &["expr"],
            true,
            false,
        ),
        tool(
            "dump",
            "Dump all fields of an object.",
            json!({"expr": {"type": "string", "description": "Object expression to dump."}}),
            &["expr"],
            true,
            false,
        ),
        tool(
            "eval",
            "Evaluate an expression (alias of print).",
            json!({"expr": {"type": "string", "description": "Expression to evaluate."}}),
            &["expr"],
            true,
            false,
        ),
        tool(
            "threads",
            "List all threads with id, name, group, and state. Optionally filter by a \
             case-insensitive substring of the thread name (e.g. \"http-nio\") to cut through \
             the noise of a large app's thread list. The hit thread (if any) is marked with `*`.",
            json!({
                "filter": {"type": "string", "description": "Case-insensitive substring of the thread name to filter by (e.g. \"http-nio\", \"worker\"). Omit to list all threads."}
            }),
            &[],
            true,
            false,
        ),
        tool(
            "classes",
            "Search loaded classes by substring pattern. Without a pattern lists ALL loaded classes \
             (can be thousands — always pass a pattern). Use this to find CGLIB proxies, inner classes, \
             or confirm a class is loaded before setting breakpoints.",
            json!({
                "pattern": {"type": "string", "description": "Case-sensitive substring filter (e.g. 'Service', 'EnhancerBySpringCGLIB', 'Controller')."}
            }),
            &[],
            true,
            false,
        ),
        tool(
            "methods",
            "List all methods of a loaded class (with parameter types). Use after `classes` to find \
             the exact method signature for `break_in`.",
            json!({
                "class": {"type": "string", "description": "Fully-qualified class name (from `classes` output)."}
            }),
            &["class"],
            true,
            false,
        ),
        tool(
            "thread",
            "Switch the current thread.",
            json!({"id": {"type": "string", "description": "Thread id — the full hex value WITH 0x prefix from `threads` output, e.g. \"0x37f2\". Not the thread name."}}),
            &["id"],
            true,
            false,
        ),
        tool(
            "frame",
            "Move within the current thread's call stack.",
            json!({
                "direction": {"type": "string", "enum": ["up", "down"], "description": "Move up (toward callers) or down (toward callees)."},
                "n": {"type": "integer", "description": "Number of frames to move (default 1)."}
            }),
            &["direction"],
            true,
            false,
        ),
        tool(
            "list_source",
            "Show source code around the current location, or around a given line.",
            json!({"line": {"type": "integer", "description": "Center on this line (default: current location)."}}),
            &[],
            true,
            false,
        ),
        tool(
            "inspect",
            "Inspect a collection, array, or list: shows its size and first N elements. \
             Works with ArrayList, HashMap.keySet()/values(), arrays, and any object with \
             .size()/.length + .get(i)/[i] accessors.",
            json!({
                "expr": {"type": "string", "description": "Collection expression to inspect."},
                "max_elements": {"type": "integer", "description": "Max elements to show (default 10, max 50)."}
            }),
            &["expr"],
            true,
            false,
        ),
        tool(
            "raw",
            "Escape hatch: on JDI sessions, dispatch supported jdb-style aliases; on jdb sessions, send \
             a literal jdb command (monitor, fields, methods, classes, redefine, trace, …).",
            json!({"command": {"type": "string", "description": "Raw command string. Use backend='jdb' when you need literal jdb stdin passthrough."}}),
            &["command"],
            true,
            false,
        ),
        tool(
            "suspend",
            "Suspend a thread by id, or all threads if no id is given. Pairs with `resume` for \
             fine-grained thread control (e.g. freezing one worker while inspecting a race).",
            json!({"id": {"type": "string", "description": "Thread id from `threads` output. Omit to suspend all threads."}}),
            &[],
            true,
            false,
        ),
        tool(
            "resume",
            "Resume a thread by id, or all threads if no id is given. Unlike `cont` (which resumes \
             the whole VM from a breakpoint and waits for the next stop), `resume` just clears a \
             prior `suspend` and returns immediately.",
            json!({"id": {"type": "string", "description": "Thread id from `threads` output. Omit to resume all threads."}}),
            &[],
            true,
            false,
        ),
        tool(
            "set",
            "Assign a value to a variable, field, or array element in the suspended frame — \
             MUTATES live program state. Use to test a fix hypothesis or force a branch before \
             continuing. e.g. lvalue \"this.count\", value \"42\".",
            json!({
                "lvalue": {"type": "string", "description": "Left-hand side: local var, field, or array element (e.g. \"x\", \"this.count\", \"arr[0]\")."},
                "value": {"type": "string", "description": "Right-hand side expression (e.g. \"42\", \"\\\"hello\\\"\", \"null\")."}
            }),
            &["lvalue", "value"],
            true,
            false,
        ),
        tool(
            "force_return",
            "Force the current method to return a value in a JDI session. MUTATES control flow; \
             use only when intentionally testing a branch or bypassing a failing method.",
            json!({
                "value": {"type": "string", "description": "Return expression to evaluate in the current suspended frame."}
            }),
            &["value"],
            true,
            false,
        ),
        tool(
            "ignore",
            "Stop catching an exception — removes a breakpoint previously set with `catch`. \
             The mode must match how it was caught.",
            json!({
                "exception": {"type": "string", "description": "Exception class name or pattern (as passed to `catch`)."},
                "mode": {"type": "string", "enum": ["caught", "uncaught", "all"], "description": "Must match the mode used with `catch` (default: all)."}
            }),
            &["exception"],
            true,
            false,
        ),
        tool(
            "lock",
            "Show monitor/lock info for an object: which thread owns its monitor and which threads \
             are waiting on it. Useful for diagnosing contention and deadlocks.",
            json!({"expr": {"type": "string", "description": "Object expression to inspect the monitor of."}}),
            &["expr"],
            true,
            false,
        ),
        tool(
            "threadlocks",
            "Show the locks a thread currently owns and the monitor it is blocked on — the core \
             command for deadlock diagnosis. Omit id for the current thread.",
            json!({"id": {"type": "string", "description": "Thread id from `threads` output. Omit for the current thread."}}),
            &[],
            true,
            false,
        ),
    ]
}

/// Translate one `tools/call` (tool name + arguments) into a [`Request`] sent to the daemon.
pub fn dispatch_tool(name: &str, args: &Value) -> Result<Request, JsonRpcError> {
    let session = optional_str(args, "session");
    let timeout = args.get("timeout").and_then(Value::as_u64);

    let cmd = match name {
        "launch" => {
            let (backend, backend_explicit) = optional_backend(args)?;
            Command::Launch {
                main_class: require_str(args, "main_class")?,
                backend,
                backend_explicit,
                classpath: classpath_or_current(optional_str(args, "classpath").as_deref()),
                sourcepath: sourcepath_or_current(optional_str(args, "sourcepath").as_deref()),
                app_args: optional_str_array(args, "app_args"),
                jdb_args: optional_str_array(args, "jdb_args"),
                name: optional_str(args, "name"),
                jdb_path: optional_str(args, "jdb_path"),
            }
        }
        "attach" => {
            let (backend, backend_explicit) = optional_backend(args)?;
            Command::Attach {
                backend,
                backend_explicit,
                host: optional_str(args, "host").unwrap_or_else(|| "localhost".to_string()),
                port: optional_u16(args, "port").unwrap_or(5005),
                sourcepath: sourcepath_or_current(optional_str(args, "sourcepath").as_deref()),
                name: optional_str(args, "name"),
                jdb_path: optional_str(args, "jdb_path"),
            }
        }
        "status" => Command::Status,
        "list" => Command::List,
        "kill" => Command::Kill,
        "break_at" => Command::BreakAt {
            class: require_str(args, "class")?,
            line: require_u32(args, "line")?,
            condition: optional_str(args, "condition"),
            suspend: optional_str(args, "suspend"),
        },
        "break_in" => Command::BreakIn {
            class: require_str(args, "class")?,
            method: require_str(args, "method")?,
            event: optional_method_event(args)?,
            args: optional_str(args, "args"),
            condition: optional_str(args, "condition"),
            suspend: optional_str(args, "suspend"),
        },
        "catch" => Command::Catch {
            exception: require_str(args, "exception")?,
            mode: optional_str(args, "mode").unwrap_or_else(|| "all".to_string()),
        },
        "watch" => Command::Watch {
            field: require_str(args, "field")?,
            mode: optional_str(args, "mode").unwrap_or_else(|| "modification".to_string()),
        },
        "unwatch" => Command::Unwatch {
            field: require_str(args, "field")?,
            mode: optional_str(args, "mode").unwrap_or_else(|| "modification".to_string()),
        },
        "breakpoints" => Command::Breakpoints,
        "clear" => Command::Clear {
            spec: require_str(args, "spec")?,
        },
        "run" => Command::Run,
        "cont" => Command::Cont,
        "step" => Command::Step,
        "next" => Command::Next,
        "step_out" => Command::StepOut,
        "where" => Command::Where {
            all: optional_bool(args, "all"),
        },
        "locals" => Command::Locals,
        "print" => Command::Print {
            expr: require_str(args, "expr")?,
        },
        "dump" => Command::Dump {
            expr: require_str(args, "expr")?,
        },
        "eval" => Command::Eval {
            expr: require_str(args, "expr")?,
        },
        "threads" => Command::Threads {
            filter: optional_str(args, "filter"),
        },
        "classes" => Command::Classes {
            pattern: optional_str(args, "pattern"),
        },
        "methods" => Command::Methods {
            class: require_str(args, "class")?,
        },
        "thread" => Command::Thread {
            id: require_str(args, "id")?,
        },
        "frame" => Command::Frame {
            direction: require_str(args, "direction")?,
            n: optional_u32(args, "n").unwrap_or(1),
        },
        "list_source" => Command::ListSource {
            line: optional_u32(args, "line"),
        },
        "inspect" => Command::Inspect {
            expr: require_str(args, "expr")?,
            max_elements: optional_u32(args, "max_elements").unwrap_or(10),
        },
        "raw" => Command::Raw {
            command: require_str(args, "command")?,
        },
        "suspend" => Command::Suspend {
            id: optional_str(args, "id"),
        },
        "resume" => Command::Resume {
            id: optional_str(args, "id"),
        },
        "set" => Command::Set {
            lvalue: require_str(args, "lvalue")?,
            value: require_str(args, "value")?,
        },
        "force_return" => Command::ForceReturn {
            value: require_str(args, "value")?,
        },
        "ignore" => Command::Ignore {
            exception: require_str(args, "exception")?,
            mode: optional_str(args, "mode").unwrap_or_else(|| "all".to_string()),
        },
        "lock" => Command::Lock {
            expr: require_str(args, "expr")?,
        },
        "threadlocks" => Command::ThreadLocks {
            id: optional_str(args, "id"),
        },
        _ => {
            return Err(JsonRpcError::new(
                METHOD_NOT_FOUND,
                format!("unknown tool: {name}"),
            ));
        }
    };

    Ok(Request::new(cmd, session).with_timeout(timeout))
}

// ── Internal helpers ───────────────────────────────────────────────────────────

/// Build a tool spec, automatically injecting common `session` / `timeout` properties when needed.
fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
    session: bool,
    timeout: bool,
) -> ToolSpec {
    let mut props: Map<String, Value> = properties.as_object().cloned().unwrap_or_default();
    if session {
        props.insert(
            "session".into(),
            json!({"type": "string", "description": "Target session id; omit when exactly one session exists."}),
        );
    }
    if timeout {
        props.insert(
            "timeout".into(),
            json!({"type": "integer", "description": "Per-command timeout in seconds (overrides the default)."}),
        );
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    ToolSpec {
        name,
        description,
        input_schema: Value::Object(schema),
    }
}

/// Read a JSON value as a string, coercing numbers and booleans to their text
/// form. LLM clients routinely emit a bare number for a string-typed parameter
/// (e.g. a thread id as `582` instead of `"582"`); rejecting that would surface
/// a confusing "missing required string parameter" error. Objects, arrays, and
/// null are not coercible and yield `None`.
fn coerce_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn require_str(args: &Value, key: &str) -> Result<String, JsonRpcError> {
    args.get(key).and_then(coerce_str).ok_or_else(|| {
        JsonRpcError::new(
            INVALID_PARAMS,
            format!("missing required string parameter: {key}"),
        )
    })
}

fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(coerce_str)
}

fn require_u32(args: &Value, key: &str) -> Result<u32, JsonRpcError> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .ok_or_else(|| {
            JsonRpcError::new(
                INVALID_PARAMS,
                format!("missing required integer parameter: {key}"),
            )
        })
}

fn optional_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(Value::as_u64).map(|n| n as u32)
}

fn optional_u16(args: &Value, key: &str) -> Option<u16> {
    args.get(key).and_then(Value::as_u64).map(|n| n as u16)
}

fn optional_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn optional_backend(args: &Value) -> Result<(BackendKind, bool), JsonRpcError> {
    match optional_str(args, "backend") {
        Some(raw) => raw
            .parse()
            .map(|backend| (backend, true))
            .map_err(|e: String| {
                JsonRpcError::new(INVALID_PARAMS, format!("invalid backend: {e}"))
            }),
        None => Ok((BackendKind::default(), false)),
    }
}

fn optional_method_event(args: &Value) -> Result<MethodEventKind, JsonRpcError> {
    match optional_str(args, "event") {
        Some(raw) => raw.parse().map_err(|e: String| {
            JsonRpcError::new(INVALID_PARAMS, format!("invalid method event: {e}"))
        }),
        None => Ok(MethodEventKind::Entry),
    }
}

fn optional_str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BackendKind, Command};
    use serde_json::json;

    #[test]
    fn exposes_37_tools() {
        assert_eq!(tool_specs().len(), 37);
    }

    #[test]
    fn every_schema_is_object_and_names_unique() {
        let specs = tool_specs();
        let mut names = std::collections::HashSet::new();
        for s in &specs {
            assert_eq!(
                s.input_schema["type"],
                json!("object"),
                "tool {} schema not object",
                s.name
            );
            assert!(names.insert(s.name), "duplicate tool name: {}", s.name);
        }
    }

    #[test]
    fn launch_absolutizes_classpath() {
        let req =
            dispatch_tool("launch", &json!({"main_class": "Main", "classpath": "out"})).unwrap();
        match req.cmd {
            Command::Launch {
                main_class,
                classpath,
                ..
            } => {
                assert_eq!(main_class, "Main");
                // Relative classpath entries are absolutized against the CLI cwd so
                // the launch does not depend on the long-lived daemon's directory.
                assert_eq!(classpath, classpath_or_current(Some("out")));
                assert!(
                    classpath
                        .iter()
                        .all(|p| std::path::Path::new(p).is_absolute()),
                    "classpath entries should be absolute: {classpath:?}"
                );
            }
            other => panic!("expected Launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_defaults_classpath_to_current_dir() {
        let req = dispatch_tool("launch", &json!({"main_class": "Main"})).unwrap();
        match req.cmd {
            Command::Launch { classpath, .. } => {
                assert_eq!(classpath, classpath_or_current(None));
            }
            other => panic!("expected Launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_app_args_array_passed_through() {
        let req = dispatch_tool(
            "launch",
            &json!({"main_class": "Main", "app_args": ["a", "b"]}),
        )
        .unwrap();
        match req.cmd {
            Command::Launch { app_args, .. } => {
                assert_eq!(app_args, vec!["a".to_string(), "b".to_string()])
            }
            other => panic!("expected Launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_backend_maps_to_command() {
        let req =
            dispatch_tool("launch", &json!({"main_class": "Main", "backend": "jdi"})).unwrap();
        match req.cmd {
            Command::Launch {
                backend,
                backend_explicit,
                ..
            } => {
                assert_eq!(backend, BackendKind::Jdi);
                assert!(backend_explicit);
            }
            other => panic!("expected Launch, got {other:?}"),
        }
    }

    #[test]
    fn attach_backend_defaults_to_jdi() {
        let req = dispatch_tool("attach", &json!({})).unwrap();
        match req.cmd {
            Command::Attach {
                backend,
                backend_explicit,
                ..
            } => {
                assert_eq!(backend, BackendKind::Jdi);
                assert!(!backend_explicit);
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn attach_explicit_jdb_marks_backend_explicit() {
        let req = dispatch_tool("attach", &json!({"backend": "jdb"})).unwrap();
        match req.cmd {
            Command::Attach {
                backend,
                backend_explicit,
                ..
            } => {
                assert_eq!(backend, BackendKind::Jdb);
                assert!(backend_explicit);
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn break_at_maps_class_and_line() {
        let req = dispatch_tool("break_at", &json!({"class": "Main", "line": 42})).unwrap();
        assert!(
            matches!(req.cmd, Command::BreakAt { ref class, line, .. } if class == "Main" && line == 42)
        );
    }

    #[test]
    fn session_argument_lifts_to_request() {
        let req = dispatch_tool("locals", &json!({"session": "abc123"})).unwrap();
        assert_eq!(req.session.as_deref(), Some("abc123"));
        assert!(matches!(req.cmd, Command::Locals));
    }

    #[test]
    fn timeout_argument_lifts_to_request() {
        let req = dispatch_tool("cont", &json!({"timeout": 60})).unwrap();
        assert_eq!(req.timeout, Some(60));
        assert!(matches!(req.cmd, Command::Cont));
    }

    #[test]
    fn frame_defaults_n_to_one() {
        let req = dispatch_tool("frame", &json!({"direction": "up"})).unwrap();
        assert!(
            matches!(req.cmd, Command::Frame { ref direction, n } if direction == "up" && n == 1)
        );
    }

    #[test]
    fn catch_defaults_mode_to_all() {
        let req = dispatch_tool(
            "catch",
            &json!({"exception": "java.lang.NullPointerException"}),
        )
        .unwrap();
        assert!(matches!(req.cmd, Command::Catch { ref mode, .. } if mode == "all"));
    }

    #[test]
    fn raw_takes_single_command_string() {
        let req = dispatch_tool("raw", &json!({"command": "methods java.lang.String"})).unwrap();
        assert!(
            matches!(req.cmd, Command::Raw { ref command } if command == "methods java.lang.String")
        );
    }

    #[test]
    fn where_all_flag_maps() {
        let req = dispatch_tool("where", &json!({"all": true})).unwrap();
        assert!(matches!(req.cmd, Command::Where { all: true }));
    }

    #[test]
    fn attach_uses_defaults_when_absent() {
        let req = dispatch_tool("attach", &json!({})).unwrap();
        match req.cmd {
            Command::Attach {
                host,
                port,
                sourcepath,
                ..
            } => {
                assert_eq!(host, "localhost");
                assert_eq!(port, 5005);
                assert_eq!(sourcepath, sourcepath_or_current(None));
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn attach_sourcepath_is_absolutized() {
        let req = dispatch_tool("attach", &json!({"sourcepath": "."})).unwrap();
        match req.cmd {
            Command::Attach { sourcepath, .. } => {
                assert_eq!(sourcepath, sourcepath_or_current(Some(".")));
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_param_is_invalid_params() {
        let err = dispatch_tool("break_at", &json!({"class": "Main"})).unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[test]
    fn unknown_tool_is_method_not_found() {
        let err = dispatch_tool("nonexistent", &json!({})).unwrap_err();
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    #[test]
    fn no_arg_tools_dispatch_without_arguments() {
        assert!(matches!(
            dispatch_tool("status", &json!({})).unwrap().cmd,
            Command::Status
        ));
        assert!(matches!(
            dispatch_tool("list", &json!({})).unwrap().cmd,
            Command::List
        ));
        assert!(matches!(
            dispatch_tool("run", &json!({})).unwrap().cmd,
            Command::Run
        ));
    }

    #[test]
    fn inspect_maps_expr_and_defaults_max() {
        let req = dispatch_tool("inspect", &json!({"expr": "myList"})).unwrap();
        assert!(
            matches!(req.cmd, Command::Inspect { ref expr, max_elements } if expr == "myList" && max_elements == 10)
        );
    }

    #[test]
    fn inspect_accepts_custom_max_elements() {
        let req = dispatch_tool("inspect", &json!({"expr": "arr", "max_elements": 5})).unwrap();
        assert!(
            matches!(req.cmd, Command::Inspect { ref expr, max_elements } if expr == "arr" && max_elements == 5)
        );
    }

    // ─── suspend parameter tests ─────────────────────────────────────────────

    // ─── classes / methods / watch / unwatch tests ─────────────────────────

    #[test]
    fn classes_without_pattern() {
        let req = dispatch_tool("classes", &json!({})).unwrap();
        assert!(matches!(req.cmd, Command::Classes { pattern: None }));
    }

    #[test]
    fn classes_with_pattern() {
        let req = dispatch_tool("classes", &json!({"pattern": "Service"})).unwrap();
        match req.cmd {
            Command::Classes { pattern } => assert_eq!(pattern, Some("Service".to_string())),
            other => panic!("expected Classes, got {other:?}"),
        }
    }

    #[test]
    fn methods_requires_class() {
        let err = dispatch_tool("methods", &json!({})).unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[test]
    fn methods_maps_class() {
        let req = dispatch_tool("methods", &json!({"class": "java.lang.String"})).unwrap();
        match req.cmd {
            Command::Methods { class } => assert_eq!(class, "java.lang.String"),
            other => panic!("expected Methods, got {other:?}"),
        }
    }

    #[test]
    fn watch_defaults_mode_to_modification() {
        let req = dispatch_tool("watch", &json!({"field": "Main.x"})).unwrap();
        match req.cmd {
            Command::Watch { field, mode } => {
                assert_eq!(field, "Main.x");
                assert_eq!(mode, "modification");
            }
            other => panic!("expected Watch, got {other:?}"),
        }
    }

    #[test]
    fn watch_with_access_mode() {
        let req = dispatch_tool("watch", &json!({"field": "Main.x", "mode": "access"})).unwrap();
        match req.cmd {
            Command::Watch { mode, .. } => assert_eq!(mode, "access"),
            other => panic!("expected Watch, got {other:?}"),
        }
    }

    #[test]
    fn unwatch_maps_field() {
        let req = dispatch_tool("unwatch", &json!({"field": "Main.x"})).unwrap();
        match req.cmd {
            Command::Unwatch { field, mode } => {
                assert_eq!(field, "Main.x");
                assert_eq!(mode, "modification");
            }
            other => panic!("expected Unwatch, got {other:?}"),
        }
    }

    #[test]
    fn unwatch_maps_mode_access() {
        let req = dispatch_tool("unwatch", &json!({"field": "Main.x", "mode": "access"})).unwrap();
        match req.cmd {
            Command::Unwatch { field, mode } => {
                assert_eq!(field, "Main.x");
                assert_eq!(mode, "access");
            }
            other => panic!("expected Unwatch, got {other:?}"),
        }
    }

    // ─── suspend parameter tests ─────────────────────────────────────────────

    #[test]
    fn break_at_suspend_thread_maps() {
        let req = dispatch_tool(
            "break_at",
            &json!({"class": "Main", "line": 10, "suspend": "thread"}),
        )
        .unwrap();
        match req.cmd {
            Command::BreakAt { suspend, .. } => assert_eq!(suspend, Some("thread".to_string())),
            other => panic!("expected BreakAt, got {other:?}"),
        }
    }

    #[test]
    fn break_at_suspend_all_maps() {
        let req = dispatch_tool(
            "break_at",
            &json!({"class": "Main", "line": 10, "suspend": "all"}),
        )
        .unwrap();
        match req.cmd {
            Command::BreakAt { suspend, .. } => assert_eq!(suspend, Some("all".to_string())),
            other => panic!("expected BreakAt, got {other:?}"),
        }
    }

    #[test]
    fn break_at_suspend_absent_is_none() {
        let req = dispatch_tool("break_at", &json!({"class": "Main", "line": 10})).unwrap();
        match req.cmd {
            Command::BreakAt { suspend, .. } => assert_eq!(suspend, None),
            other => panic!("expected BreakAt, got {other:?}"),
        }
    }

    #[test]
    fn break_in_suspend_thread_maps() {
        let req = dispatch_tool(
            "break_in",
            &json!({"class": "Main", "method": "foo", "suspend": "thread"}),
        )
        .unwrap();
        match req.cmd {
            Command::BreakIn { suspend, .. } => assert_eq!(suspend, Some("thread".to_string())),
            other => panic!("expected BreakIn, got {other:?}"),
        }
    }

    #[test]
    fn break_in_suspend_absent_is_none() {
        let req = dispatch_tool("break_in", &json!({"class": "Main", "method": "foo"})).unwrap();
        match req.cmd {
            Command::BreakIn { suspend, event, .. } => {
                assert_eq!(suspend, None);
                assert_eq!(event, crate::protocol::MethodEventKind::Entry);
            }
            other => panic!("expected BreakIn, got {other:?}"),
        }
    }

    #[test]
    fn break_in_event_maps_and_schema_lists_choices() {
        let req = dispatch_tool(
            "break_in",
            &json!({"class": "Main", "method": "foo", "event": "exit"}),
        )
        .unwrap();
        match req.cmd {
            Command::BreakIn { event, .. } => {
                assert_eq!(event, crate::protocol::MethodEventKind::Exit)
            }
            other => panic!("expected BreakIn, got {other:?}"),
        }

        let spec = tool_specs()
            .into_iter()
            .find(|tool| tool.name == "break_in")
            .expect("break_in tool spec");
        assert_eq!(
            spec.input_schema["properties"]["event"]["enum"],
            json!(["entry", "exit", "both"])
        );
    }

    #[test]
    fn break_at_with_condition_and_suspend() {
        let req = dispatch_tool(
            "break_at",
            &json!({
                "class": "Main", "line": 10,
                "condition": "x > 5",
                "suspend": "thread"
            }),
        )
        .unwrap();
        match req.cmd {
            Command::BreakAt {
                condition, suspend, ..
            } => {
                assert_eq!(condition, Some("x > 5".to_string()));
                assert_eq!(suspend, Some("thread".to_string()));
            }
            other => panic!("expected BreakAt, got {other:?}"),
        }
    }

    // ─── new commands: threads filter + thread control / set / ignore / locks ───

    #[test]
    fn threads_with_filter_maps() {
        let req = dispatch_tool("threads", &json!({"filter": "http-nio"})).unwrap();
        match req.cmd {
            Command::Threads { filter } => assert_eq!(filter, Some("http-nio".to_string())),
            other => panic!("expected Threads, got {other:?}"),
        }
    }

    #[test]
    fn threads_without_filter_is_none() {
        let req = dispatch_tool("threads", &json!({})).unwrap();
        match req.cmd {
            Command::Threads { filter } => assert_eq!(filter, None),
            other => panic!("expected Threads, got {other:?}"),
        }
    }

    #[test]
    fn suspend_with_and_without_id() {
        match dispatch_tool("suspend", &json!({"id": "0x1"})).unwrap().cmd {
            Command::Suspend { id } => assert_eq!(id, Some("0x1".to_string())),
            other => panic!("expected Suspend, got {other:?}"),
        }
        match dispatch_tool("suspend", &json!({})).unwrap().cmd {
            Command::Suspend { id } => assert_eq!(id, None),
            other => panic!("expected Suspend, got {other:?}"),
        }
    }

    #[test]
    fn resume_maps() {
        match dispatch_tool("resume", &json!({"id": "18315"}))
            .unwrap()
            .cmd
        {
            Command::Resume { id } => assert_eq!(id, Some("18315".to_string())),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn set_maps_lvalue_and_value() {
        match dispatch_tool("set", &json!({"lvalue": "this.count", "value": "42"}))
            .unwrap()
            .cmd
        {
            Command::Set { lvalue, value } => {
                assert_eq!(lvalue, "this.count");
                assert_eq!(value, "42");
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn set_missing_value_is_error() {
        assert!(dispatch_tool("set", &json!({"lvalue": "x"})).is_err());
    }

    #[test]
    fn force_return_maps_value() {
        match dispatch_tool("force_return", &json!({"value": "123"}))
            .unwrap()
            .cmd
        {
            Command::ForceReturn { value } => assert_eq!(value, "123"),
            other => panic!("expected ForceReturn, got {other:?}"),
        }
    }

    #[test]
    fn thread_id_accepts_numeric_json() {
        // LLM clients frequently emit a numeric thread id (e.g. {"id": 582})
        // instead of a string. require_str must coerce it, not reject it.
        match dispatch_tool("thread", &json!({"id": 582})).unwrap().cmd {
            Command::Thread { id } => assert_eq!(id, "582"),
            other => panic!("expected Thread, got {other:?}"),
        }
        match dispatch_tool("thread", &json!({"id": "0x37f2"}))
            .unwrap()
            .cmd
        {
            Command::Thread { id } => assert_eq!(id, "0x37f2"),
            other => panic!("expected Thread, got {other:?}"),
        }
    }

    #[test]
    fn suspend_id_accepts_numeric_json() {
        // A numeric id must target that thread, not silently fall back to "all".
        match dispatch_tool("suspend", &json!({"id": 1})).unwrap().cmd {
            Command::Suspend { id } => assert_eq!(id, Some("1".to_string())),
            other => panic!("expected Suspend, got {other:?}"),
        }
    }

    #[test]
    fn ignore_maps_with_mode_default() {
        match dispatch_tool("ignore", &json!({"exception": "java.lang.NPE"}))
            .unwrap()
            .cmd
        {
            Command::Ignore { exception, mode } => {
                assert_eq!(exception, "java.lang.NPE");
                assert_eq!(mode, "all");
            }
            other => panic!("expected Ignore, got {other:?}"),
        }
        match dispatch_tool("ignore", &json!({"exception": "E", "mode": "uncaught"}))
            .unwrap()
            .cmd
        {
            Command::Ignore { mode, .. } => assert_eq!(mode, "uncaught"),
            other => panic!("expected Ignore, got {other:?}"),
        }
    }

    #[test]
    fn lock_maps_expr() {
        match dispatch_tool("lock", &json!({"expr": "this.mutex"}))
            .unwrap()
            .cmd
        {
            Command::Lock { expr } => assert_eq!(expr, "this.mutex"),
            other => panic!("expected Lock, got {other:?}"),
        }
    }

    #[test]
    fn threadlocks_with_and_without_id() {
        match dispatch_tool("threadlocks", &json!({"id": "0xbb"}))
            .unwrap()
            .cmd
        {
            Command::ThreadLocks { id } => assert_eq!(id, Some("0xbb".to_string())),
            other => panic!("expected ThreadLocks, got {other:?}"),
        }
        match dispatch_tool("threadlocks", &json!({})).unwrap().cmd {
            Command::ThreadLocks { id } => assert_eq!(id, None),
            other => panic!("expected ThreadLocks, got {other:?}"),
        }
    }
}

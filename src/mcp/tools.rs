//! 工具目录 + 翻译层——MCP `tools/call` ↔ [`crate::protocol::Command`] 的唯一映射点。
//!
//! 每个 jdbg 子命令对应一个 MCP 工具（细粒度 1:1）。[`tool_specs`] 供 `tools/list` 列出；
//! [`dispatch_tool`] 把 (工具名, arguments) 翻译成 [`Request`]，对齐 `main.rs::build_command`
//! 的既有转换（如 `classpath` string→`Vec`、`raw` 单 string、各默认值）。

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::jsonrpc::{INVALID_PARAMS, JsonRpcError, METHOD_NOT_FOUND};
use crate::protocol::{Command, Request};

/// 一个工具的对外描述（序列化进 `tools/list`）。
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// 全部 25 个工具的 spec（供 `tools/list`）。
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        // ── 会话 ──
        tool(
            "launch",
            "Launch a Java program under the debugger. Returns a session in state 'loaded' \
             (JVM not started yet) — set breakpoints, then call `run`.",
            json!({
                "main_class": {"type": "string", "description": "Fully-qualified main class, e.g. com.example.Main."},
                "classpath": {"type": "string", "description": "Classpath (OS-separated entries)."},
                "sourcepath": {"type": "string", "description": "Source path; enables `list_source`/`locals` line info."},
                "app_args": {"type": "array", "items": {"type": "string"}, "description": "Arguments passed to the program's main()."},
                "jdb_args": {"type": "array", "items": {"type": "string"}, "description": "Extra raw arguments passed to jdb."},
                "name": {"type": "string", "description": "Optional session display name."},
                "jdb_path": {"type": "string", "description": "Override path to the jdb executable."}
            }),
            &["main_class"],
            false,
            false,
        ),
        tool(
            "attach",
            "Attach to an already-running JVM started with JDWP. Returns state 'suspended' — \
             set breakpoints, then call `cont` (attach has no `run`).",
            json!({
                "host": {"type": "string", "description": "Target host (default localhost)."},
                "port": {"type": "integer", "description": "Target JDWP port (default 5005)."},
                "sourcepath": {"type": "string", "description": "Source path for line info."},
                "name": {"type": "string", "description": "Optional session display name."},
                "jdb_path": {"type": "string", "description": "Override path to the jdb executable."}
            }),
            &[],
            false,
            false,
        ),
        tool("status", "Report a session's run state, mode, target, and last event (sends no jdb command).", json!({}), &[], true, false),
        tool("list", "List all active debug sessions.", json!({}), &[], false, false),
        tool("kill", "End a debug session (defaults to the sole session if exactly one exists).", json!({}), &[], true, false),
        // ── 断点 ──
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
            "Set a method-entry breakpoint at Class.method. Use `args` (comma-separated param types) to \
             disambiguate overloads. Use `condition` to only stop when a boolean expression is true. \
             Use `suspend: \"thread\"` to only suspend the hit thread (keeps heartbeat threads alive).",
            json!({
                "class": {"type": "string", "description": "Class name."},
                "method": {"type": "string", "description": "Method name."},
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
        tool("breakpoints", "List the currently set breakpoints.", json!({}), &[], true, false),
        tool(
            "clear",
            "Remove a breakpoint by spec (Class:line or Class.method).",
            json!({"spec": {"type": "string", "description": "Breakpoint spec to clear, e.g. Main:42 or Main.foo."}}),
            &["spec"],
            true,
            false,
        ),
        // ── 执行控制（阻塞，较大默认超时）──
        tool("run", "Start the debugged application (launch mode only). Blocks until a breakpoint, exception, or exit. Returns the stop location with source context and top stack frame when available.", json!({}), &[], true, true),
        tool("cont", "Continue execution until the next stop (breakpoint, exception, or program exit). Returns the stop location with source context and top stack frame when available.", json!({}), &[], true, true),
        tool("step", "Step into the next line, entering called methods. Returns the stop location with source context and top stack frame when available.", json!({}), &[], true, true),
        tool("next", "Step over the next line, without entering called methods. Returns the stop location with source context and top stack frame when available.", json!({}), &[], true, true),
        tool("step_out", "Run until the current method returns. Returns the stop location with source context and top stack frame when available.", json!({}), &[], true, true),
        // ── 检查（快）──
        tool(
            "where",
            "Show the current thread's call stack. Set all=true for every thread's stack.",
            json!({"all": {"type": "boolean", "description": "Show the stack of every thread."}}),
            &[],
            true,
            false,
        ),
        tool("locals", "Show local variables in the current frame (requires classes compiled with `javac -g`).", json!({}), &[], true, false),
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
        tool("threads", "List all threads with id, name, group, and state.", json!({}), &[], true, false),
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
            "Escape hatch: send a literal jdb command (monitor, fields, methods, classes, redefine, trace, …) \
             and return its raw output.",
            json!({"command": {"type": "string", "description": "The literal jdb command string."}}),
            &["command"],
            true,
            false,
        ),
    ]
}

/// 把一次 `tools/call`（工具名 + arguments）翻译成发往 daemon 的 [`Request`]。
pub fn dispatch_tool(name: &str, args: &Value) -> Result<Request, JsonRpcError> {
    let session = optional_str(args, "session");
    let timeout = args.get("timeout").and_then(Value::as_u64);

    let cmd = match name {
        "launch" => Command::Launch {
            main_class: require_str(args, "main_class")?,
            classpath: str_to_vec(optional_str(args, "classpath")),
            sourcepath: str_to_vec(optional_str(args, "sourcepath")),
            app_args: optional_str_array(args, "app_args"),
            jdb_args: optional_str_array(args, "jdb_args"),
            name: optional_str(args, "name"),
            jdb_path: optional_str(args, "jdb_path"),
        },
        "attach" => Command::Attach {
            host: optional_str(args, "host").unwrap_or_else(|| "localhost".to_string()),
            port: optional_u16(args, "port").unwrap_or(5005),
            sourcepath: str_to_vec(optional_str(args, "sourcepath")),
            name: optional_str(args, "name"),
            jdb_path: optional_str(args, "jdb_path"),
        },
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
            args: optional_str(args, "args"),
            condition: optional_str(args, "condition"),
            suspend: optional_str(args, "suspend"),
        },
        "catch" => Command::Catch {
            exception: require_str(args, "exception")?,
            mode: optional_str(args, "mode").unwrap_or_else(|| "all".to_string()),
        },
        "breakpoints" => Command::Breakpoints,
        "clear" => Command::Clear { spec: require_str(args, "spec")? },
        "run" => Command::Run,
        "cont" => Command::Cont,
        "step" => Command::Step,
        "next" => Command::Next,
        "step_out" => Command::StepOut,
        "where" => Command::Where { all: optional_bool(args, "all") },
        "locals" => Command::Locals,
        "print" => Command::Print { expr: require_str(args, "expr")? },
        "dump" => Command::Dump { expr: require_str(args, "expr")? },
        "eval" => Command::Eval { expr: require_str(args, "expr")? },
        "threads" => Command::Threads,
        "thread" => Command::Thread { id: require_str(args, "id")? },
        "frame" => Command::Frame {
            direction: require_str(args, "direction")?,
            n: optional_u32(args, "n").unwrap_or(1),
        },
        "list_source" => Command::ListSource { line: optional_u32(args, "line") },
        "inspect" => Command::Inspect {
            expr: require_str(args, "expr")?,
            max_elements: optional_u32(args, "max_elements").unwrap_or(10),
        },
        "raw" => Command::Raw { command: require_str(args, "command")? },
        _ => return Err(JsonRpcError::new(METHOD_NOT_FOUND, format!("unknown tool: {name}"))),
    };

    Ok(Request::new(cmd, session).with_timeout(timeout))
}

// ── 内部 helpers ──────────────────────────────────────────────────────────────

/// 构建一个工具 spec，自动按需注入通用的 `session` / `timeout` 属性。
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
    ToolSpec { name, description, input_schema: Value::Object(schema) }
}

fn require_str(args: &Value, key: &str) -> Result<String, JsonRpcError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, format!("missing required string parameter: {key}")))
}

fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

fn require_u32(args: &Value, key: &str) -> Result<u32, JsonRpcError> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, format!("missing required integer parameter: {key}")))
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

fn optional_str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// classpath/sourcepath：MCP 收单个 string，包成长度≤1 的 `Vec`（与 `main.rs` 一致）。
fn str_to_vec(opt: Option<String>) -> Vec<String> {
    opt.map(|s| vec![s]).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Command;
    use serde_json::json;

    #[test]
    fn exposes_26_tools() {
        assert_eq!(tool_specs().len(), 26);
    }

    #[test]
    fn every_schema_is_object_and_names_unique() {
        let specs = tool_specs();
        let mut names = std::collections::HashSet::new();
        for s in &specs {
            assert_eq!(s.input_schema["type"], json!("object"), "tool {} schema not object", s.name);
            assert!(names.insert(s.name), "duplicate tool name: {}", s.name);
        }
    }

    #[test]
    fn launch_maps_classpath_string_to_vec() {
        let req = dispatch_tool("launch", &json!({"main_class": "Main", "classpath": "out"})).unwrap();
        match req.cmd {
            Command::Launch { main_class, classpath, .. } => {
                assert_eq!(main_class, "Main");
                assert_eq!(classpath, vec!["out".to_string()]);
            }
            other => panic!("expected Launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_app_args_array_passed_through() {
        let req = dispatch_tool("launch", &json!({"main_class": "Main", "app_args": ["a", "b"]})).unwrap();
        match req.cmd {
            Command::Launch { app_args, .. } => assert_eq!(app_args, vec!["a".to_string(), "b".to_string()]),
            other => panic!("expected Launch, got {other:?}"),
        }
    }

    #[test]
    fn break_at_maps_class_and_line() {
        let req = dispatch_tool("break_at", &json!({"class": "Main", "line": 42})).unwrap();
        assert!(matches!(req.cmd, Command::BreakAt { ref class, line, .. } if class == "Main" && line == 42));
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
        assert!(matches!(req.cmd, Command::Frame { ref direction, n } if direction == "up" && n == 1));
    }

    #[test]
    fn catch_defaults_mode_to_all() {
        let req = dispatch_tool("catch", &json!({"exception": "java.lang.NullPointerException"})).unwrap();
        assert!(matches!(req.cmd, Command::Catch { ref mode, .. } if mode == "all"));
    }

    #[test]
    fn raw_takes_single_command_string() {
        let req = dispatch_tool("raw", &json!({"command": "methods java.lang.String"})).unwrap();
        assert!(matches!(req.cmd, Command::Raw { ref command } if command == "methods java.lang.String"));
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
            Command::Attach { host, port, .. } => {
                assert_eq!(host, "localhost");
                assert_eq!(port, 5005);
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
        assert!(matches!(dispatch_tool("status", &json!({})).unwrap().cmd, Command::Status));
        assert!(matches!(dispatch_tool("list", &json!({})).unwrap().cmd, Command::List));
        assert!(matches!(dispatch_tool("run", &json!({})).unwrap().cmd, Command::Run));
    }

    #[test]
    fn inspect_maps_expr_and_defaults_max() {
        let req = dispatch_tool("inspect", &json!({"expr": "myList"})).unwrap();
        assert!(matches!(req.cmd, Command::Inspect { ref expr, max_elements } if expr == "myList" && max_elements == 10));
    }

    #[test]
    fn inspect_accepts_custom_max_elements() {
        let req = dispatch_tool("inspect", &json!({"expr": "arr", "max_elements": 5})).unwrap();
        assert!(matches!(req.cmd, Command::Inspect { ref expr, max_elements } if expr == "arr" && max_elements == 5));
    }

    // ─── suspend parameter tests ─────────────────────────────────────────────

    #[test]
    fn break_at_suspend_thread_maps() {
        let req = dispatch_tool("break_at", &json!({"class": "Main", "line": 10, "suspend": "thread"})).unwrap();
        match req.cmd {
            Command::BreakAt { suspend, .. } => assert_eq!(suspend, Some("thread".to_string())),
            other => panic!("expected BreakAt, got {other:?}"),
        }
    }

    #[test]
    fn break_at_suspend_all_maps() {
        let req = dispatch_tool("break_at", &json!({"class": "Main", "line": 10, "suspend": "all"})).unwrap();
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
        let req = dispatch_tool("break_in", &json!({"class": "Main", "method": "foo", "suspend": "thread"})).unwrap();
        match req.cmd {
            Command::BreakIn { suspend, .. } => assert_eq!(suspend, Some("thread".to_string())),
            other => panic!("expected BreakIn, got {other:?}"),
        }
    }

    #[test]
    fn break_in_suspend_absent_is_none() {
        let req = dispatch_tool("break_in", &json!({"class": "Main", "method": "foo"})).unwrap();
        match req.cmd {
            Command::BreakIn { suspend, .. } => assert_eq!(suspend, None),
            other => panic!("expected BreakIn, got {other:?}"),
        }
    }

    #[test]
    fn break_at_with_condition_and_suspend() {
        let req = dispatch_tool("break_at", &json!({
            "class": "Main", "line": 10,
            "condition": "x > 5",
            "suspend": "thread"
        })).unwrap();
        match req.cmd {
            Command::BreakAt { condition, suspend, .. } => {
                assert_eq!(condition, Some("x > 5".to_string()));
                assert_eq!(suspend, Some("thread".to_string()));
            }
            other => panic!("expected BreakAt, got {other:?}"),
        }
    }
}

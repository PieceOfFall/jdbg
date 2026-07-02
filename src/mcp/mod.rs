//! MCP (Model Context Protocol) server: expose jdbg as native tool calls for coding agents.
//!
//! Design: the MCP server is the daemon's **second client**, peer to the CLI. It runs a handwritten
//! JSON-RPC 2.0 loop over stdio (no tokio; reuse serde_json + blocking IO), translates each `tools/call`
//! into [`crate::protocol::Command`] + [`crate::protocol::Request`], sends it to the daemon via
//! [`crate::client::send_request`], then renders [`crate::protocol::Response`] back into an MCP
//! `CallToolResult`.
//!
//! Module split:
//! - [`jsonrpc`]: basic JSON-RPC 2.0 types (request/response/error).
//! - [`tools`]: 37 tool specs (name/description/inputSchema) plus tool-name → `Command` translation.
//!
//! **stdout discipline**: stdout carries only JSON-RPC messages. Any logs/diagnostics must go to stderr
//! (`eprintln!`), otherwise they corrupt the protocol stream.

pub mod jsonrpc;
pub mod rmcp_server;
pub mod tools;

use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

use crate::client;
use crate::output;
use crate::protocol::Response;
use jsonrpc::{
    INVALID_PARAMS, JsonRpcError, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND, PARSE_ERROR,
};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp_server::JdbgRmcpServer;

/// Default MCP protocol version we announce. If the client supplies a version during initialize, echo it.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Run the MCP server: a line-delimited JSON-RPC 2.0 loop over stdin/stdout until stdin EOF.
pub fn run_mcp() -> anyhow::Result<()> {
    // Windows: mark this process's stdout/stderr as non-inheritable. Otherwise the detached daemon
    // auto-spawned by the first tool call inherits the MCP server's stdout pipe writer, so the agent
    // never observes EOF.
    #[cfg(windows)]
    detach_std_handles_from_children();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = JdbgRmcpServer::new().serve(stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

/// Legacy handwritten JSON-RPC loop retained for unit-test coverage and as a reference for protocol mapping.
#[allow(dead_code)]
fn run_legacy_mcp_loop() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF: the client closed the pipe.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                write_message(
                    &stdout,
                    &JsonRpcResponse::error(
                        Value::Null,
                        JsonRpcError::new(PARSE_ERROR, format!("parse error: {e}")),
                    ),
                )?;
                continue;
            }
        };

        // Notifications have no id: handle them without responding. notifications/initialized currently needs no action.
        if req.is_notification() {
            continue;
        }

        let id = req.id.clone().unwrap_or(Value::Null);
        let response = handle_request(&req, id);
        write_message(&stdout, &response)?;
    }
    Ok(())
}

/// Route one request with an id.
fn handle_request(req: &JsonRpcRequest, id: Value) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => JsonRpcResponse::success(id, initialize_result(req)),
        "tools/list" => JsonRpcResponse::success(id, json!({ "tools": tools::tool_specs() })),
        "tools/call" => match call_tool(req) {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(err) => JsonRpcResponse::error(id, err),
        },
        "ping" => JsonRpcResponse::success(id, json!({})),
        other => JsonRpcResponse::error(
            id,
            JsonRpcError::new(METHOD_NOT_FOUND, format!("method not found: {other}")),
        ),
    }
}

/// initialize response: announce tools capability plus serverInfo.
fn initialize_result(req: &JsonRpcRequest) -> Value {
    let version = req
        .params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "jdbg", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// Handle `tools/call`: translate, send to daemon, then map to `CallToolResult`.
///
/// Protocol-layer problems (missing params/name, unknown tool, missing required params) become
/// `Err(JsonRpcError)` JSON-RPC errors. Business/connection failures (dead session, daemon spawn failure)
/// become `Ok` tool-level errors (`isError: true`) so the agent can see the message and continue.
fn call_tool(req: &JsonRpcRequest) -> Result<Value, JsonRpcError> {
    let params = req
        .params
        .as_ref()
        .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, "missing params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, "missing tool name"))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);

    let request = tools::dispatch_tool(name, args)?;

    match client::send_request(&request) {
        Ok(resp) => Ok(response_to_call_result(resp)),
        Err(e) => Ok(tool_error(format!("daemon request failed: {e}"))),
    }
}

/// Map a daemon `Response` into an MCP `CallToolResult` with text content plus isError.
fn response_to_call_result(resp: Response) -> Value {
    if resp.ok {
        let text = resp
            .result
            .as_ref()
            .map(|cr| output::render(cr, false))
            .unwrap_or_else(|| "(no result)".to_string());
        json!({ "content": [{ "type": "text", "text": text }], "isError": false })
    } else {
        let msg = resp
            .error
            .map(|e| format!("[{}] {}", e.code, e.message))
            .unwrap_or_else(|| "unknown error".to_string());
        tool_error(msg)
    }
}

/// Build a tool-level error result (`isError: true`).
fn tool_error(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }], "isError": true })
}

/// Write one JSON-RPC message to stdout as a single flushed line. stdout carries only protocol data.
fn write_message(stdout: &std::io::Stdout, msg: &JsonRpcResponse) -> anyhow::Result<()> {
    let s = serde_json::to_string(msg)?;
    let mut lock = stdout.lock();
    lock.write_all(s.as_bytes())?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}

/// Windows: clear `HANDLE_FLAG_INHERIT` on this process's stdout/stderr handles so later child processes
/// (the auto-spawned daemon) no longer inherit them. Zero-dependency raw FFI (kernel32).
///
/// Do not touch stdin: the agent writes to stdin and pipe closure naturally ends the server, so there is no
/// inheritance issue. Failures are ignored; invalid handles (for example redirected to files) do not have
/// this inheritance leak risk.
#[cfg(windows)]
fn detach_std_handles_from_children() {
    use std::os::windows::io::AsRawHandle;

    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    unsafe extern "system" {
        fn SetHandleInformation(h: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    for raw in [stdout.as_raw_handle(), stderr.as_raw_handle()] {
        if !raw.is_null() {
            // flags=0 clears the INHERIT bit.
            unsafe { SetHandleInformation(raw as *mut std::ffi::c_void, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

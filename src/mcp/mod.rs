//! MCP（Model Context Protocol）server——把 jdbg 暴露为 Claude Code 的原生工具调用。
//!
//! 设计：MCP server 是 daemon 的**第二种客户端**，与 CLI 平级。它通过 stdio 跑一个手写的
//! JSON-RPC 2.0 循环（无 tokio，复用现有 serde_json + blocking IO），把每个 `tools/call`
//! 翻译成 [`crate::protocol::Command`] + [`crate::protocol::Request`]，经 [`crate::client::send_request`]
//! 发给 daemon，再把 [`crate::protocol::Response`] 渲染回 MCP 的 `CallToolResult`。
//!
//! 模块划分：
//! - [`jsonrpc`]：JSON-RPC 2.0 基础类型（请求/响应/错误）。
//! - [`tools`]：25 个工具的 spec（name/description/inputSchema）+ 工具名→`Command` 翻译层。
//!
//! **stdout 纪律**：stdout 只承载 JSON-RPC 消息，任何日志/诊断必须走 stderr（`eprintln!`），
//! 否则会污染协议流。

pub mod jsonrpc;
pub mod tools;

use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

use crate::client;
use crate::output;
use crate::protocol::Response;
use jsonrpc::{INVALID_PARAMS, JsonRpcError, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND, PARSE_ERROR};

/// 我们默认通告的 MCP 协议版本（initialize 时若客户端给了版本则回显其值）。
const PROTOCOL_VERSION: &str = "2025-06-18";

/// 运行 MCP server：stdin/stdout 上的 JSON-RPC 2.0 行分隔循环，直到 stdin EOF。
pub fn run_mcp() -> anyhow::Result<()> {
    // Windows：把本进程的 stdout/stderr 标记为不可继承，否则首次工具调用 auto-spawn 的
    // detached daemon 会继承 MCP server 的 stdout 管道写端，使 Claude 端永远读不到 EOF。
    #[cfg(windows)]
    detach_std_handles_from_children();

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF：客户端关闭了管道。
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
                    &JsonRpcResponse::error(Value::Null, JsonRpcError::new(PARSE_ERROR, format!("parse error: {e}"))),
                )?;
                continue;
            }
        };

        // 通知（无 id）：处理但不回响应。notifications/initialized 等目前无需动作。
        if req.is_notification() {
            continue;
        }

        let id = req.id.clone().unwrap_or(Value::Null);
        let response = handle_request(&req, id);
        write_message(&stdout, &response)?;
    }
    Ok(())
}

/// 路由一条带 id 的请求。
fn handle_request(req: &JsonRpcRequest, id: Value) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => JsonRpcResponse::success(id, initialize_result(req)),
        "tools/list" => JsonRpcResponse::success(id, json!({ "tools": tools::tool_specs() })),
        "tools/call" => match call_tool(req) {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(err) => JsonRpcResponse::error(id, err),
        },
        "ping" => JsonRpcResponse::success(id, json!({})),
        other => {
            JsonRpcResponse::error(id, JsonRpcError::new(METHOD_NOT_FOUND, format!("method not found: {other}")))
        }
    }
}

/// initialize 响应：通告 tools 能力 + serverInfo。
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

/// 处理 `tools/call`：翻译 → 发给 daemon → 映射成 `CallToolResult`。
///
/// 协议层问题（缺 params/name、未知工具、缺必填参数）→ `Err(JsonRpcError)`（JSON-RPC error）。
/// 业务/连接问题（session dead、daemon 拉起失败）→ `Ok` 的 tool-level error（`isError: true`），
/// 让 Claude 能看到信息并继续。
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

/// 把 daemon 的 `Response` 映射成 MCP `CallToolResult`（文本内容 + isError）。
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

/// 构造一个 tool-level 错误结果（`isError: true`）。
fn tool_error(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }], "isError": true })
}

/// 写一条 JSON-RPC 消息到 stdout（单行 + flush）。stdout 只承载协议。
fn write_message(stdout: &std::io::Stdout, msg: &JsonRpcResponse) -> anyhow::Result<()> {
    let s = serde_json::to_string(msg)?;
    let mut lock = stdout.lock();
    lock.write_all(s.as_bytes())?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}

/// Windows：清除本进程 stdout/stderr 句柄的 `HANDLE_FLAG_INHERIT`，使后续 spawn 的子进程
/// （auto-spawn 的 daemon）不再继承它们。零依赖裸 FFI（kernel32）。
///
/// 不动 stdin：stdin 由 Claude 写、随管道关闭自然结束，无继承问题。失败静默忽略——
/// 句柄无效（如被重定向到文件）时本就没有继承泄漏风险。
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
            // flags=0 清除 INHERIT 位。
            unsafe { SetHandleInformation(raw as *mut std::ffi::c_void, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

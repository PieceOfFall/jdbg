//! rmcp-based MCP server adapter.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorCode, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::Value;

use crate::client;
use crate::mcp::tools;
use crate::output;
use crate::protocol::Response;

#[derive(Debug, Clone, Default)]
pub struct JdbgRmcpServer;

impl JdbgRmcpServer {
    pub fn new() -> Self {
        Self
    }
}

impl ServerHandler for JdbgRmcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("jdbg", env!("CARGO_PKG_VERSION")))
            .with_instructions("jdbg Java debugger tools")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: rmcp_tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| Value::Object(Default::default()));
        let daemon_request = tools::dispatch_tool(&request.name, &args)
            .map_err(|e| McpError::new(ErrorCode(e.code), e.message, None))?;

        match client::send_request(&daemon_request) {
            Ok(resp) => Ok(response_to_call_tool_result(resp)),
            Err(e) => Ok(tool_error_result(format!("daemon request failed: {e}"))),
        }
    }
}

pub fn rmcp_tools() -> Vec<Tool> {
    tools::tool_specs()
        .into_iter()
        .map(|spec| {
            let schema = spec
                .input_schema
                .as_object()
                .cloned()
                .unwrap_or_else(Default::default);
            Tool::new(spec.name, spec.description, Arc::new(schema))
        })
        .collect()
}

pub fn response_to_call_tool_result(resp: Response) -> CallToolResult {
    if resp.ok {
        let text = resp
            .result
            .as_ref()
            .map(|cr| output::render(cr, false))
            .unwrap_or_else(|| "(no result)".to_string());
        CallToolResult::success(vec![ContentBlock::text(text)])
    } else {
        let msg = resp
            .error
            .map(|e| format!("[{}] {}", e.code, e.message))
            .unwrap_or_else(|| "unknown error".to_string());
        tool_error_result(msg)
    }
}

fn tool_error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CommandResponse, CommandResult, Response};
    use rmcp::model::ContentBlock;

    #[test]
    fn initialize_info_preserves_jdbg_identity() {
        let info = JdbgRmcpServer::new().get_info();

        assert_eq!(info.server_info.name, "jdbg");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn rmcp_tools_preserve_existing_catalog_and_schemas() {
        let tools = rmcp_tools();

        assert_eq!(tools.len(), crate::mcp::tools::tool_specs().len());
        assert!(tools.iter().any(|tool| {
            tool.name == "launch"
                && tool.schema_as_json_value()["properties"]["main_class"]["type"] == "string"
        }));
    }

    #[test]
    fn successful_daemon_response_becomes_successful_call_tool_result() {
        let result = response_to_call_tool_result(Response::ok(
            "r1",
            CommandResponse {
                result: CommandResult::Raw {
                    text: "hello".into(),
                },
                stderr: None,
                note: None,
            },
        ));

        assert_eq!(result.is_error, Some(false));
        assert_eq!(first_text(&result), "hello");
    }

    #[test]
    fn failed_daemon_response_is_tool_level_error() {
        let result = response_to_call_tool_result(Response::err("r1", 5, "session dead"));

        assert_eq!(result.is_error, Some(true));
        assert!(first_text(&result).contains("[5] session dead"));
    }

    fn first_text(result: &rmcp::model::CallToolResult) -> &str {
        match result.content.first().expect("content") {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

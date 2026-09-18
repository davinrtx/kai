//! Model Context Protocol (MCP) JSON-RPC 2.0 tool caller and mock transports.
//!
//! Provides [`McpClient`] and [`McpTransport`] for invoking external agent tools via
//! the standardized Model Context Protocol over stdio, HTTP/SSE, or in-memory mock transports.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

/// Definition of an MCP tool advertised by a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    /// Canonical name of the remote tool.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema specification for input arguments.
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// Content element returned within an MCP tool execution result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpContent {
    /// Content type (e.g. "text", "image", "resource").
    #[serde(rename = "type")]
    pub content_type: String,
    /// Text body if content is textual.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 data if content is binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Structured outcome of an MCP tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCallResult {
    /// Content payload returned by the remote server.
    pub content: Vec<McpContent>,
    /// Indicates whether execution resulted in an error on the remote server.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

impl McpCallResult {
    /// Combines all text blocks into a single string.
    pub fn combined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Transport abstraction for Model Context Protocol communication.
pub trait McpTransport: Send + Sync {
    /// Transmits a JSON-RPC 2.0 request and awaits the JSON-RPC response.
    fn send_request<'a>(
        &'a self,
        request: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value>>;
}

type MockHandler =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, Result<String>> + Send + Sync + 'static>;

/// Offline, deterministic in-memory MCP transport for testing.
#[derive(Clone, Default)]
pub struct MockMcpTransport {
    tools: Arc<RwLock<HashMap<String, (McpToolDefinition, MockHandler)>>>,
}

impl MockMcpTransport {
    /// Constructs a new [`MockMcpTransport`].
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registers a mock tool with its definition and execution handler.
    pub async fn register_tool<F>(&self, def: McpToolDefinition, handler: F)
    where
        F: Fn(serde_json::Value) -> BoxFuture<'static, Result<String>> + Send + Sync + 'static,
    {
        let mut guard = self.tools.write().await;
        guard.insert(def.name.clone(), (def, Arc::new(handler)));
    }
}

impl McpTransport for MockMcpTransport {
    fn send_request<'a>(
        &'a self,
        request: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let id = request.get("id").cloned().unwrap_or(json!(1));
            let method = request
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or_default();

            match method {
                "tools/list" => {
                    let guard = self.tools.read().await;
                    let mut defs: Vec<McpToolDefinition> =
                        guard.values().map(|(def, _)| def.clone()).collect();
                    defs.sort_by(|a, b| a.name.cmp(&b.name));
                    Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": defs
                        }
                    }))
                }
                "tools/call" => {
                    let params = request.get("params").cloned().unwrap_or(json!({}));
                    let tool_name = params
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default();
                    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                    let handler = {
                        let guard = self.tools.read().await;
                        guard.get(tool_name).map(|(_, h)| h.clone())
                    };

                    match handler {
                        Some(h) => match h(arguments).await {
                            Ok(text) => Ok(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [
                                        {
                                            "type": "text",
                                            "text": text
                                        }
                                    ],
                                    "isError": false
                                }
                            })),
                            Err(err) => Ok(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [
                                        {
                                            "type": "text",
                                            "text": err.to_string()
                                        }
                                    ],
                                    "isError": true
                                }
                            })),
                        },
                        None => Ok(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("Tool not found: '{tool_name}'")
                            }
                        })),
                    }
                }
                other => Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: '{other}'")
                    }
                })),
            }
        })
    }
}

/// Client for communicating with an MCP server over an [`McpTransport`].
#[derive(Clone)]
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    next_id: Arc<AtomicU64>,
}

impl McpClient {
    /// Constructs a new [`McpClient`] with the designated transport.
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            transport,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Queries the MCP server for all advertised tool specifications.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDefinition>> {
        let req_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "tools/list",
            "params": {}
        });

        let resp = self.transport.send_request(&req).await?;

        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown JSON-RPC error");
            return Err(KaiError::Tool(ToolError::ExecutionFailed {
                name: "mcp_client".to_string(),
                reason: msg.to_string(),
            }));
        }

        let tools_val = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .cloned()
            .unwrap_or(json!([]));

        let tools: Vec<McpToolDefinition> = serde_json::from_value(tools_val).map_err(|err| {
            KaiError::Tool(ToolError::ExecutionFailed {
                name: "mcp_client".to_string(),
                reason: format!("Failed to parse MCP tools list: {err}"),
            })
        })?;

        Ok(tools)
    }

    /// Invokes a specific remote tool by name with arguments.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpCallResult> {
        let req_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        });

        let resp = self.transport.send_request(&req).await?;

        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown JSON-RPC error");
            return Err(KaiError::Tool(ToolError::ExecutionFailed {
                name: name.to_string(),
                reason: msg.to_string(),
            }));
        }

        let result_val = resp
            .get("result")
            .cloned()
            .unwrap_or(json!({ "content": [], "isError": false }));

        let call_res: McpCallResult = serde_json::from_value(result_val).map_err(|err| {
            KaiError::Tool(ToolError::ExecutionFailed {
                name: name.to_string(),
                reason: format!("Failed to parse MCP call result: {err}"),
            })
        })?;

        Ok(call_res)
    }
}

/// Arguments accepted by [`McpClientTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClientArgs {
    /// Action: "list" or "call".
    pub action: String,
    /// Name of the remote tool to call (required if action is "call").
    pub tool_name: Option<String>,
    /// Arguments for the remote tool invocation.
    pub arguments: Option<serde_json::Value>,
}

/// Tool implementing the KAI [`Tool`] contract for Model Context Protocol invocation.
#[derive(Clone)]
pub struct McpClientTool {
    client: McpClient,
}

impl Default for McpClientTool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClientTool {
    /// Constructs an [`McpClientTool`] using an offline mock transport.
    pub fn new() -> Self {
        Self {
            client: McpClient::new(Arc::new(MockMcpTransport::new())),
        }
    }

    /// Constructs an [`McpClientTool`] with a designated [`McpClient`].
    pub fn with_client(client: McpClient) -> Self {
        Self { client }
    }
}

impl Tool for McpClientTool {
    fn name(&self) -> &str {
        "mcp_client"
    }

    fn description(&self) -> &str {
        "Dispatches tool discovery ('list') and tool invocation ('call') to Model Context Protocol (MCP) servers via JSON-RPC 2.0."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "call"],
                    "description": "MCP operation: 'list' or 'call'"
                },
                "tool_name": {
                    "type": "string",
                    "description": "Remote tool identifier (required for 'call')"
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON arguments payload for the remote tool"
                }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::NetworkAccess
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if !arguments.is_object() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "Arguments must be a valid JSON object".to_string(),
            });
        }

        let args: McpClientArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
            ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: err.to_string(),
            }
        })?;

        match args.action.as_str() {
            "list" => Ok(()),
            "call" => {
                if args.tool_name.as_deref().unwrap_or("").trim().is_empty() {
                    Err(ToolError::InvalidArguments {
                        name: self.name().to_string(),
                        reason: "tool_name is required when action is 'call'".to_string(),
                    })
                } else {
                    Ok(())
                }
            }
            other => Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: format!("Unknown action '{other}', expected 'list' or 'call'"),
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            context.check_cancellation()?;

            let args: McpClientArgs = match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Invalid arguments: {err}"),
                    ));
                }
            };

            match args.action.as_str() {
                "list" => match self.client.list_tools().await {
                    Ok(tools) => {
                        let json_out = serde_json::to_string_pretty(&tools).unwrap_or_default();
                        Ok(ToolResult::success(self.name(), json_out))
                    }
                    Err(err) => Ok(ToolResult::error(
                        self.name(),
                        format!("MCP tools/list failed: {err}"),
                    )),
                },
                "call" => {
                    let tool_name = args.tool_name.as_deref().unwrap_or("");
                    if tool_name.trim().is_empty() {
                        return Ok(ToolResult::error(
                            self.name(),
                            "tool_name is required when action is 'call'",
                        ));
                    }
                    let call_args = args.arguments.unwrap_or(json!({}));

                    match self.client.call_tool(tool_name, call_args).await {
                        Ok(res) => {
                            let text = res.combined_text();
                            if res.is_error {
                                Ok(ToolResult::error(self.name(), text))
                            } else {
                                Ok(ToolResult::success(self.name(), text))
                            }
                        }
                        Err(err) => Ok(ToolResult::error(
                            self.name(),
                            format!("MCP tools/call failed: {err}"),
                        )),
                    }
                }
                other => Err(KaiError::Tool(ToolError::InvalidArguments {
                    name: self.name().to_string(),
                    reason: format!("Unknown action '{other}'"),
                })),
            }
        })
    }
}

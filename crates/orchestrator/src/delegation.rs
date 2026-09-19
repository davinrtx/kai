//! Native tool for delegating tasks to supervised sub-agents.

use std::sync::Arc;

use kai_core::error::Result;
use kai_core::message::{truncate_tool_output, ToolResult};
use kai_core::traits::{BoxFuture, PermissionCategory, TaskDispatcher, Tool, ToolContext};
use serde_json::json;

/// Tool enabling agents to delegate tasks to registered sub-agents in the fleet.
pub struct DelegateTaskTool {
    dispatcher: Arc<dyn TaskDispatcher>,
}

impl DelegateTaskTool {
    /// Constructs a new [`DelegateTaskTool`] wired to a [`TaskDispatcher`].
    pub fn new(dispatcher: Arc<dyn TaskDispatcher>) -> Self {
        Self { dispatcher }
    }
}

impl Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn description(&self) -> &str {
        "Delegates a discrete task or investigation to a supervised sub-agent and returns the result"
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Identifier of the target sub-agent to execute the task"
                },
                "task": {
                    "type": "string",
                    "description": "Detailed instructions and objective for the sub-agent"
                }
            },
            "required": ["agent_id", "task"]
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileRead
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            let agent_id = arguments
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if agent_id.is_empty() {
                return Ok(ToolResult::error(
                    self.name(),
                    "Missing required argument 'agent_id'",
                ));
            }

            let task = arguments
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if task.is_empty() {
                return Ok(ToolResult::error(
                    self.name(),
                    "Missing required argument 'task'",
                ));
            }

            match self.dispatcher.dispatch_task(agent_id, task, context).await {
                Ok(output) => {
                    let truncated = truncate_tool_output(&output);
                    Ok(ToolResult::success(self.name(), truncated))
                }
                Err(err) => Ok(ToolResult::error(
                    self.name(),
                    format!("Sub-agent '{agent_id}' execution failed: {err}"),
                )),
            }
        })
    }
}

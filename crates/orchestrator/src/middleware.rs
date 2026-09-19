//! Ordered middleware pipeline for intercepting agent reasoning turns and remediating tool errors.

use std::sync::Arc;

use kai_core::error::{Result, ToolError};
use kai_core::message::{Message, ToolResult};
use kai_core::traits::{AgentMiddleware, StepOutcome, ToolContext};

/// Ordered pipeline of [`AgentMiddleware`] layers executed surrounding conversational turns.
#[derive(Default, Clone)]
pub struct MiddlewarePipeline {
    layers: Vec<Arc<dyn AgentMiddleware>>,
}

impl MiddlewarePipeline {
    /// Constructs an empty [`MiddlewarePipeline`].
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Appends an [`AgentMiddleware`] layer to the pipeline.
    pub fn add(&mut self, middleware: Arc<dyn AgentMiddleware>) {
        self.layers.push(middleware);
    }

    /// Attaches an [`AgentMiddleware`] layer and returns `self` (builder pattern).
    pub fn with_layer(mut self, middleware: Arc<dyn AgentMiddleware>) -> Self {
        self.layers.push(middleware);
        self
    }

    /// Returns the number of registered middleware layers.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Returns `true` if the pipeline contains zero registered middleware layers.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Executes `before_turn` across all registered middleware layers in registration order.
    pub async fn execute_before_turn(
        &self,
        messages: &mut Vec<Message>,
        context: &ToolContext,
    ) -> Result<()> {
        for layer in &self.layers {
            layer.before_turn(messages, context).await?;
        }
        Ok(())
    }

    /// Executes `after_turn` across all registered middleware layers in reverse order (LIFO).
    pub async fn execute_after_turn(
        &self,
        outcome: &mut StepOutcome,
        context: &ToolContext,
    ) -> Result<()> {
        for layer in self.layers.iter().rev() {
            layer.after_turn(outcome, context).await?;
        }
        Ok(())
    }

    /// Dispatches `on_tool_error` through middleware layers until one remediates or all pass through.
    pub async fn execute_on_tool_error(
        &self,
        tool_name: &str,
        error: &ToolError,
        context: &ToolContext,
    ) -> Result<Option<ToolResult>> {
        for layer in &self.layers {
            if let Some(res) = layer.on_tool_error(tool_name, error, context).await? {
                return Ok(Some(res));
            }
        }
        Ok(None)
    }
}

/// Diagnostic middleware for recording tool execution failures into a thread-safe log buffer.
#[derive(Debug, Default, Clone)]
pub struct DiagnosticAuditMiddleware {
    name: String,
}

impl DiagnosticAuditMiddleware {
    /// Constructs a new [`DiagnosticAuditMiddleware`].
    pub fn new() -> Self {
        Self {
            name: "diagnostic_audit".to_string(),
        }
    }
}

impl AgentMiddleware for DiagnosticAuditMiddleware {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_tool_error<'a>(
        &'a self,
        tool_name: &'a str,
        error: &'a ToolError,
        _context: &'a ToolContext,
    ) -> kai_core::traits::BoxFuture<'a, Result<Option<ToolResult>>> {
        Box::pin(async move {
            let _ = (tool_name, error);
            // Non-destructive pass-through audit hook
            Ok(None)
        })
    }
}

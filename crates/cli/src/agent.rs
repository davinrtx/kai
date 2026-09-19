//! Concrete LLM-backed agent implementation of [`kai_core::traits::Agent`].
//!
//! Maintains conversational history, builds OpenAI-compatible completion requests,
//! and maps model responses into deterministic [`StepOutcome`] actions.

use std::sync::Arc;

use kai_core::error::{KaiError, OrchestratorError, Result};
use kai_core::message::{current_timestamp_ms, ContentBlock, Message, Role};
use kai_core::traits::{Agent, BoxFuture, StepOutcome};
use serde_json::Value;

use crate::client::ModelClient;

/// LLM-powered autonomous engineering agent.
pub struct LlmAgent {
    id: String,
    name: String,
    system_prompt: String,
    client: Arc<ModelClient>,
    tool_schemas: Vec<Value>,
    history: Vec<Message>,
}

impl LlmAgent {
    /// Constructs a new [`LlmAgent`].
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        system_prompt: impl Into<String>,
        client: Arc<ModelClient>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            system_prompt: system_prompt.into(),
            client,
            tool_schemas: Vec::new(),
            history: Vec::new(),
        }
    }

    /// Attaches available tool JSON schemas to the agent.
    pub fn with_tool_schemas(mut self, tool_schemas: Vec<Value>) -> Self {
        self.tool_schemas = tool_schemas;
        self
    }

    /// Returns a reference to the internal conversational history.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Clears the conversational turn history while preserving configuration.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

impl Agent for LlmAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn tool_schemas(&self) -> Vec<Value> {
        self.tool_schemas.clone()
    }

    fn step<'a>(&'a mut self, inbox: &'a [Message]) -> BoxFuture<'a, Result<StepOutcome>> {
        Box::pin(async move {
            // Append incoming inbox messages to local turn history
            for msg in inbox {
                self.history.push(msg.clone());
            }

            // Dispatch completion request to model endpoint
            let response = self
                .client
                .complete(&self.system_prompt, &self.history, &self.tool_schemas)
                .await
                .map_err(|err| {
                    KaiError::Orchestrator(OrchestratorError::Interrupted {
                        reason: format!("Model inference failure: {err}"),
                    })
                })?;

            let timestamp = current_timestamp_ms();

            if !response.tool_calls.is_empty() {
                // Construct assistant message with text + tool calls
                let mut content_blocks = Vec::with_capacity(response.tool_calls.len() + 1);

                if let Some(text) = response.text {
                    if !text.trim().is_empty() {
                        content_blocks.push(ContentBlock::text(text));
                    }
                }

                for call in response.tool_calls {
                    content_blocks.push(ContentBlock::ToolUse(call));
                }

                let assistant_msg = Message::new(
                    format!("msg_asst_{timestamp}"),
                    Role::Assistant,
                    content_blocks,
                );

                self.history.push(assistant_msg.clone());
                Ok(StepOutcome::Continue(vec![assistant_msg]))
            } else {
                // Assistant reached completion with final message
                let text = response
                    .text
                    .unwrap_or_else(|| "Task completed without text output.".to_string());

                let assistant_msg = Message::assistant(format!("msg_asst_{timestamp}"), text);

                self.history.push(assistant_msg.clone());
                Ok(StepOutcome::Completed(assistant_msg))
            }
        })
    }
}

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
    show_reasoning: bool,
    accumulated_tokens: usize,
    last_turn_tokens: usize,
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
            show_reasoning: false,
            accumulated_tokens: 0,
            last_turn_tokens: 0,
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

    /// Restores conversational turn history from a resumed session.
    pub fn restore_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// Clears the conversational turn history while preserving configuration.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Returns the active inference model identifier.
    pub fn model(&self) -> &str {
        self.client.model()
    }

    /// Returns the target inference endpoint base URL.
    pub fn base_url(&self) -> &str {
        self.client.base_url()
    }

    /// Returns the active inference API key, if configured.
    pub fn api_key(&self) -> Option<&str> {
        self.client.api_key()
    }

    /// Dynamically switches the model identifier and optionally the base URL.
    pub fn set_model(&mut self, model: impl Into<String>, base_url: Option<String>) {
        let endpoint = base_url.unwrap_or_else(|| self.client.base_url().to_string());
        self.client = Arc::new(ModelClient::with_transport(
            self.client.transport().clone(),
            endpoint,
            model,
            self.client.api_key().map(String::from),
        ));
    }

    /// Dynamically updates the inference API key for the active agent.
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.client = Arc::new(ModelClient::with_transport(
            self.client.transport().clone(),
            self.client.base_url(),
            self.client.model(),
            api_key,
        ));
    }

    /// Returns whether `<think>` reasoning traces should be displayed.
    pub fn show_reasoning(&self) -> bool {
        self.show_reasoning
    }

    /// Toggles the visibility of `<think>` reasoning traces.
    pub fn set_show_reasoning(&mut self, show: bool) {
        self.show_reasoning = show;
    }

    /// Returns the total accumulated token count consumed across turns.
    pub fn accumulated_tokens(&self) -> usize {
        self.accumulated_tokens
    }

    /// Returns the token count consumed in the most recent turn.
    pub fn last_turn_tokens(&self) -> usize {
        self.last_turn_tokens
    }

    /// Records token consumption into the running metrics.
    pub fn record_tokens(&mut self, tokens: usize) {
        self.last_turn_tokens = tokens;
        self.accumulated_tokens += tokens;
    }

    /// Removes the trailing user message if a turn was cancelled before completion.
    pub fn pop_last_if_user(&mut self) -> Option<Message> {
        if let Some(last) = self.history.last() {
            if last.role == Role::User {
                return self.history.pop();
            }
        }
        None
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

            let mut spinner =
                crate::ui::Spinner::start(format!("Waiting for {}", self.client.model()));

            // Dispatch completion request to model endpoint
            let response_result = self
                .client
                .complete(&self.system_prompt, &self.history, &self.tool_schemas)
                .await
                .map_err(|err| {
                    KaiError::Orchestrator(OrchestratorError::Interrupted {
                        reason: format!("Model inference failure: {err}"),
                    })
                });

            spinner.stop();
            let response = response_result?;

            let timestamp = current_timestamp_ms();

            let tokens = if let Some(usage) = response.usage {
                usage.total_tokens
            } else {
                let prompt_chars: usize = self.system_prompt.len()
                    + self
                        .history
                        .iter()
                        .map(|m| m.text_content().len())
                        .sum::<usize>();
                let completion_chars = response.text.as_deref().map(|t| t.len()).unwrap_or(0);
                (prompt_chars + completion_chars) / 4
            };
            self.last_turn_tokens = tokens;
            self.accumulated_tokens += tokens;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelClient;
    use std::sync::Arc;

    #[test]
    fn test_pop_last_if_user() {
        let client = Arc::new(ModelClient::new(
            "http://localhost:8000",
            "test-model",
            None,
        ));
        let mut agent = LlmAgent::new("agent-1", "Test Agent", "System", client);

        assert!(agent.pop_last_if_user().is_none());

        agent.history.push(Message::user("msg-1", "User prompt"));
        let popped = agent.pop_last_if_user();
        assert!(popped.is_some());
        assert_eq!(popped.unwrap().text_content(), "User prompt");
        assert!(agent.pop_last_if_user().is_none());

        agent
            .history
            .push(Message::assistant("msg-2", "Assistant reply"));
        assert!(agent.pop_last_if_user().is_none());
        assert_eq!(agent.history.len(), 1);
    }
}

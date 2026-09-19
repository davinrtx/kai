//! OpenAI-compatible inference HTTP client and tool-calling protocol serializer.
//!
//! Connects to local endpoints (Ollama, vLLM, LocalAI) and remote providers (OpenAI, Groq),
//! transforming KAI messages and tool schemas into the standard chat completions payload.

use std::sync::Arc;

use kai_core::message::{ContentBlock, Message, Role, ToolCall};
use kai_core::traits::BoxFuture;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::error::{CliError, Result};

/// Token consumption metrics reported by the inference provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Number of tokens in the prompt.
    pub prompt_tokens: usize,
    /// Number of tokens generated in the completion.
    pub completion_tokens: usize,
    /// Total token count.
    pub total_tokens: usize,
}

/// Structured response received from a chat completions endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatResponse {
    /// Textual content returned by the assistant, if any.
    pub text: Option<String>,
    /// Tool call requests issued by the assistant.
    pub tool_calls: Vec<ToolCall>,
    /// Reason reported for completion termination (`stop`, `tool_calls`, `length`).
    pub finish_reason: Option<String>,
    /// Token consumption metrics, if provided by the inference endpoint.
    pub usage: Option<TokenUsage>,
}

/// Abstract transport contract for dispatching inference HTTP requests.
///
/// Enables seamless offline unit and integration testing via in-memory mock transports.
pub trait LlmTransport: Send + Sync {
    /// Dispatches a JSON request payload to the inference endpoint.
    fn send_request<'a>(
        &'a self,
        url: &'a str,
        api_key: Option<&'a str>,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<Value>>;
}

/// Standard production HTTP transport backed by `reqwest`.
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    /// Constructs a new [`HttpTransport`].
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmTransport for HttpTransport {
    fn send_request<'a>(
        &'a self,
        url: &'a str,
        api_key: Option<&'a str>,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

            if let Some(key) = api_key {
                if !key.trim().is_empty() {
                    let auth_val = format!("Bearer {}", key.trim());
                    if let Ok(val) = HeaderValue::from_str(&auth_val) {
                        headers.insert(AUTHORIZATION, val);
                    }
                }
            }

            let response = self
                .client
                .post(url)
                .headers(headers)
                .json(payload)
                .send()
                .await
                .map_err(CliError::Http)?;

            let status = response.status();
            if !status.is_success() {
                let status_code = status.as_u16();
                let error_body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Failed to read response body".to_string());
                return Err(CliError::Api {
                    status: status_code,
                    message: error_body,
                });
            }

            let body: Value = response.json().await.map_err(CliError::Http)?;
            Ok(body)
        })
    }
}

/// Client coordinating chat completion requests and tool schemas against an inference API.
pub struct ModelClient {
    base_url: String,
    model: String,
    api_key: Option<String>,
    transport: Arc<dyn LlmTransport>,
}

impl ModelClient {
    /// Constructs a [`ModelClient`] backed by the default [`HttpTransport`].
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key,
            transport: Arc::new(HttpTransport::new()),
        }
    }

    /// Constructs a [`ModelClient`] with a custom or mock transport.
    pub fn with_transport(
        transport: Arc<dyn LlmTransport>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key,
            transport,
        }
    }

    /// Returns the target inference endpoint base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the active inference model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the configured authorization API key, if present.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Returns a reference to the active transport driver.
    pub fn transport(&self) -> &Arc<dyn LlmTransport> {
        &self.transport
    }

    /// Translates conversational messages into standard OpenAI chat completion JSON.
    pub fn format_messages(system_prompt: &str, messages: &[Message]) -> Vec<Value> {
        let mut formatted = Vec::with_capacity(messages.len() + 1);

        // Prepend system prompt if non-empty
        if !system_prompt.trim().is_empty() {
            formatted.push(json!({
                "role": "system",
                "content": system_prompt
            }));
        }

        for msg in messages {
            match msg.role {
                Role::System => {
                    formatted.push(json!({
                        "role": "system",
                        "content": msg.text_content()
                    }));
                }
                Role::User => {
                    formatted.push(json!({
                        "role": "user",
                        "content": msg.text_content()
                    }));
                }
                Role::Assistant => {
                    let tool_calls = msg.tool_call_blocks();
                    if tool_calls.is_empty() {
                        formatted.push(json!({
                            "role": "assistant",
                            "content": msg.text_content()
                        }));
                    } else {
                        let formatted_calls: Vec<Value> = tool_calls
                            .into_iter()
                            .map(|call| {
                                json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments.to_string()
                                    }
                                })
                            })
                            .collect();

                        let text = msg.text_content();
                        let content_val = if text.is_empty() {
                            Value::Null
                        } else {
                            Value::String(text)
                        };

                        formatted.push(json!({
                            "role": "assistant",
                            "content": content_val,
                            "tool_calls": formatted_calls
                        }));
                    }
                }
                Role::Tool => {
                    for block in &msg.content {
                        if let ContentBlock::ToolResult(res) = block {
                            formatted.push(json!({
                                "role": "tool",
                                "tool_call_id": res.tool_call_id,
                                "content": res.output
                            }));
                        }
                    }
                }
            }
        }

        formatted
    }

    /// Formats tool definitions into the OpenAI function tools schema.
    pub fn format_tools(tool_schemas: &[Value]) -> Vec<Value> {
        tool_schemas
            .iter()
            .map(|schema| {
                json!({
                    "type": "function",
                    "function": schema
                })
            })
            .collect()
    }

    /// Dispatches a completion request and parses the resulting text and tool calls.
    pub async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tool_schemas: &[Value],
    ) -> Result<ChatResponse> {
        let endpoint = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let formatted_messages = Self::format_messages(system_prompt, messages);
        let mut payload = json!({
            "model": self.model,
            "messages": formatted_messages,
            "temperature": 0.0,
        });

        if !tool_schemas.is_empty() {
            let formatted_tools = Self::format_tools(tool_schemas);
            if let Some(map) = payload.as_object_mut() {
                map.insert("tools".to_string(), Value::Array(formatted_tools));
            }
        }

        let response_json = self
            .transport
            .send_request(&endpoint, self.api_key.as_deref(), &payload)
            .await?;

        Self::parse_response(&response_json)
    }

    /// Parses the raw JSON response returned by the chat completions API.
    pub fn parse_response(response: &Value) -> Result<ChatResponse> {
        let choices = response
            .get("choices")
            .and_then(|c| c.as_array())
            .ok_or_else(|| {
                CliError::Configuration(format!(
                    "Missing 'choices' array in API response: {response}"
                ))
            })?;

        let first_choice = choices.first().ok_or_else(|| {
            CliError::Configuration("Empty 'choices' array received from inference API".to_string())
        })?;

        let finish_reason = first_choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());

        let message = first_choice
            .get("message")
            .ok_or_else(|| CliError::Configuration("Missing 'message' in choice".to_string()))?;

        let text = message
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let mut tool_calls = Vec::new();

        if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
            for call_val in calls {
                let id = call_val
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("call_default")
                    .to_string();

                let function = call_val.get("function").ok_or_else(|| {
                    CliError::Configuration("Tool call missing 'function' object".to_string())
                })?;

                let name = function
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| {
                        CliError::Configuration(
                            "Tool function missing 'name' identifier".to_string(),
                        )
                    })?
                    .to_string();

                let args_val = match function.get("arguments") {
                    Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                        .unwrap_or(Value::Object(Default::default())),
                    Some(val @ Value::Object(_)) => val.clone(),
                    _ => Value::Object(Default::default()),
                };

                tool_calls.push(ToolCall::new(id, name, args_val));
            }
        }

        let usage = response.get("usage").map(|u| {
            let prompt_tokens =
                u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let completion_tokens = u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let total_tokens = u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: if total_tokens > 0 {
                    total_tokens
                } else {
                    prompt_tokens + completion_tokens
                },
            }
        });

        Ok(ChatResponse {
            text,
            tool_calls,
            finish_reason,
            usage,
        })
    }
}

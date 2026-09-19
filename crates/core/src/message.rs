//! Message schemas and conversational data structures.
//!
//! Provides the canonical representation for conversational turns,
//! content blocks, tool requests, execution responses, and token metrics.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::ToolError;

/// Maximum allowed output size in bytes for tool execution results (4 KB).
pub const MAX_TOOL_OUTPUT_BYTES: usize = 4096;

/// Maximum allowed items for list-based tool outputs.
pub const MAX_TOOL_OUTPUT_ITEMS: usize = 50;

/// Truncation notice appended when tool output exceeds [`MAX_TOOL_OUTPUT_BYTES`].
pub const TRUNCATION_BYTE_NOTICE: &str = "\n[Truncated: exceeded 4 KB limit. Refine query]";

/// Returns the current system time in milliseconds since UNIX epoch.
pub fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Truncates a tool output string to ensure it strictly respects [`MAX_TOOL_OUTPUT_BYTES`].
///
/// If `raw_output` exceeds 4 KB, it is safely truncated at a valid UTF-8 character boundary
/// and appended with a truncation notice.
pub fn truncate_output(raw_output: &str) -> String {
    if raw_output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return raw_output.to_string();
    }

    let max_content_len = MAX_TOOL_OUTPUT_BYTES.saturating_sub(TRUNCATION_BYTE_NOTICE.len());
    let mut boundary = max_content_len;
    while boundary > 0 && !raw_output.is_char_boundary(boundary) {
        boundary -= 1;
    }

    let mut truncated = String::with_capacity(boundary + TRUNCATION_BYTE_NOTICE.len());
    truncated.push_str(&raw_output[..boundary]);
    truncated.push_str(TRUNCATION_BYTE_NOTICE);
    truncated
}

/// Truncates a tool output string to ensure it strictly respects both the
/// [`MAX_TOOL_OUTPUT_ITEMS`] (50 lines) cap and [`MAX_TOOL_OUTPUT_BYTES`] (4 KB) cap.
///
/// Limits line count to 50 lines using a lazy bounded iterator (never allocating O(N) heap),
/// caps individual line inspection to avoid large single-line allocations,
/// and guarantees the final output never exceeds 4 KB at UTF-8 boundaries.
pub fn truncate_tool_output(raw_output: &str) -> String {
    if raw_output.is_empty() {
        return String::new();
    }

    let mut line_iter = raw_output.lines();
    let mut kept = Vec::with_capacity(MAX_TOOL_OUTPUT_ITEMS.min(64));
    let mut accumulated_bytes = 0;

    for _ in 0..MAX_TOOL_OUTPUT_ITEMS {
        if let Some(line) = line_iter.next() {
            let line_len = line.len();
            // If any single line exceeds MAX_TOOL_OUTPUT_BYTES, truncate it immediately
            if line_len > MAX_TOOL_OUTPUT_BYTES {
                kept.push(truncate_output(line));
                break;
            } else {
                accumulated_bytes += line_len + 1;
                kept.push(line.to_string());
                if accumulated_bytes >= MAX_TOOL_OUTPUT_BYTES {
                    break;
                }
            }
        } else {
            break;
        }
    }

    let remaining = line_iter.count();
    let content = if remaining > 0 {
        format!(
            "{}\n[Truncated: {} remaining items. Refine query]",
            kept.join("\n"),
            remaining
        )
    } else {
        kept.join("\n")
    };

    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        content
    } else {
        truncate_output(&content)
    }
}

/// Truncates a slice of items to [`MAX_TOOL_OUTPUT_ITEMS`], returning the bounded items
/// along with an optional notice specifying the count of omitted items.
pub fn truncate_items<T: std::fmt::Display>(items: &[T]) -> (Vec<String>, Option<String>) {
    if items.len() <= MAX_TOOL_OUTPUT_ITEMS {
        let string_items = items.iter().map(|item| item.to_string()).collect();
        return (string_items, None);
    }

    let string_items = items[..MAX_TOOL_OUTPUT_ITEMS]
        .iter()
        .map(|item| item.to_string())
        .collect();
    let remaining = items.len() - MAX_TOOL_OUTPUT_ITEMS;
    let notice = format!("[Truncated: {} remaining items. Refine query]", remaining);
    (string_items, Some(notice))
}

/// Quantified token usage metrics reported by an inference provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Number of prompt tokens processed.
    pub prompt_tokens: usize,
    /// Number of generated completion tokens.
    pub completion_tokens: usize,
    /// Total tokens consumed (prompt + completion).
    pub total_tokens: usize,
}

impl TokenUsage {
    /// Constructs a new [`TokenUsage`] record.
    pub fn new(prompt_tokens: usize, completion_tokens: usize) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
        }
    }
}

/// Conversational participant role in an agent interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System-level instructional prompt.
    System,
    /// Human end-user or driver.
    User,
    /// Autonomous model assistant.
    Assistant,
    /// Tool execution response.
    Tool,
}

/// Invocation request for an executable tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call instance.
    pub id: String,
    /// Canonical name of the targeted tool.
    pub name: String,
    /// Structured parameters passed to the tool.
    pub arguments: serde_json::Value,
}

impl ToolCall {
    /// Creates a new [`ToolCall`] with the specified ID, name, and arguments.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    /// Deserializes the JSON arguments into a strongly typed target struct.
    pub fn parse_arguments<T: serde::de::DeserializeOwned>(&self) -> Result<T, ToolError> {
        serde_json::from_value(self.arguments.clone()).map_err(|err| ToolError::InvalidArguments {
            name: self.name.clone(),
            reason: err.to_string(),
        })
    }
}

/// Structured outcome returned by an executed tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Correlation identifier matching the corresponding [`ToolCall::id`].
    pub tool_call_id: String,
    /// Output produced by the tool execution (strictly capped at 4 KB and 50 lines).
    pub output: String,
    /// Indicates whether the tool execution resulted in an error.
    pub is_error: bool,
    /// Subprocess exit status code, if execution was an external command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Measured execution duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl ToolResult {
    /// Creates a successful [`ToolResult`], applying dual truncation (4 KB and 50 lines).
    pub fn success(tool_call_id: impl Into<String>, output: impl AsRef<str>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            output: truncate_tool_output(output.as_ref()),
            is_error: false,
            exit_code: None,
            duration_ms: None,
        }
    }

    /// Creates an error [`ToolResult`], applying dual truncation (4 KB and 50 lines).
    pub fn error(tool_call_id: impl Into<String>, output: impl AsRef<str>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            output: truncate_tool_output(output.as_ref()),
            is_error: true,
            exit_code: None,
            duration_ms: None,
        }
    }

    /// Creates an unconstrained [`ToolResult`] without applying automatic truncation.
    pub fn raw(tool_call_id: impl Into<String>, output: impl Into<String>, is_error: bool) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            output: output.into(),
            is_error,
            exit_code: None,
            duration_ms: None,
        }
    }

    /// Sets the subprocess exit status code.
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    /// Sets the measured execution duration in milliseconds.
    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }
}

/// Polymorphic content block contained within a conversational message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content.
    Text {
        /// Textual payload.
        text: String,
    },
    /// Internal reasoning or chain-of-thought generated by the model.
    Thinking {
        /// Reasoning trace text.
        thoughts: String,
        /// Optional cryptographic verification signature required by providers for tool turns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Visual or binary image content.
    Image {
        /// MIME media type (e.g. "image/png", "image/jpeg").
        media_type: String,
        /// Base64-encoded image payload or sandbox-confined file path.
        data: String,
    },
    /// Binary or structured document content (e.g. PDF, CSV, markdown attachment).
    Document {
        /// MIME media type (e.g. "application/pdf").
        media_type: String,
        /// Base64-encoded document payload or sandbox-confined file path.
        data: String,
    },
    /// Request to invoke a tool.
    ToolUse(ToolCall),
    /// Result received from a tool execution.
    ToolResult(ToolResult),
}

impl ContentBlock {
    /// Creates a text content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Creates a thinking / chain-of-thought content block without signature.
    pub fn thinking(thoughts: impl Into<String>) -> Self {
        Self::Thinking {
            thoughts: thoughts.into(),
            signature: None,
        }
    }

    /// Creates a thinking / chain-of-thought content block with a verification signature.
    pub fn thinking_with_signature(
        thoughts: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self::Thinking {
            thoughts: thoughts.into(),
            signature: Some(signature.into()),
        }
    }

    /// Creates an image content block.
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    /// Creates a document content block.
    pub fn document(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Document {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    /// Returns a reference to the inner text if this block is [`ContentBlock::Text`].
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }

    /// Returns a reference to the inner thoughts if this block is [`ContentBlock::Thinking`].
    pub fn as_thinking(&self) -> Option<&str> {
        match self {
            Self::Thinking { thoughts, .. } => Some(thoughts.as_str()),
            _ => None,
        }
    }

    /// Returns the thinking block's verification signature if present.
    pub fn thinking_signature(&self) -> Option<&str> {
        match self {
            Self::Thinking { signature, .. } => signature.as_deref(),
            _ => None,
        }
    }

    /// Returns references to the media type and data if this block is [`ContentBlock::Image`].
    pub fn as_image(&self) -> Option<(&str, &str)> {
        match self {
            Self::Image { media_type, data } => Some((media_type.as_str(), data.as_str())),
            _ => None,
        }
    }

    /// Returns references to the media type and data if this block is [`ContentBlock::Document`].
    pub fn as_document(&self) -> Option<(&str, &str)> {
        match self {
            Self::Document { media_type, data } => Some((media_type.as_str(), data.as_str())),
            _ => None,
        }
    }

    /// Returns a reference to the inner [`ToolCall`] if this block is [`ContentBlock::ToolUse`].
    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolUse(call) => Some(call),
            _ => None,
        }
    }

    /// Returns a reference to the inner [`ToolResult`] if this block is [`ContentBlock::ToolResult`].
    pub fn as_tool_result(&self) -> Option<&ToolResult> {
        match self {
            Self::ToolResult(res) => Some(res),
            _ => None,
        }
    }
}

impl From<String> for ContentBlock {
    fn from(text: String) -> Self {
        Self::Text { text }
    }
}

impl From<&str> for ContentBlock {
    fn from(text: &str) -> Self {
        Self::Text {
            text: text.to_string(),
        }
    }
}

impl From<ToolCall> for ContentBlock {
    fn from(call: ToolCall) -> Self {
        Self::ToolUse(call)
    }
}

impl From<ToolResult> for ContentBlock {
    fn from(res: ToolResult) -> Self {
        Self::ToolResult(res)
    }
}

/// Single conversational entry in an agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Unique identifier for this message.
    pub id: String,
    /// Conversational role of the author.
    pub role: Role,
    /// Content blocks comprising the message body.
    pub content: Vec<ContentBlock>,
    /// Timestamp when this message was created (in milliseconds since UNIX epoch).
    pub timestamp_ms: u64,
    /// Optional token consumption metrics reported by inference engine for this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// Optional metadata associated with the message.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl Message {
    /// Constructs a message with arbitrary role and content blocks, stamped with current time.
    pub fn new(id: impl Into<String>, role: Role, content: Vec<ContentBlock>) -> Self {
        Self {
            id: id.into(),
            role,
            content,
            timestamp_ms: current_timestamp_ms(),
            token_usage: None,
            metadata: HashMap::new(),
        }
    }

    /// Constructs a system message with a single text block.
    pub fn system(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(id, Role::System, vec![ContentBlock::text(text)])
    }

    /// Constructs a user message with a single text block.
    pub fn user(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(id, Role::User, vec![ContentBlock::text(text)])
    }

    /// Constructs an assistant message with a single text block.
    pub fn assistant(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(id, Role::Assistant, vec![ContentBlock::text(text)])
    }

    /// Constructs an assistant message requesting one or more tool calls.
    pub fn tool_calls(id: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        let content = calls.into_iter().map(ContentBlock::ToolUse).collect();
        Self::new(id, Role::Assistant, content)
    }

    /// Constructs a tool response message with execution results.
    pub fn tool_results(id: impl Into<String>, results: Vec<ToolResult>) -> Self {
        let content = results.into_iter().map(ContentBlock::ToolResult).collect();
        Self::new(id, Role::Tool, content)
    }

    /// Overrides the message creation timestamp.
    pub fn with_timestamp_ms(mut self, timestamp_ms: u64) -> Self {
        self.timestamp_ms = timestamp_ms;
        self
    }

    /// Attaches token consumption metrics to this message.
    pub fn with_token_usage(mut self, token_usage: TokenUsage) -> Self {
        self.token_usage = Some(token_usage);
        self
    }

    /// Extracts and concatenates all text blocks in this message.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Collects references to all thoughts blocks in this message.
    pub fn thinking_blocks(&self) -> Vec<&str> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_thinking)
            .collect()
    }

    /// Collects references to all image blocks in this message as (media_type, data).
    pub fn image_blocks(&self) -> Vec<(&str, &str)> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_image)
            .collect()
    }

    /// Collects references to all document blocks in this message as (media_type, data).
    pub fn document_blocks(&self) -> Vec<(&str, &str)> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_document)
            .collect()
    }

    /// Collects references to all [`ToolCall`] blocks in this message.
    pub fn tool_call_blocks(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_tool_call)
            .collect()
    }

    /// Collects references to all [`ToolResult`] blocks in this message.
    pub fn tool_result_blocks(&self) -> Vec<&ToolResult> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_tool_result)
            .collect()
    }

    /// Appends a key-value metadata entry to the message.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Appends an arbitrary content block to the message.
    pub fn with_block(mut self, block: impl Into<ContentBlock>) -> Self {
        self.content.push(block.into());
        self
    }

    /// Appends a text content block to the message.
    pub fn with_text(self, text: impl Into<String>) -> Self {
        self.with_block(ContentBlock::text(text))
    }

    /// Appends an image content block to the message.
    pub fn with_image(self, media_type: impl Into<String>, data: impl Into<String>) -> Self {
        self.with_block(ContentBlock::image(media_type, data))
    }

    /// Appends a document content block to the message.
    pub fn with_document(self, media_type: impl Into<String>, data: impl Into<String>) -> Self {
        self.with_block(ContentBlock::document(media_type, data))
    }

    /// Appends a thinking / reasoning content block to the message.
    pub fn with_thinking(self, thoughts: impl Into<String>) -> Self {
        self.with_block(ContentBlock::thinking(thoughts))
    }
}

/// Forensic diagnostic reflection recorded when a session DAG branch fails and rolls back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureTombstone {
    /// Branch name or identifier where the failure occurred.
    pub failed_branch: String,
    /// Identifier of the specific node that triggered the failure.
    pub failed_node_id: String,
    /// Trigger action or tool invocation that caused the failure.
    pub trigger_action: String,
    /// Raw diagnostic error trace or compiler assertion.
    pub error_signature: String,
    /// Structured technical root-cause synthesis.
    pub root_cause_analysis: String,
    /// Negative constraints that subsequent turns and alternative branches must avoid.
    pub negative_constraints: Vec<String>,
}

impl FailureTombstone {
    /// Constructs a new [`FailureTombstone`].
    pub fn new(
        failed_branch: impl Into<String>,
        failed_node_id: impl Into<String>,
        trigger_action: impl Into<String>,
        error_signature: impl Into<String>,
        root_cause_analysis: impl Into<String>,
    ) -> Self {
        Self {
            failed_branch: failed_branch.into(),
            failed_node_id: failed_node_id.into(),
            trigger_action: trigger_action.into(),
            error_signature: error_signature.into(),
            root_cause_analysis: root_cause_analysis.into(),
            negative_constraints: Vec::new(),
        }
    }

    /// Attaches negative constraints to this tombstone.
    pub fn with_negative_constraints(mut self, constraints: Vec<String>) -> Self {
        self.negative_constraints = constraints;
        self
    }

    /// Formats the tombstone into a high-priority negative context prompt block.
    pub fn format_as_negative_prompt(&self) -> String {
        let mut out = format!(
            "--- NEGATIVE CONSTRAINT: PREVIOUS BRANCH FAILURE ('{}') ---\n\
            Trigger: {}\n\
            Signature: {}\n\
            Root Cause: {}\n\
            Forbidden Approaches:\n",
            self.failed_branch, self.trigger_action, self.error_signature, self.root_cause_analysis
        );
        for constraint in &self.negative_constraints {
            out.push_str(&format!("  - {constraint}\n"));
        }
        out.push_str("--- END CONSTRAINT ---");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Deserialize, PartialEq, Debug)]
    struct MockArgs {
        path: String,
        offset: usize,
    }

    #[test]
    fn test_role_serialization() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }

    #[test]
    fn test_tool_call_parse_arguments() {
        let call = ToolCall::new(
            "call_01",
            "read_file",
            json!({"path": "lib.rs", "offset": 10}),
        );
        let parsed: MockArgs = call.parse_arguments().expect("parse should succeed");
        assert_eq!(
            parsed,
            MockArgs {
                path: "lib.rs".to_string(),
                offset: 10
            }
        );

        let invalid_call = ToolCall::new("call_02", "read_file", json!({"offset": "bad_type"}));
        let err = invalid_call.parse_arguments::<MockArgs>().unwrap_err();
        match err {
            ToolError::InvalidArguments { name, .. } => assert_eq!(name, "read_file"),
            _ => panic!("expected InvalidArguments error"),
        }
    }

    #[test]
    fn test_image_block_serialization() {
        let block = ContentBlock::image("image/png", "base64data...");
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(serialized.contains("\"type\":\"image\""));
        assert!(serialized.contains("\"media_type\":\"image/png\""));

        let deserialized: ContentBlock = serde_json::from_str(&serialized).unwrap();
        assert_eq!(block, deserialized);
        assert_eq!(
            deserialized.as_image(),
            Some(("image/png", "base64data..."))
        );
    }

    #[test]
    fn test_document_block_serialization() {
        let block = ContentBlock::document("application/pdf", "JVBERi0xLjQK...");
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(serialized.contains("\"type\":\"document\""));
        assert!(serialized.contains("\"media_type\":\"application/pdf\""));

        let deserialized: ContentBlock = serde_json::from_str(&serialized).unwrap();
        assert_eq!(block, deserialized);
        assert_eq!(
            deserialized.as_document(),
            Some(("application/pdf", "JVBERi0xLjQK..."))
        );

        let msg = Message::user("m_doc", "").with_document("application/pdf", "raw_pdf_bytes");
        assert_eq!(
            msg.document_blocks(),
            vec![("application/pdf", "raw_pdf_bytes")]
        );
    }

    #[test]
    fn test_thinking_block_with_signature() {
        let block = ContentBlock::thinking_with_signature(
            "Analyzing memory footprints...",
            "sig_verify_123",
        );
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(serialized.contains("\"type\":\"thinking\""));
        assert!(serialized.contains("\"signature\":\"sig_verify_123\""));

        let deserialized: ContentBlock = serde_json::from_str(&serialized).unwrap();
        assert_eq!(block, deserialized);
        assert_eq!(deserialized.thinking_signature(), Some("sig_verify_123"));
    }

    #[test]
    fn test_truncation_cap_at_4kb() {
        let small_output = "Line 1: ok";
        assert_eq!(truncate_output(small_output), small_output);

        let large_output = "A".repeat(5000);
        let truncated = truncate_output(&large_output);
        assert!(truncated.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(truncated.contains("[Truncated: exceeded 4 KB limit. Refine query]"));

        let tool_res = ToolResult::success("call_over", large_output);
        assert!(tool_res.output.len() <= MAX_TOOL_OUTPUT_BYTES);
    }

    #[test]
    fn test_item_truncation_cap_at_50() {
        let items: Vec<usize> = (1..=60).collect();
        let (truncated, notice) = truncate_items(&items);
        assert_eq!(truncated.len(), 50);
        assert_eq!(
            notice.as_deref(),
            Some("[Truncated: 10 remaining items. Refine query]")
        );

        let small_items = vec!["file1.rs", "file2.rs"];
        let (small_res, small_notice) = truncate_items(&small_items);
        assert_eq!(small_res.len(), 2);
        assert_eq!(small_notice, None);
    }

    #[test]
    fn test_truncate_tool_output_massive_single_line() {
        // Massive 50,000 byte single line (no newlines)
        let huge_line = "B".repeat(50_000);
        let truncated = truncate_tool_output(&huge_line);
        assert!(truncated.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(truncated.contains("[Truncated: exceeded 4 KB limit. Refine query]"));
    }

    #[test]
    fn test_message_roundtrip_json() {
        let tool_call = ToolCall::new(
            "call_123",
            "read_window",
            json!({"path": "src/main.rs", "offset": 1, "limit": 100}),
        );
        let msg = Message::new(
            "msg_001",
            Role::Assistant,
            vec![
                ContentBlock::thinking("I need to inspect main.rs"),
                ContentBlock::text("Inspecting main entry point."),
                ContentBlock::image("image/png", "img_data"),
                ContentBlock::ToolUse(tool_call),
            ],
        )
        .with_timestamp_ms(1726650000000)
        .with_token_usage(TokenUsage::new(120, 45))
        .with_metadata("model", "test-driver");

        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&serialized).unwrap();

        assert_eq!(msg, deserialized);
        assert_eq!(deserialized.timestamp_ms, 1726650000000);
        assert_eq!(deserialized.token_usage, Some(TokenUsage::new(120, 45)));
        assert_eq!(deserialized.text_content(), "Inspecting main entry point.");
        assert_eq!(
            deserialized.thinking_blocks(),
            vec!["I need to inspect main.rs"]
        );
        assert_eq!(deserialized.image_blocks(), vec![("image/png", "img_data")]);
        assert_eq!(deserialized.tool_call_blocks().len(), 1);
        assert_eq!(deserialized.tool_call_blocks()[0].name, "read_window");
    }

    #[test]
    fn test_tool_result_helpers() {
        let success = ToolResult::success("call_1", "file content")
            .with_exit_code(0)
            .with_duration_ms(45);
        assert!(!success.is_error);
        assert_eq!(success.output, "file content");
        assert_eq!(success.exit_code, Some(0));
        assert_eq!(success.duration_ms, Some(45));

        let failure = ToolResult::error("call_2", "file not found").with_exit_code(1);
        assert!(failure.is_error);
        assert_eq!(failure.output, "file not found");
        assert_eq!(failure.exit_code, Some(1));

        let msg = Message::tool_results("msg_002", vec![success, failure]);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_result_blocks().len(), 2);
    }

    #[test]
    fn test_truncate_tool_output_dual_invariants() {
        // Case 1: 100 short lines (total bytes < 4 KB, but lines > 50)
        let lines_100 = (1..=100)
            .map(|i| format!("item_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result_lines = truncate_tool_output(&lines_100);
        assert!(result_lines.contains("[Truncated: 50 remaining items. Refine query]"));
        assert!(result_lines.lines().count() <= 51); // 50 items + 1 notice line

        // Case 2: 5 long lines (total bytes > 4 KB, lines <= 50)
        let long_line = "x".repeat(1000);
        let text_5kb = format!("{long_line}\n{long_line}\n{long_line}\n{long_line}\n{long_line}");
        let result_bytes = truncate_tool_output(&text_5kb);
        assert!(result_bytes.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(result_bytes.contains("[Truncated: exceeded 4 KB limit. Refine query]"));

        // Case 3: ToolResult::success automatically enforces the dual cap
        let res = ToolResult::success("call_dual", lines_100);
        assert!(res
            .output
            .contains("[Truncated: 50 remaining items. Refine query]"));
    }

    #[test]
    fn test_message_builder_chain() {
        let msg = Message::new("msg_b", Role::Assistant, vec![])
            .with_thinking("Planning execution")
            .with_text("Here is the result")
            .with_image("image/png", "base64...")
            .with_metadata("source", "builder");

        assert_eq!(msg.content.len(), 3);
        assert_eq!(msg.thinking_blocks(), vec!["Planning execution"]);
        assert_eq!(msg.text_content(), "Here is the result");
        assert_eq!(msg.image_blocks(), vec![("image/png", "base64...")]);
        assert_eq!(
            msg.metadata.get("source").map(String::as_str),
            Some("builder")
        );
    }

    #[test]
    fn test_truncate_utf8_multibyte_stress() {
        // 4-byte UTF-8 emoji repeated to cross the boundary
        let crab = "🦀"; // 4 bytes
        let repeat_count = (MAX_TOOL_OUTPUT_BYTES / 4) + 10;
        let multibyte_str = crab.repeat(repeat_count);

        let truncated = truncate_output(&multibyte_str);
        assert!(truncated.len() <= MAX_TOOL_OUTPUT_BYTES);
        // Ensure string is valid UTF-8 and contains valid characters
        assert!(truncated.contains("🦀"));
        assert!(truncated.contains("[Truncated: exceeded 4 KB limit. Refine query]"));

        // 3-byte Euro currency symbol
        let euro = "€"; // 3 bytes
        let euro_str = euro.repeat((MAX_TOOL_OUTPUT_BYTES / 3) + 10);
        let euro_truncated = truncate_output(&euro_str);
        assert!(euro_truncated.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(euro_truncated.contains("€"));
    }

    #[test]
    fn test_truncate_tool_output_empty_lines_stress() {
        // 10,000 empty newlines
        let empty_lines = "\n".repeat(10_000);
        let result = truncate_tool_output(&empty_lines);
        assert!(result.contains("[Truncated: 9950 remaining items. Refine query]"));
        assert!(result.len() <= MAX_TOOL_OUTPUT_BYTES);
    }
}

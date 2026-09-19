//! Conversational trajectory extraction and dataset formatting for fine-tuning and evaluation.

use kai_core::error::Result;
use kai_core::message::Role;
use kai_core::traits::SessionNode;
use serde_json::json;

/// Conversational format for exported trajectories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrajectoryFormat {
    /// Line-delimited JSON with standard message turns.
    Jsonl,
    /// ShareGPT format with "from" and "value" objects.
    ShareGpt,
}

/// Exporter for compiling DAG session branches into standardized conversational datasets.
#[derive(Debug, Default, Clone)]
pub struct TrajectoryExporter;

impl TrajectoryExporter {
    /// Constructs a new [`TrajectoryExporter`].
    pub fn new() -> Self {
        Self
    }

    /// Exports a linear slice of [`SessionNode`] records into a JSONL string.
    pub fn export_jsonl(&self, nodes: &[SessionNode]) -> Result<String> {
        let mut lines = Vec::with_capacity(nodes.len());
        for node in nodes {
            let record = json!({
                "id": node.id,
                "timestamp_ms": node.timestamp_ms,
                "git_commit": node.git_commit,
                "role": format!("{:?}", node.message.role).to_ascii_lowercase(),
                "text": node.message.text_content(),
                "has_tool_calls": !node.message.tool_call_blocks().is_empty(),
                "has_tool_results": !node.message.tool_result_blocks().is_empty(),
            });
            lines.push(record.to_string());
        }
        Ok(lines.join("\n"))
    }

    /// Exports a linear slice of [`SessionNode`] records into ShareGPT-compatible JSON.
    pub fn export_sharegpt(&self, nodes: &[SessionNode]) -> Result<serde_json::Value> {
        let mut conversations = Vec::new();
        for node in nodes {
            let from = match node.message.role {
                Role::System => "system",
                Role::User => "human",
                Role::Assistant => "gpt",
                Role::Tool => "tool",
            };

            let text = node.message.text_content();
            let calls = node.message.tool_call_blocks();
            let results = node.message.tool_result_blocks();

            let mut value = text;
            if !calls.is_empty() {
                let call_strs: Vec<String> = calls
                    .iter()
                    .map(|c| {
                        format!(
                            "<tool_call>{{\"name\":\"{}\",\"arguments\":{}}}</tool_call>",
                            c.name, c.arguments
                        )
                    })
                    .collect();
                if !value.is_empty() {
                    value.push('\n');
                }
                value.push_str(&call_strs.join("\n"));
            } else if !results.is_empty() {
                let result_strs: Vec<String> = results
                    .iter()
                    .map(|r| {
                        if r.is_error {
                            format!(
                                "<tool_response id=\"{}\" status=\"error\">{}</tool_response>",
                                r.tool_call_id, r.output
                            )
                        } else {
                            format!(
                                "<tool_response id=\"{}\">{}</tool_response>",
                                r.tool_call_id, r.output
                            )
                        }
                    })
                    .collect();
                if !value.is_empty() {
                    value.push('\n');
                }
                value.push_str(&result_strs.join("\n"));
            }

            conversations.push(json!({
                "from": from,
                "value": value,
            }));
        }

        Ok(json!({
            "conversations": conversations
        }))
    }
}

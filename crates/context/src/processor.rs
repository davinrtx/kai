//! Deterministic context processor for token estimation and message compaction.
//!
//! Implements the [`kai_core::ContextProcessor`] contract using deterministic heuristics:
//! - Zero-alloc character-to-token estimation without external inference costs.
//! - Stage 1: Terminal ANSI scrubbing and exit-code head/tail truncation on tool results.
//! - Stage 2: AST skeleton extraction for large code blocks.
//! - Stage 2.5: Historical thinking block compaction.
//! - Stage 3: Turn-atomic pruning protecting conversational integrity (User-Assistant-Tool blocks)
//!   and eliminating orphan `ToolResult` errors.

use regex::Regex;
use std::sync::OnceLock;

use kai_core::{
    BoxFuture, ContentBlock, ContextError, ContextProcessor, KaiError, Message, Result, Role,
};

use crate::ast::AstSkeleton;
use crate::scrubber::TerminalScrubber;

/// Global regex for detecting markdown code fences (e.g. ```rust ... ```).
fn code_fence_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```([a-zA-Z0-9_\-+]*)\n(.*?)```").ok())
        .as_ref()
}

use std::fmt::Write;

/// Zero-alloc character counter implementing [`std::fmt::Write`].
struct CharCounter(usize);

impl Write for CharCounter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// Measures character count of a [`std::fmt::Display`] value without heap allocations.
fn count_display_chars<T: std::fmt::Display>(val: &T) -> usize {
    let mut counter = CharCounter(0);
    let _ = write!(counter, "{}", val);
    counter.0
}

/// Deterministic context optimization pipeline.
#[derive(Debug, Clone)]
pub struct DeterministicContextProcessor {
    /// Average character count per token (heuristic, default 4).
    pub chars_per_token: usize,
    /// Head lines to preserve in tool outputs during compaction.
    pub scrub_head_lines: usize,
    /// Tail lines to preserve in tool outputs during compaction.
    pub scrub_tail_lines: usize,
    /// Minimum character length of a code fence to trigger AST skeleton extraction.
    pub min_code_fence_chars: usize,
}

impl Default for DeterministicContextProcessor {
    fn default() -> Self {
        Self {
            chars_per_token: 4,
            scrub_head_lines: 5,
            scrub_tail_lines: 5,
            min_code_fence_chars: 150,
        }
    }
}

impl DeterministicContextProcessor {
    /// Constructs a [`DeterministicContextProcessor`] with default heuristics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the characters-per-token heuristic.
    pub fn with_chars_per_token(mut self, chars: usize) -> Self {
        self.chars_per_token = chars.max(1);
        self
    }

    /// Sets head and tail line boundaries for terminal output scrubbing.
    pub fn with_scrub_lines(mut self, head: usize, tail: usize) -> Self {
        self.scrub_head_lines = head;
        self.scrub_tail_lines = tail;
        self
    }

    /// Estimates token consumption for an individual message without intermediate string allocations.
    pub fn estimate_message_tokens(&self, message: &Message) -> usize {
        let chars: usize = message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::Thinking {
                    thoughts,
                    signature,
                } => thoughts.len() + signature.as_ref().map_or(0, |s| s.len()),
                ContentBlock::Image { .. } => 1024,
                ContentBlock::Document { data, .. } => data.len(),
                ContentBlock::ToolUse(call) => {
                    call.name.len() + count_display_chars(&call.arguments)
                }
                ContentBlock::ToolResult(res) => res.output.len(),
            })
            .sum();

        let tokens = chars.div_ceil(self.chars_per_token);
        // Include 4 tokens of protocol encapsulation overhead per message
        tokens + 4
    }

    /// Compresses markdown code fences within a string slice using [`AstSkeleton`].
    pub fn compress_code_blocks(&self, text: &str) -> String {
        if !text.contains("```") {
            return text.to_string();
        }
        let Some(regex) = code_fence_regex() else {
            return text.to_string();
        };
        regex
            .replace_all(text, |caps: &regex::Captures| {
                let lang = caps.get(1).map_or("", |m| m.as_str());
                let body = caps.get(2).map_or("", |m| m.as_str());

                if body.len() >= self.min_code_fence_chars {
                    let skeleton = AstSkeleton::extract(body, lang);
                    if skeleton.len() < body.len() {
                        return format!("```{lang}\n{skeleton}\n```");
                    }
                }
                caps.get(0).map_or("", |m| m.as_str()).to_string()
            })
            .to_string()
    }
}

impl ContextProcessor for DeterministicContextProcessor {
    fn estimate_tokens(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| self.estimate_message_tokens(m))
            .sum()
    }

    fn process<'a>(
        &'a self,
        messages: &'a [Message],
        target_tokens: usize,
    ) -> BoxFuture<'a, Result<Vec<Message>>> {
        Box::pin(async move {
            if messages.is_empty() {
                return Ok(Vec::new());
            }

            let initial_tokens = self.estimate_tokens(messages);
            if initial_tokens <= target_tokens {
                return Ok(messages.to_vec());
            }

            let mut working = messages.to_vec();

            // Stage 1: Scrub and bound tool outputs
            for msg in &mut working {
                for block in &mut msg.content {
                    if let ContentBlock::ToolResult(res) = block {
                        res.output = TerminalScrubber::format_output(
                            &res.output,
                            res.exit_code,
                            self.scrub_head_lines,
                            self.scrub_tail_lines,
                        );
                    }
                }
            }

            if self.estimate_tokens(&working) <= target_tokens {
                return Ok(working);
            }

            // Stage 2: Compress code blocks using AST skeletons (except the latest message)
            let len = working.len();
            for msg in working.iter_mut().take(len.saturating_sub(1)) {
                for block in &mut msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            *text = self.compress_code_blocks(text);
                        }
                        ContentBlock::ToolResult(res) => {
                            res.output = self.compress_code_blocks(&res.output);
                        }
                        _ => {}
                    }
                }
            }

            if self.estimate_tokens(&working) <= target_tokens {
                return Ok(working);
            }

            // Stage 2.5: Compact historical thinking blocks (all except the latest turn)
            for msg in working.iter_mut().take(len.saturating_sub(1)) {
                for block in &mut msg.content {
                    if let ContentBlock::Thinking { thoughts, .. } = block {
                        if thoughts.len() > 100 {
                            *thoughts =
                                "[Thinking trace omitted to preserve token budget]".to_string();
                        }
                    }
                }
            }

            if self.estimate_tokens(&working) <= target_tokens {
                return Ok(working);
            }

            // Stage 3: Turn-atomic pruning
            // Protects conversational integrity: prunes entire (User + Assistant + Tool) turns
            // to ensure no orphan ToolResults and that the sequence strictly alternates validly.
            let mut pruned_turns = 0usize;
            let estimated_notice_tokens = 24usize;

            loop {
                // Find indices of all User messages
                let user_indices: Vec<usize> = working
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, m)| {
                        if m.role == Role::User {
                            Some(idx)
                        } else {
                            None
                        }
                    })
                    .collect();

                // If there's 1 or fewer User turns, we cannot prune further without destroying the latest turn
                if user_indices.len() <= 1 {
                    break;
                }

                let current_tokens = self.estimate_tokens(&working)
                    + if pruned_turns > 0 {
                        estimated_notice_tokens
                    } else {
                        0
                    };

                if current_tokens <= target_tokens {
                    break;
                }

                // Prune the oldest User turn: from user_indices[0] to user_indices[1]
                let turn_start = user_indices[0];
                let turn_end = user_indices[1];
                let count_to_remove = turn_end - turn_start;

                for _ in 0..count_to_remove {
                    working.remove(turn_start);
                }
                pruned_turns += 1;
            }

            if pruned_turns > 0 {
                let notice_text = format!(
                    "[Context compacted: {} earlier conversational turns omitted]",
                    pruned_turns
                );

                if let Some(first_msg) = working.first_mut() {
                    if first_msg.role == Role::System {
                        // Append to existing system message to respect single-system prompt invariant
                        let mut appended = false;
                        for block in &mut first_msg.content {
                            if let ContentBlock::Text { text } = block {
                                text.push_str(&format!("\n\n{}", notice_text));
                                appended = true;
                                break;
                            }
                        }
                        if !appended {
                            first_msg.content.push(ContentBlock::text(notice_text));
                        }
                    } else {
                        // No system message at index 0, insert system notice at index 0
                        let notice = Message::system(
                            format!("compacted-notice-{}", kai_core::current_timestamp_ms()),
                            notice_text,
                        );
                        working.insert(0, notice);
                    }
                }
            }

            // Stage 3.5: If still exceeding budget, compress code blocks in the latest turn
            if self.estimate_tokens(&working) > target_tokens {
                if let Some(last_msg) = working.last_mut() {
                    for block in &mut last_msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                *text = self.compress_code_blocks(text);
                            }
                            ContentBlock::ToolResult(res) => {
                                res.output = self.compress_code_blocks(&res.output);
                            }
                            _ => {}
                        }
                    }
                }
            }

            let final_tokens = self.estimate_tokens(&working);
            if final_tokens > target_tokens {
                Err(KaiError::Context(ContextError::TokenLimitExceeded {
                    tokens: final_tokens,
                    limit: target_tokens,
                }))
            } else {
                Ok(working)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        let processor = DeterministicContextProcessor::new();
        let msg = Message::user("msg-1", "Hello world, testing token estimation.");
        let tokens = processor.estimate_message_tokens(&msg);
        assert!(tokens > 5);
    }

    #[tokio::test]
    async fn test_process_within_budget() {
        let processor = DeterministicContextProcessor::new();
        let msgs = vec![
            Message::system("sys", "System instruction"),
            Message::user("usr", "Short query"),
        ];
        let result = processor.process(&msgs, 1000).await.expect("must succeed");
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn test_process_turn_atomic_pruning_and_orphan_protection() {
        let processor = DeterministicContextProcessor::new().with_scrub_lines(2, 2);

        let mut long_terminal_output = String::new();
        for i in 1..=50 {
            long_terminal_output.push_str(&format!("\x1B[32mline {}\x1B[0m\n", i));
        }

        let msgs = vec![
            Message::system("sys", "Pinned system"),
            // Turn 1
            Message::user("u1", "Old query 1"),
            Message::assistant("a1", "Old answer 1 with tool call"),
            Message::tool_results(
                "t1",
                vec![kai_core::ToolResult::success(
                    "call-1",
                    long_terminal_output,
                )],
            ),
            // Turn 2
            Message::user("u2", "Old query 2"),
            Message::assistant("a2", "Answer 2"),
            // Turn 3 (Latest)
            Message::user("u_latest", "Latest important user query"),
        ];

        let target_tokens = 60;
        let result = processor
            .process(&msgs, target_tokens)
            .await
            .expect("must compact");

        assert!(processor.estimate_tokens(&result) <= target_tokens);
        // Pinned system preserved
        assert_eq!(result[0].role, Role::System);
        // Latest user preserved
        assert_eq!(result.last().unwrap().role, Role::User);
        assert!(result
            .last()
            .unwrap()
            .text_content()
            .contains("Latest important user query"));

        // Verify no orphan tool results exist without preceding user turn
        for (i, msg) in result.iter().enumerate() {
            if msg.role == Role::Tool {
                // Must be preceded by Assistant and User
                assert!(i >= 2);
            }
        }
    }

    #[tokio::test]
    async fn test_process_thinking_compaction() {
        let processor = DeterministicContextProcessor::new();
        let thinking_content = "Thinking step ".repeat(100); // 1400 chars

        let msgs = vec![
            Message::system("sys", "System"),
            Message::user("u1", "Query 1"),
            Message::new(
                "a1",
                Role::Assistant,
                vec![
                    ContentBlock::thinking(thinking_content),
                    ContentBlock::text("Final answer 1"),
                ],
            ),
            Message::user("u2", "Query 2"),
        ];

        let target_tokens = 60;
        let result = processor
            .process(&msgs, target_tokens)
            .await
            .expect("compact");
        assert!(processor.estimate_tokens(&result) <= target_tokens);
    }

    #[tokio::test]
    async fn test_process_token_limit_exceeded() {
        let processor = DeterministicContextProcessor::new();
        let msgs = vec![
            Message::system("sys", "System instruction"),
            Message::user("usr", "Cannot fit in 5 tokens"),
        ];

        let err = processor.process(&msgs, 5).await.unwrap_err();
        assert!(matches!(
            err,
            KaiError::Context(ContextError::TokenLimitExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn test_process_latest_turn_code_compression() {
        let processor = DeterministicContextProcessor::new();
        let code_snippet = "```rust\npub struct Service {}\nimpl Service {\n    pub fn run(&self) {\n        let mut x = 0;\n        for i in 0..100 { x += i; }\n        println!(\"{}\", x);\n    }\n}\n```";

        let msgs = vec![
            Message::system("sys", "System instruction"),
            Message::user("u1", "Please inspect code"),
            Message::assistant(
                "a1",
                format!("Here is the implementation:\n{}", code_snippet),
            ),
        ];

        let target_tokens = 60;
        let result = processor
            .process(&msgs, target_tokens)
            .await
            .expect("must compact latest turn");
        assert!(processor.estimate_tokens(&result) <= target_tokens);
        let text = result.last().unwrap().text_content();
        assert!(text.contains("pub fn run(&self) { /* omitted */ }"));
    }

    #[tokio::test]
    async fn test_single_system_message_invariant() {
        let processor = DeterministicContextProcessor::new();
        let msgs = vec![
            Message::system("sys-init", "Initial base system instructions."),
            Message::user("u1", "Turn 1 question"),
            Message::assistant("a1", "Turn 1 answer"),
            Message::user("u2", "Turn 2 question"),
            Message::assistant("a2", "Turn 2 answer"),
            Message::user("u3", "Turn 3 latest query"),
        ];

        let target_tokens = 40;
        let result = processor
            .process(&msgs, target_tokens)
            .await
            .expect("must compact");

        // Count how many system messages exist
        let system_count = result.iter().filter(|m| m.role == Role::System).count();
        assert_eq!(system_count, 1, "There must be exactly one system message");
        assert_eq!(result[0].role, Role::System);
        assert!(result[0]
            .text_content()
            .contains("Initial base system instructions."));
        assert!(result[0].text_content().contains("[Context compacted:"));
    }
}

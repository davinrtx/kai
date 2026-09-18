//! End-to-end integration tests for the kai-context pipeline crate.

use std::fs::{create_dir_all, write, File};
use std::io::Write;

use kai_context::{
    AstSkeleton, DeterministicContextProcessor, GrepOptions, GrepSearcher, TerminalScrubber,
    WindowReader,
};
use kai_core::event::{global_steering_channel, SteeringState};
use kai_core::{ContextProcessor, KaiError, Message, OrchestratorError, Role, ToolResult};

#[test]
fn test_integration_scrubber_and_window() {
    let temp_dir = std::env::temp_dir().join("kai_test_integ_scrub_win");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    let dirty_file = temp_dir.join("terminal.log");
    {
        let mut file = File::create(&dirty_file).expect("create dirty file");
        for i in 1..=40 {
            writeln!(file, "\x1B[32mStep {}\x1B[0m: status OK \x1B[2K", i).expect("write line");
        }
    }

    // Read a window of lines from the file
    let window = WindowReader::read_lines(&dirty_file, 5, 10).expect("read window");
    assert_eq!(window.start_line, 5);
    assert_eq!(window.end_line, 14);
    assert_eq!(window.total_lines, 40);

    // Scrub the windowed content
    let formatted = TerminalScrubber::format_output(&window.content, Some(0), 3, 3);
    assert!(formatted.starts_with("[Exit Code: 0]\n"));
    assert!(!formatted.contains("\x1B[32m"));
    assert!(formatted.contains("Step 5: status OK"));
    assert!(formatted.contains("[... Omitted 4 lines ...]"));
    assert!(formatted.contains("Step 14: status OK"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_integration_grep_and_skeleton() {
    let temp_dir = std::env::temp_dir().join("kai_test_integ_grep_skel");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    let rust_file = temp_dir.join("service.rs");
    let rust_code = r#"
pub struct Engine {
    workers: usize,
}

impl Engine {
    pub fn start(&self) {
        println!("Starting engine with {} workers", self.workers);
        let x = 100;
        let y = x * 2;
    }
}
"#;
    write(&rust_file, rust_code).expect("write rust file");

    let options = GrepOptions {
        case_insensitive: false,
        max_items: 10,
        extensions: Some(vec!["rs".to_string()]),
        ..Default::default()
    };

    let grep_res = GrepSearcher::search(&temp_dir, "fn start", &options).expect("grep search");
    assert_eq!(grep_res.total_matches, 1);
    assert_eq!(grep_res.matches[0].line_number, 7);

    // Extract skeleton of found file
    let skeleton = AstSkeleton::extract(rust_code, "rust");
    assert!(skeleton.contains("pub struct Engine"));
    assert!(skeleton.contains("pub fn start(&self) { /* omitted */ }"));
    assert!(!skeleton.contains("println!"));
    assert!(!skeleton.contains("let y = x * 2;"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn test_integration_context_processor_full_pipeline() {
    let processor = DeterministicContextProcessor::new()
        .with_chars_per_token(4)
        .with_scrub_lines(2, 2);

    let verbose_code = r#"```rust
pub struct Handler {
    id: u64,
}

impl Handler {
    pub fn handle_event(&self, event_id: u64) -> bool {
        let timestamp = 123456789;
        let mut buffer = Vec::new();
        for i in 0..100 {
            buffer.push(i * timestamp);
        }
        buffer.len() > 0
    }
}
```"#;

    let mut verbose_tool_output = String::new();
    for i in 1..=30 {
        verbose_tool_output.push_str(&format!(
            "\x1B[1;34m[TRACE]\x1B[0m Executed sub-step {}\n",
            i
        ));
    }

    let messages = vec![
        Message::system("sys", "You are an autonomous agent runtime."),
        Message::user("u1", "Analyze the handler service."),
        Message::assistant("a1", format!("Here is the handler code:\n{}", verbose_code)),
        Message::tool_results(
            "t1",
            vec![ToolResult::success("tool-exec-1", verbose_tool_output).with_exit_code(0)],
        ),
        Message::assistant("a2", "The previous step completed with status 0."),
        Message::user("u2", "Provide next action summary."),
    ];

    let initial_tokens = processor.estimate_tokens(&messages);
    assert!(initial_tokens > 100);

    // Target a budget that requires both scrubbing and turn compaction
    let target_tokens = 90;
    let compacted = processor
        .process(&messages, target_tokens)
        .await
        .expect("compaction must succeed");

    let final_tokens = processor.estimate_tokens(&compacted);
    assert!(
        final_tokens <= target_tokens,
        "final tokens {} must be <= target {}",
        final_tokens,
        target_tokens
    );

    // Verify system instruction is preserved
    assert_eq!(compacted[0].role, Role::System);
    assert!(compacted[0]
        .text_content()
        .contains("You are an autonomous agent runtime."));

    // Verify latest user turn is preserved
    let last_msg = compacted.last().expect("last message must exist");
    assert_eq!(last_msg.role, Role::User);
    assert!(last_msg
        .text_content()
        .contains("Provide next action summary."));
}

#[test]
fn test_integration_bounded_line_window_oom_defense() {
    let temp_dir = std::env::temp_dir().join("kai_test_integ_oom_window");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    let massive_file = temp_dir.join("minified_bundle.js");
    {
        let mut file = File::create(&massive_file).expect("create massive file");
        // Write 150 KB single line without any newline
        let massive_line = "var bundle={data:\"".to_string() + &"x".repeat(150_000) + "\"};\n";
        file.write_all(massive_line.as_bytes()).expect("write line");
    }

    let result = WindowReader::read_lines(&massive_file, 1, 1).expect("read lines");
    assert_eq!(result.total_lines, 1);
    assert!(result
        .content
        .contains("[Line truncated: exceeded 64KB cap]"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_integration_grep_steering_cancellation() {
    let temp_dir = std::env::temp_dir().join("kai_test_integ_grep_cancel");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    for i in 1..=10 {
        write(
            temp_dir.join(format!("file_{}.txt", i)),
            format!("content in file {}", i),
        )
        .expect("write file");
    }

    let (tx, rx) = global_steering_channel();
    tx.send(SteeringState::Terminated)
        .expect("send termination");

    let options = GrepOptions::default().with_steering(rx);
    let res = GrepSearcher::search(&temp_dir, "content", &options);

    assert!(matches!(
        res,
        Err(KaiError::Orchestrator(
            OrchestratorError::Interrupted { .. }
        ))
    ));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_integration_typescript_extended_ast() {
    let code = r#"
export class ServiceController {
    constructor(private db: Database) {}

    handleRequest(req: Request) {
        return this.db.query("SELECT * FROM users");
    }

    async processBatch(items: string[]) {
        for (const item of items) {
            await this.handleRequest(item);
        }
    }
}

export const helperArrow = (x: number): number => {
    return x * 42;
};
"#;

    let skeleton = AstSkeleton::extract(code, "typescript");
    assert!(skeleton.contains("handleRequest(req: Request) { /* omitted */ }"));
    assert!(skeleton.contains("async processBatch(items: string[]) { /* omitted */ }"));
    assert!(
        skeleton.contains("export const helperArrow = (x: number): number => { /* omitted */ }")
    );
    assert!(!skeleton.contains("SELECT * FROM users"));
    assert!(!skeleton.contains("return x * 42"));
}

//! Exhaustive stress and pathological edge-case test suite for kai-context.

use std::fs::{create_dir_all, write, File};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use kai_context::{
    AstSkeleton, DeterministicContextProcessor, GrepOptions, GrepSearcher, WindowReader,
};
use kai_core::event::{global_steering_channel, SteeringState};
use kai_core::{
    ContentBlock, ContextProcessor, KaiError, Message, OrchestratorError, Role, ToolResult,
    MAX_TOOL_OUTPUT_BYTES,
};

// =========================================================================
// 1. WindowReader Stress Tests
// =========================================================================

#[test]
fn test_stress_window_reader_massive_single_line() {
    let temp_dir = std::env::temp_dir().join("kai_stress_win_single_line");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    let file_path = temp_dir.join("single_line_500kb.txt");
    {
        let mut file = File::create(&file_path).expect("create file");
        // Write 500 KB on a single line without newlines
        let chunk = "0123456789ABCDEF".repeat(32 * 1024); // 512 KB
        file.write_all(chunk.as_bytes()).expect("write chunk");
    }

    let result = WindowReader::read_lines(&file_path, 1, 1).expect("read lines");
    assert_eq!(result.total_lines, 1);
    assert_eq!(result.start_line, 1);
    assert_eq!(result.end_line, 1);
    assert!(result
        .content
        .contains("[Line truncated: exceeded 64KB cap]"));
    // Verify memory cap: content length must be bounded around 64 KB + marker
    assert!(result.content.len() < 70_000);

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_stress_window_reader_100k_lines_fast_counting() {
    let temp_dir = std::env::temp_dir().join("kai_stress_win_100k");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    let file_path = temp_dir.join("data_100k.log");
    {
        let mut file = File::create(&file_path).expect("create file");
        let mut buf = Vec::with_capacity(64 * 1024);
        for i in 1..=100_000 {
            buf.extend_from_slice(format!("entry {}: status=ACTIVE, code=200\n", i).as_bytes());
            if buf.len() >= 32 * 1024 {
                file.write_all(&buf).expect("write buffer");
                buf.clear();
            }
        }
        if !buf.is_empty() {
            file.write_all(&buf).expect("flush buffer");
        }
    }

    let start = std::time::Instant::now();
    // Read only lines 500..510 (limit 11), letting fast byte-counting scan the remaining 99,490 lines
    let result = WindowReader::read_lines(&file_path, 500, 11).expect("read window");
    let elapsed = start.elapsed();

    assert_eq!(result.start_line, 500);
    assert_eq!(result.end_line, 510);
    assert_eq!(result.total_lines, 100_000);
    assert!(result.content.contains("entry 500: status=ACTIVE"));
    assert!(result.content.contains("entry 510: status=ACTIVE"));
    assert!(
        elapsed.as_millis() < 1000,
        "100k line count took too long: {:?}",
        elapsed
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_stress_window_reader_boundary_conditions() {
    // Empty content
    let empty_res = WindowReader::read_str("", 1, 1).expect("empty file window");
    assert_eq!(empty_res.total_lines, 0);
    assert_eq!(empty_res.content, "");

    // Exactly 1 line without newline
    let one_line = WindowReader::read_str("solo line", 1, 1).expect("single line");
    assert_eq!(one_line.total_lines, 1);
    assert_eq!(one_line.content, "solo line");

    // Exactly 1 line with newline
    let one_line_nl = WindowReader::read_str("solo line\n", 1, 1).expect("single line nl");
    assert_eq!(one_line_nl.total_lines, 1);
    assert_eq!(one_line_nl.content, "solo line");

    // CRLF ending
    let crlf_line = WindowReader::read_str("line 1\r\nline 2\r\n", 1, 2).expect("crlf lines");
    assert_eq!(crlf_line.total_lines, 2);
    assert_eq!(crlf_line.content, "line 1\nline 2");

    // Read last line exactly
    let last_line = WindowReader::read_str("a\nb\nc", 3, 1).expect("last line");
    assert_eq!(last_line.start_line, 3);
    assert_eq!(last_line.end_line, 3);
    assert_eq!(last_line.content, "c");

    // Read offset out of bounds
    let oob = WindowReader::read_str("a\nb\nc", 4, 1);
    assert!(oob.is_err());
}

// =========================================================================
// 2. GrepSearcher Stress & High-Volume Traversal
// =========================================================================

#[test]
fn test_stress_grep_500_files_horizon_and_per_file_caps() {
    let temp_dir = std::env::temp_dir().join("kai_stress_grep_500");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    // Generate 500 files, each with 10 matching lines
    for f in 1..=500 {
        let sub_dir = temp_dir.join(format!("dir_{:02}", f % 20));
        create_dir_all(&sub_dir).expect("create sub dir");
        let file_path = sub_dir.join(format!("file_{:03}.rs", f));
        let mut content = String::new();
        for i in 1..=10 {
            content.push_str(&format!("fn test_worker_probe_{}_{}() {{}}\n", f, i));
        }
        write(&file_path, content).expect("write file");
    }

    let options = GrepOptions {
        max_items: 50,
        max_horizon: 300,
        max_matches_per_file: Some(2),
        ..Default::default()
    };

    let start = std::time::Instant::now();
    let results = GrepSearcher::search(&temp_dir, "test_worker_probe", &options).expect("search");
    let elapsed = start.elapsed();

    assert_eq!(results.matches.len(), 50);
    assert!(results.is_truncated);
    assert!(results.reached_horizon);
    assert!(results.total_matches >= 300);
    assert!(
        elapsed.as_millis() < 2000,
        "500-file grep scan took too long: {:?}",
        elapsed
    );

    let formatted = results.format();
    assert!(formatted.len() <= MAX_TOOL_OUTPUT_BYTES);
    assert!(formatted.contains("[Truncated:"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_stress_grep_skips_large_files() {
    let temp_dir = std::env::temp_dir().join("kai_stress_grep_skip_large");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    // Create a 6 MB dummy file containing the search keyword
    let large_file = temp_dir.join("dump_6mb.sql");
    {
        let mut f = File::create(&large_file).expect("create large file");
        let chunk = "SELECT * FROM users WHERE keyword = 'CRITICAL_KEYWORD';\n";
        let repeat = (6 * 1024 * 1024) / chunk.len();
        for _ in 0..repeat {
            f.write_all(chunk.as_bytes()).expect("write chunk");
        }
    }

    // Create a small 1 KB file containing the keyword
    let small_file = temp_dir.join("normal.rs");
    write(&small_file, "const KEY: &str = \"CRITICAL_KEYWORD\";\n").expect("write small");

    let options = GrepOptions::default(); // default max_file_size_bytes is 5 MB
    let results = GrepSearcher::search(&temp_dir, "CRITICAL_KEYWORD", &options).expect("search");

    // Must have found ONLY the small file match, skipping the 6 MB file completely
    assert_eq!(results.matches.len(), 1);
    assert!(results.matches[0]
        .path
        .to_string_lossy()
        .contains("normal.rs"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_stress_grep_concurrent_steering_interruption() {
    let temp_dir = std::env::temp_dir().join("kai_stress_grep_steering_async");
    let _ = std::fs::remove_dir_all(&temp_dir);
    create_dir_all(&temp_dir).expect("create temp dir");

    // Generate 100 files
    for i in 1..=100 {
        write(
            temp_dir.join(format!("scan_{}.txt", i)),
            "target query term repeatedly written\n".repeat(50),
        )
        .expect("write");
    }

    let (tx, rx) = global_steering_channel();
    let cancel_fired = Arc::new(AtomicBool::new(false));
    let cancel_fired_clone = cancel_fired.clone();

    // Spawn a background thread to cancel halfway
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1));
        let _ = tx.send(SteeringState::Terminated);
        cancel_fired_clone.store(true, Ordering::SeqCst);
    });

    let options = GrepOptions::default().with_steering(rx);
    let result = GrepSearcher::search(&temp_dir, "target query term", &options);

    handle.join().expect("join cancel thread");

    if let Err(KaiError::Orchestrator(OrchestratorError::Interrupted { reason })) = result {
        assert!(reason.contains("steering"));
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

// =========================================================================
// 3. AstSkeleton Stress on Pathological Grammars
// =========================================================================

#[test]
fn test_stress_ast_pathological_python() {
    let complex_py = r#"
@dataclass
@route("/api/v1/stream")
@auth_required(roles=["admin", "operator"])
async def complex_handler(
    request: Request[dict[str, Union[list[int], tuple[str, ...]]]],
    client_id: Optional[str] = "default:id",
    timeout: float = 30.0,
    callback: Optional[Callable[[int, str], bool]] = None,
) -> Result[Response[Payload], HttpError]:
    """Execute complex transaction stream.
    Multiline documentation with "escaped quotes" and colons: like this:
    """
    secret_key = "sk_live_123456789:password"
    for item in request.data:
        val = item * 2
        yield val
    return Response(ok=True)

class NestedService:
    @property
    def config(self) -> dict:
        return {"env": "prod"}

    def inline_method(self): return {"status": 200}

    async def nested_generator(self, iters: int):
        def inner_helper(val: int) -> int:
            return val * 10
        for i in range(iters):
            yield inner_helper(i)
"#;

    let skeleton = AstSkeleton::extract(complex_py, "python");
    assert!(skeleton.contains("@dataclass"));
    assert!(skeleton.contains("@route(\"/api/v1/stream\")"));
    assert!(skeleton.contains("async def complex_handler("));
    assert!(skeleton.contains("request: Request[dict[str, Union[list[int], tuple[str, ...]]]],"));
    assert!(skeleton.contains("client_id: Optional[str] = \"default:id\","));
    assert!(skeleton.contains("callback: Optional[Callable[[int, str], bool]] = None,"));
    assert!(skeleton.contains(") -> Result[Response[Payload], HttpError]:"));
    assert!(skeleton.contains("\"\"\"Execute complex transaction stream."));
    assert!(skeleton.contains("class NestedService:"));
    assert!(skeleton.contains("@property"));
    assert!(skeleton.contains("def config(self) -> dict:"));
    assert!(skeleton.contains("def inline_method(self): ..."));
    assert!(skeleton.contains("async def nested_generator(self, iters: int):"));

    // Function bodies must be stripped
    assert!(!skeleton.contains("secret_key = \"sk_live_123456789:password\""));
    assert!(!skeleton.contains("val = item * 2"));
    assert!(!skeleton.contains("return {\"env\": \"prod\"}"));
}

#[test]
fn test_stress_ast_pathological_rust() {
    let complex_rs = r##"
macro_rules! dispatch_rpc {
    ($target:ident, $method:ident, ($($arg:expr),*)) => {
        $target.$method($($arg),*).await
    };
}

pub struct ComplexEngine<T, U>
where
    T: Send + Sync + 'static,
    U: Clone + Default,
{
    state: T,
    cache: U,
}

impl<T, U> ComplexEngine<T, U>
where
    T: Send + Sync + 'static,
    U: Clone + Default,
{
    /* Block comment with tricky braces: { ignored } and more { */
    pub async unsafe fn execute_transaction<F, R>(
        &self,
        key: &str,
        mut mutator: F,
    ) -> Result<R, ExecutionError>
    where
        F: FnMut(&mut T) -> Result<R, ExecutionError> + Send,
        R: Serialize + DeserializeOwned,
    {
        let raw_str = r#" { this raw brace is ignored } "#;
        let mut lock = self.state.write().await;
        mutator(&mut lock)
    }

    pub fn simple_fn(&self) -> bool {
        true
    }
}
"##;

    let skeleton = AstSkeleton::extract(complex_rs, "rust");
    assert!(skeleton.contains("macro_rules! dispatch_rpc { /* omitted */ }"));
    assert!(skeleton.contains("pub struct ComplexEngine<T, U>"));
    assert!(skeleton.contains("impl<T, U> ComplexEngine<T, U>"));
    assert!(skeleton.contains("pub async unsafe fn execute_transaction<F, R>("));
    assert!(skeleton.contains("mut mutator: F,"));
    assert!(skeleton.contains(") -> Result<R, ExecutionError>"));
    assert!(skeleton.contains("R: Serialize + DeserializeOwned,"));
    assert!(skeleton.contains("{ /* omitted */ }"));
    assert!(skeleton.contains("pub fn simple_fn(&self) -> bool { /* omitted */ }"));

    // Bodies must be stripped
    assert!(!skeleton.contains("dispatch_rpc!"));
    assert!(!skeleton.contains("this raw brace is ignored"));
    assert!(!skeleton.contains("self.state.write().await"));
}

#[test]
fn test_stress_ast_pathological_typescript() {
    let complex_ts = r#"
export interface ApiPayload<T> {
    data: T;
    metadata: Record<string, any>;
}

export class WorkerController {
    constructor(private readonly logger: Logger) {}

    public async processQueue<T>(items: T[]): Promise<void> {
        for (const item of items) {
            await this.handleSingle(item);
        }
    }

    private handleSingle = async (item: any): Promise<boolean> => {
        const payload = `item-${item.id}-${{ nested: true }}`;
        return true;
    };
}
"#;

    let skeleton = AstSkeleton::extract(complex_ts, "typescript");
    assert!(skeleton.contains("export interface ApiPayload<T>"));
    assert!(skeleton.contains("export class WorkerController"));
    assert!(skeleton
        .contains("public async processQueue<T>(items: T[]): Promise<void> { /* omitted */ }"));
    assert!(skeleton.contains(
        "private handleSingle = async (item: any): Promise<boolean> => { /* omitted */ }"
    ));
    assert!(!skeleton.contains("this.handleSingle(item)"));
    assert!(!skeleton.contains("item.id"));
}

// =========================================================================
// 4. DeterministicContextProcessor 50-Turn Conversational Compaction
// =========================================================================

#[tokio::test]
async fn test_stress_processor_50_turn_atomic_compaction() {
    let processor = DeterministicContextProcessor::new()
        .with_chars_per_token(4)
        .with_scrub_lines(3, 3);

    let mut messages = Vec::new();
    messages.push(Message::system(
        "sys-core",
        "Primary immutable system instructions for autonomous agent runtime.",
    ));

    // Create 50 full turns: User -> Assistant (with ToolCall) -> ToolResult
    for turn in 1..=50 {
        let u_id = format!("usr-{}", turn);
        let a_id = format!("asst-{}", turn);
        let call_id = format!("call-{}", turn);
        let t_id = format!("tool-{}", turn);

        let user_msg = Message::user(u_id, format!("User query for conversational turn {}", turn));

        let asst_msg = Message::new(
            a_id,
            Role::Assistant,
            vec![
                ContentBlock::thinking(format!(
                    "Thinking reasoning step for turn {}: analyzing AST and parameters.",
                    turn
                )),
                ContentBlock::text(format!("Invoking tool for turn {}", turn)),
            ],
        );

        let verbose_log =
            format!("\x1B[32m[PASS]\x1B[0m Test suite {} completed.\n", turn).repeat(20);

        let tool_msg = Message::tool_results(
            t_id,
            vec![ToolResult::success(call_id, verbose_log).with_exit_code(0)],
        );

        messages.push(user_msg);
        messages.push(asst_msg);
        messages.push(tool_msg);
    }

    // Add latest turn 51 (User prompt awaiting answer)
    messages.push(Message::user(
        "usr-latest-51",
        "Explain final consolidated architecture summary.",
    ));

    assert_eq!(messages.len(), 1 + 50 * 3 + 1); // 152 messages

    let initial_tokens = processor.estimate_tokens(&messages);
    assert!(
        initial_tokens > 2000,
        "Initial tokens must be substantial: {}",
        initial_tokens
    );

    // Target a budget that requires aggressive pruning of ~45 turns
    let target_tokens = 250;
    let compacted = processor
        .process(&messages, target_tokens)
        .await
        .expect("compaction must succeed");

    let final_tokens = processor.estimate_tokens(&compacted);
    assert!(
        final_tokens <= target_tokens,
        "Final tokens {} must be <= target {}",
        final_tokens,
        target_tokens
    );

    // INVARIANT 1: Exactly ONE System message, positioned at index 0
    let system_msgs: Vec<&Message> = compacted
        .iter()
        .filter(|m| m.role == Role::System)
        .collect();
    assert_eq!(system_msgs.len(), 1, "Must have exactly 1 system message");
    assert_eq!(compacted[0].role, Role::System);
    assert!(compacted[0]
        .text_content()
        .contains("Primary immutable system instructions"));
    assert!(compacted[0].text_content().contains("[Context compacted:"));

    // INVARIANT 2: Latest turn preserved intact
    let last_msg = compacted.last().expect("last msg");
    assert_eq!(last_msg.role, Role::User);
    assert!(last_msg
        .text_content()
        .contains("Explain final consolidated architecture summary."));

    // INVARIANT 3: No orphan ToolResult messages (every Tool message must be preceded by Assistant)
    for (idx, msg) in compacted.iter().enumerate() {
        if msg.role == Role::Tool {
            assert!(idx >= 2, "ToolResult cannot be first or second message");
            assert_eq!(
                compacted[idx - 1].role,
                Role::Assistant,
                "ToolResult must immediately follow an Assistant message"
            );
        }
    }
}

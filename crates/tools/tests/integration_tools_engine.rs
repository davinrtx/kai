//! Exhaustive integration test suite for `kai-tools` (System Tool Engine).
//!
//! Verifies transactional atomicity, bounded reads (limit <= 150), ProcessGuard RAII
//! subprocess termination, steering cancellation, and offline browser/MCP drivers.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use kai_core::event::{global_steering_channel, SteeringState};
use kai_core::traits::{Tool, ToolContext};
use kai_tools::{
    default_tools, ApplyPatchTool, BrowserActionTool, ExecCommandTool, McpClient, McpClientTool,
    McpToolDefinition, MockBrowserDriver, MockMcpTransport, ReadWindowTool, MAX_WINDOW_LIMIT,
};
use serde_json::json;

#[tokio::test]
async fn test_read_window_bounded_and_formatted() {
    let temp_dir =
        std::env::temp_dir().join(format!("kai_test_read_window_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let file_path = temp_dir.join("sample.txt");
    let content = (1..=200)
        .map(|i| format!("Line content {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file_path, content).unwrap();

    let tool = ReadWindowTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_rw_1", "agent_rw");

    // 1. Read window 10..20 (10 lines)
    let args = json!({
        "path": "sample.txt",
        "offset": 10,
        "limit": 10
    });
    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(!res.is_error);
    assert!(res.output.contains("10 | Line content 10"));
    assert!(res.output.contains("19 | Line content 19"));
    assert!(!res.output.contains("20 | Line content 20"));
    assert!(res.output.contains("More lines available in file"));

    // 2. Validate max limit enforcement (limit > 150 must fail validation)
    let bad_args = json!({
        "path": "sample.txt",
        "offset": 1,
        "limit": 151
    });
    assert!(tool.validate_arguments(&bad_args).is_err());

    let exec_bad_res = tool.execute(bad_args, &ctx).await.unwrap();
    assert!(exec_bad_res.is_error);
    assert!(exec_bad_res.output.contains("must be between 1 and"));

    // 3. Exact max limit (150 lines) succeeds
    let max_args = json!({
        "path": "sample.txt",
        "offset": 1,
        "limit": MAX_WINDOW_LIMIT
    });
    assert!(tool.validate_arguments(&max_args).is_ok());
    let max_res = tool.execute(max_args, &ctx).await.unwrap();
    assert!(!max_res.is_error);
    assert!(max_res.output.contains("1 | Line content 1"));

    // 4. Offset beyond EOF
    let eof_args = json!({
        "path": "sample.txt",
        "offset": 500,
        "limit": 20
    });
    let eof_res = tool.execute(eof_args, &ctx).await.unwrap();
    assert!(!eof_res.is_error);
    assert!(eof_res.output.contains("beyond end of file"));

    // Clean up
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_apply_patch_transactional_atomic_replacement() {
    let temp_dir =
        std::env::temp_dir().join(format!("kai_test_patch_atomic_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let target_file = temp_dir.join("code.rs");
    let original_code = "fn main() {\n    println!(\"Hello World\");\n}\n";
    fs::write(&target_file, original_code).unwrap();

    let tool = ApplyPatchTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_patch_1", "agent_patch");

    let patch = r#"@@ -1,3 +1,3 @@
 fn main() {
-    println!("Hello World");
+    println!("Hello KAI");
 }
"#;

    let args = json!({
        "path": "code.rs",
        "patch": patch
    });

    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(!res.is_error);
    assert!(res.output.contains("Successfully applied 1 hunk(s)"));

    let modified_code = fs::read_to_string(&target_file).unwrap();
    assert_eq!(
        modified_code,
        "fn main() {\n    println!(\"Hello KAI\");\n}\n"
    );

    // Verify zero leftover .tmp files
    let dir_entries: Vec<_> = fs::read_dir(&temp_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(dir_entries.len(), 1);
    assert_eq!(dir_entries[0].file_name(), "code.rs");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_apply_patch_failure_rollback() {
    let temp_dir =
        std::env::temp_dir().join(format!("kai_test_patch_rollback_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let target_file = temp_dir.join("unchanged.txt");
    let original = "line one\nline two\nline three\n";
    fs::write(&target_file, original).unwrap();

    let tool = ApplyPatchTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_patch_2", "agent_patch");

    // Patch with mismatching context (expects "wrong line" instead of "line two")
    let bad_patch = r#"@@ -1,3 +1,3 @@
 line one
-wrong line
+new line
 line three
"#;

    let args = json!({
        "path": "unchanged.txt",
        "patch": bad_patch
    });

    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(res.is_error);
    assert!(res.output.contains("Patch application failed"));

    // Target file MUST remain identical to original (transactional integrity)
    let current_content = fs::read_to_string(&target_file).unwrap();
    assert_eq!(current_content, original);

    // Verify zero leftover .tmp files
    let dir_entries: Vec<_> = fs::read_dir(&temp_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(dir_entries.len(), 1);
    assert_eq!(dir_entries[0].file_name(), "unchanged.txt");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_exec_command_execution_and_truncation() {
    let temp_dir = std::env::temp_dir().join(format!("kai_test_exec_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let tool = ExecCommandTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_exec_1", "agent_exec");

    // 1. Successful execution
    let cmd = if cfg!(windows) {
        "echo Hello Shell"
    } else {
        "echo 'Hello Shell'"
    };

    let args = json!({ "command": cmd });
    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(!res.is_error);
    assert_eq!(res.exit_code, Some(0));
    assert!(res.output.contains("Hello Shell"));

    // 2. Failing command (exit code != 0)
    let fail_cmd = if cfg!(windows) {
        "dir non_existent_file_xyz_12345"
    } else {
        "ls non_existent_file_xyz_12345"
    };

    let fail_args = json!({ "command": fail_cmd });
    let fail_res = tool.execute(fail_args, &ctx).await.unwrap();
    assert!(fail_res.is_error);
    assert!(fail_res.exit_code.is_some_and(|c| c != 0));

    // 3. Subprocess output truncation check (generate 200 lines)
    let large_cmd = if cfg!(windows) {
        "for /L %i in (1,1,200) do @echo item %i"
    } else {
        "seq 1 200"
    };

    let large_args = json!({ "command": large_cmd });
    let large_res = tool.execute(large_args, &ctx).await.unwrap();
    assert!(large_res.output.contains("[Truncated:"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_exec_command_steering_cancellation() {
    let temp_dir =
        std::env::temp_dir().join(format!("kai_test_exec_cancel_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let (tx, rx) = global_steering_channel();
    let ctx = ToolContext::new(&temp_dir, "sess_exec_cancel", "agent_exec").with_steering(rx);

    let tool = ExecCommandTool::new();

    // Command that sleeps for a long time
    let sleep_cmd = if cfg!(windows) {
        "ping -n 10 127.0.0.1 > nul"
    } else {
        "sleep 10"
    };

    let args = json!({
        "command": sleep_cmd,
        "timeout_ms": 10000
    });

    // Send cancellation shortly after start
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = tx.send(SteeringState::Terminated);
    });

    let res = tool.execute(args, &ctx).await;
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("cancelled by steering signal") || err_msg.contains("Interrupted"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_browser_action_mock_driver() {
    let driver = Arc::new(MockBrowserDriver::new());
    driver
        .set_page(
            "https://example.local/home",
            "Example Dashboard",
            "<h1>Welcome</h1><button id='action-btn'>Click Me</button>",
        )
        .await;
    driver
        .set_eval_result("document.title", json!("Example Dashboard"))
        .await;

    let tool = BrowserActionTool::with_driver(driver.clone());
    let ctx = ToolContext::new("/workspace", "sess_browser_1", "agent_browser");

    // 1. Navigate
    let nav_res = tool
        .execute(
            json!({
                "action": "navigate",
                "url": "https://example.local/home"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!nav_res.is_error);
    assert!(nav_res.output.contains("Example Dashboard"));

    // 2. Click
    let click_res = tool
        .execute(
            json!({
                "action": "click",
                "selector": "#action-btn"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!click_res.is_error);
    assert!(click_res.output.contains("Clicked '#action-btn'"));
    assert_eq!(driver.clicked_selectors().await, vec!["#action-btn"]);

    // 3. Screenshot
    let shot_res = tool
        .execute(json!({ "action": "screenshot" }), &ctx)
        .await
        .unwrap();
    assert!(!shot_res.is_error);
    assert!(shot_res.output.contains("Screenshot captured"));

    // 4. Evaluate
    let eval_res = tool
        .execute(
            json!({
                "action": "evaluate",
                "script": "document.title"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!eval_res.is_error);
    assert!(eval_res.output.contains("Example Dashboard"));

    // 5. Page content
    let content_res = tool
        .execute(json!({ "action": "page_content" }), &ctx)
        .await
        .unwrap();
    assert!(!content_res.is_error);
    assert!(content_res.output.contains("Welcome"));
}

#[tokio::test]
async fn test_mcp_client_tool_discovery_and_call() {
    let transport = Arc::new(MockMcpTransport::new());

    // Register a mock tool in the transport
    let tool_def = McpToolDefinition {
        name: "math_add".to_string(),
        description: "Adds two numbers".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            }
        }),
    };

    transport
        .register_tool(tool_def, |args| {
            Box::pin(async move {
                let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                Ok(format!("{}", a + b))
            })
        })
        .await;

    let client = McpClient::new(transport);
    let tool = McpClientTool::with_client(client);
    let ctx = ToolContext::new("/workspace", "sess_mcp_1", "agent_mcp");

    // 1. List tools
    let list_res = tool
        .execute(json!({ "action": "list" }), &ctx)
        .await
        .unwrap();
    assert!(!list_res.is_error);
    assert!(list_res.output.contains("math_add"));
    assert!(list_res.output.contains("Adds two numbers"));

    // 2. Call tool successfully
    let call_res = tool
        .execute(
            json!({
                "action": "call",
                "tool_name": "math_add",
                "arguments": { "a": 15, "b": 27 }
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!call_res.is_error);
    assert_eq!(call_res.output.trim(), "42");

    // 3. Call non-existent tool
    let not_found_res = tool
        .execute(
            json!({
                "action": "call",
                "tool_name": "unknown_tool",
                "arguments": {}
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(not_found_res.is_error);
    assert!(not_found_res.output.contains("Tool not found"));
}

#[test]
fn test_default_tools_enumeration() {
    let registry = default_tools();
    assert_eq!(registry.len(), 5);
}

#[tokio::test]
async fn test_read_window_limit_zero_and_long_line() {
    let temp_dir = std::env::temp_dir().join(format!("kai_test_rw_edge_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let tool = ReadWindowTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_rw_edge", "agent_rw");

    // 1. limit == 0 rejected by validation
    let zero_args = json!({
        "path": "dummy.txt",
        "offset": 1,
        "limit": 0
    });
    assert!(tool.validate_arguments(&zero_args).is_err());
    let zero_res = tool.execute(zero_args, &ctx).await.unwrap();
    assert!(zero_res.is_error);

    // 2. Giant line (> 64KB) gets truncated with notice
    let file_path = temp_dir.join("giant_line.txt");
    let massive_line = "A".repeat(70_000);
    fs::write(&file_path, format!("{massive_line}\nshort second line\n")).unwrap();

    let read_args = json!({
        "path": "giant_line.txt",
        "offset": 1,
        "limit": 2
    });
    let read_res = tool.execute(read_args, &ctx).await.unwrap();
    assert!(!read_res.is_error);
    assert!(read_res
        .output
        .contains("[Line truncated: exceeded 2KB cap]"));
    assert!(read_res.output.contains("short second line"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_apply_patch_crlf_and_blank_lines() {
    let temp_dir = std::env::temp_dir().join(format!("kai_test_patch_crlf_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let target_file = temp_dir.join("crlf_sample.rs");
    // CRLF line endings
    let original = "fn test() {\r\n\r\n    let x = 1;\r\n}\r\n";
    fs::write(&target_file, original).unwrap();

    let tool = ApplyPatchTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_patch_crlf", "agent_patch");

    // Diff with blank line without leading space (LLM artifact)
    let patch = "@@ -1,4 +1,4 @@\n fn test() {\n\n-    let x = 1;\n+    let x = 42;\n }\n";

    let args = json!({
        "path": "crlf_sample.rs",
        "patch": patch
    });

    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(!res.is_error);

    let modified = fs::read_to_string(&target_file).unwrap();
    // Must preserve CRLF
    assert!(modified.contains("\r\n"));
    assert_eq!(modified, "fn test() {\r\n\r\n    let x = 42;\r\n}\r\n");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_apply_patch_overlapping_hunks_rejected() {
    let temp_dir =
        std::env::temp_dir().join(format!("kai_test_patch_overlap_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let target_file = temp_dir.join("sample.txt");
    fs::write(&target_file, "1\n2\n3\n4\n5\n").unwrap();

    let tool = ApplyPatchTool::new();
    let ctx = ToolContext::new(&temp_dir, "sess_patch_overlap", "agent_patch");

    // Overlapping hunks: hunk 1 covers lines 1..3, hunk 2 tries to target line 2
    let bad_patch = r#"@@ -1,3 +1,3 @@
 1
-2
+two
 3
@@ -2,3 +2,3 @@
 2
-3
+three
 4
"#;

    let args = json!({
        "path": "sample.txt",
        "patch": bad_patch
    });

    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(res.is_error);
    assert!(res.output.contains("overlapping or out-of-order"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_browser_action_validation_and_unquoting() {
    let driver = Arc::new(MockBrowserDriver::new());
    driver
        .set_eval_result("window.name", json!("AntigravityAgent"))
        .await;

    let tool = BrowserActionTool::with_driver(driver);
    let ctx = ToolContext::new("/workspace", "sess_ba_edge", "agent_ba");

    // 1. Evaluate string unquotes cleanly
    let eval_res = tool
        .execute(
            json!({
                "action": "evaluate",
                "script": "window.name"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!eval_res.is_error);
    assert_eq!(eval_res.output, "AntigravityAgent"); // Clean unquoted string

    // 2. Missing url in navigate
    let bad_nav = tool
        .execute(json!({ "action": "navigate", "url": "  " }), &ctx)
        .await
        .unwrap();
    assert!(bad_nav.is_error);
    assert!(bad_nav.output.contains("url is required"));

    // 3. Missing selector in click
    let bad_click = tool
        .execute(json!({ "action": "click", "selector": "" }), &ctx)
        .await
        .unwrap();
    assert!(bad_click.is_error);
    assert!(bad_click.output.contains("selector is required"));
}

#[tokio::test]
async fn test_mcp_client_deterministic_sorting_and_validation() {
    let transport = Arc::new(MockMcpTransport::new());

    // Register tools in reverse alphabetical order
    let tool_z = McpToolDefinition {
        name: "zebra_tool".to_string(),
        description: "Zebra description".to_string(),
        input_schema: json!({ "type": "object" }),
    };
    let tool_a = McpToolDefinition {
        name: "alpha_tool".to_string(),
        description: "Alpha description".to_string(),
        input_schema: json!({ "type": "object" }),
    };

    transport
        .register_tool(tool_z, |_| Box::pin(async { Ok("zebra".to_string()) }))
        .await;
    transport
        .register_tool(tool_a, |_| Box::pin(async { Ok("alpha".to_string()) }))
        .await;

    let client = McpClient::new(transport);
    let tool = McpClientTool::with_client(client);
    let ctx = ToolContext::new("/workspace", "sess_mcp_edge", "agent_mcp");

    // 1. tools/list must be deterministically sorted
    let list_res = tool
        .execute(json!({ "action": "list" }), &ctx)
        .await
        .unwrap();
    assert!(!list_res.is_error);
    let alpha_pos = list_res.output.find("alpha_tool").unwrap();
    let zebra_pos = list_res.output.find("zebra_tool").unwrap();
    assert!(alpha_pos < zebra_pos);

    // 2. Empty tool_name in call returns error
    let bad_call = tool
        .execute(json!({ "action": "call", "tool_name": "" }), &ctx)
        .await
        .unwrap();
    assert!(bad_call.is_error);
    assert!(bad_call.output.contains("tool_name is required"));
}

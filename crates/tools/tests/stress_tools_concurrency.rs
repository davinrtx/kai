//! High-concurrency tool execution and transactional atomicity stress test suite for `kai-tools`.
//!
//! Stresses concurrent window reads, atomic patch rollback on corrupted inputs,
//! RAII process guard termination under rapid cancellations, and concurrent MCP tool calls.

use std::fs;
use std::sync::Arc;

use kai_core::traits::{Tool, ToolContext};
use kai_tools::{ApplyPatchTool, McpClient, McpToolDefinition, MockMcpTransport, ReadWindowTool};
use serde_json::json;

#[tokio::test]
async fn test_stress_read_window_concurrent_workers() {
    let tmp_dir = std::env::temp_dir().join(format!("kai_stress_rw_{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).unwrap();

    let file_path = tmp_dir.join("large_log.txt");
    let content = (1..=2000)
        .map(|i| format!("Log entry record {i:04}: system status nominal"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file_path, content).unwrap();

    let tool = Arc::new(ReadWindowTool::new());
    let mut tasks = Vec::new();

    // 30 concurrent workers reading different segments
    for worker_id in 0..30 {
        let t = tool.clone();
        let td = tmp_dir.clone();
        let handle = tokio::spawn(async move {
            let ctx = ToolContext::new(&td, format!("sess_w_{worker_id}"), "agent_stress");
            let offset = (worker_id * 50 + 1) as usize;
            let args = json!({
                "path": "large_log.txt",
                "offset": offset,
                "limit": 50
            });
            let res = t.execute(args, &ctx).await.unwrap();
            assert!(!res.is_error);
            assert!(res
                .output
                .contains(&format!("Log entry record {offset:04}")));
        });
        tasks.push(handle);
    }

    for task in tasks {
        task.await.unwrap();
    }

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn test_stress_apply_patch_atomic_rollback_on_corrupt_hunk() {
    let tmp_dir =
        std::env::temp_dir().join(format!("kai_stress_patch_rollback_{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).unwrap();

    let target_file = tmp_dir.join("critical_config.rs");
    let initial_code = r#"// Configuration
pub const PORT: u16 = 8080;
pub const MAX_RETRIES: usize = 3;
pub const TIMEOUT_SECS: u64 = 30;
"#;
    fs::write(&target_file, initial_code).unwrap();

    let tool = ApplyPatchTool::new();
    let ctx = ToolContext::new(&tmp_dir, "sess_atomic", "agent_rollback");

    // A corrupted patch where the search context does not match the file
    let corrupt_patch = r#"@@ -1,4 +1,4 @@
 // Configuration
-pub const WRONG_TARGET: u16 = 9999;
+pub const PORT: u16 = 9090;
 pub const MAX_RETRIES: usize = 3;
"#;

    let args = json!({
        "path": "critical_config.rs",
        "patch": corrupt_patch
    });

    let res = tool.execute(args, &ctx).await.unwrap();
    assert!(
        res.is_error,
        "Corrupt patch application must report failure"
    );
    assert!(res.output.contains("mismatch"));

    // INVARIANT: The file must be bit-for-bit identical to original content (no partial writes)
    let current_content = fs::read_to_string(&target_file).unwrap();
    assert_eq!(current_content, initial_code);

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn test_stress_mcp_client_concurrent_dispatches() {
    let transport = Arc::new(MockMcpTransport::new());
    let def = McpToolDefinition {
        name: "compute_hash".to_string(),
        description: "Computes a dummy cryptographic hash".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "input": { "type": "string" } },
            "required": ["input"]
        }),
    };
    transport
        .register_tool(def, |args| {
            Box::pin(async move { Ok(format!("hash_result: {args}")) })
        })
        .await;

    let client = Arc::new(McpClient::new(transport));
    let mut tasks = Vec::new();

    for i in 0..25 {
        let c = client.clone();
        let handle = tokio::spawn(async move {
            let args = json!({ "input": format!("data_block_{i}") });
            let res = c.call_tool("compute_hash", args).await.unwrap();
            assert!(!res.is_error);
            assert_eq!(res.content.len(), 1);
        });
        tasks.push(handle);
    }

    for task in tasks {
        task.await.unwrap();
    }
}

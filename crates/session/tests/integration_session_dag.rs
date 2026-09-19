//! Exhaustive integration test suite for `kai-session`.
//!
//! Verifies DAG branching, cycle detection, multi-parent merge nodes,
//! branch pointer coordination, Git commit indexing, transactional file persistence,
//! and automated context compaction.

use std::sync::Arc;

use kai_core::error::Result;
use kai_core::message::Message;
use kai_core::traits::{BoxFuture, ContextProcessor, SessionNode, SessionStore};
use kai_session::{
    AutoCompactor, BranchManager, FileSessionStore, MemorySessionStore, DEFAULT_BRANCH_NAME,
};

#[tokio::test]
async fn test_dag_node_crud_and_branch_divergence() {
    let store = MemorySessionStore::new();

    // 1. Root node
    let root = SessionNode::root("root", Message::user("m0", "Init project"), 1000);
    store.put_node(&root).await.unwrap();

    let fetched = store.get_node("root").await.unwrap();
    assert_eq!(fetched, Some(root.clone()));

    // 2. Divergent branches: Feature A and Feature B
    let a1 = SessionNode::with_parent("a1", "root", Message::assistant("ma1", "Design A"), 2000);
    let a2 = SessionNode::with_parent("a2", "a1", Message::assistant("ma2", "Impl A"), 3000);

    let b1 = SessionNode::with_parent("b1", "root", Message::assistant("mb1", "Design B"), 2100);

    store.put_node(&a1).await.unwrap();
    store.put_node(&a2).await.unwrap();
    store.put_node(&b1).await.unwrap();

    // 3. Children verification
    let root_children = store.get_children("root").await.unwrap();
    assert_eq!(root_children.len(), 2);
    assert_eq!(root_children[0].id, "a1");
    assert_eq!(root_children[1].id, "b1");

    // 4. Branch history linearization
    let history_a = store.get_branch_history("a2").await.unwrap();
    assert_eq!(history_a.len(), 3);
    assert_eq!(history_a[0].id, "root");
    assert_eq!(history_a[1].id, "a1");
    assert_eq!(history_a[2].id, "a2");

    let history_b = store.get_branch_history("b1").await.unwrap();
    assert_eq!(history_b.len(), 2);
    assert_eq!(history_b[0].id, "root");
    assert_eq!(history_b[1].id, "b1");
}

#[tokio::test]
async fn test_dag_cycle_detection_and_invalid_parent() {
    let store = MemorySessionStore::new();

    let root = SessionNode::root("root", Message::user("m0", "Root"), 1000);
    store.put_node(&root).await.unwrap();

    // 1. Pointing to non-existent parent fails
    let bad_parent = SessionNode::with_parent(
        "orphan",
        "non_existent",
        Message::user("m1", "Orphan"),
        2000,
    );
    let orphan_res = store.put_node(&bad_parent).await;
    assert!(orphan_res.is_err());
    let err_str = orphan_res.unwrap_err().to_string();
    assert!(err_str.contains("Node not found") || err_str.contains("non_existent"));

    // 2. Self-parenting is caught by node validation
    let self_parent = SessionNode::with_parent("self", "self", Message::user("m2", "Loop"), 3000);
    assert!(store.put_node(&self_parent).await.is_err());

    // 3. Cycle detection: A -> B -> C -> A
    let node_a = SessionNode::with_parent("node_a", "root", Message::user("ma", "A"), 4000);
    store.put_node(&node_a).await.unwrap();

    let node_b = SessionNode::with_parent("node_b", "node_a", Message::user("mb", "B"), 5000);
    store.put_node(&node_b).await.unwrap();

    // Attempt to update node_a with parent node_b -> cycle!
    let circular =
        SessionNode::with_parent("node_a", "node_b", Message::user("m_cyc", "Cycle"), 6000);
    let cycle_res = store.put_node(&circular).await;
    assert!(cycle_res.is_err());
    assert!(cycle_res
        .unwrap_err()
        .to_string()
        .contains("Cycle detected"));
}

#[tokio::test]
async fn test_dag_merge_nodes_multi_parent() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = BranchManager::new(store.clone());

    let root = SessionNode::root("root", Message::user("m0", "Objective"), 1000);
    store.put_node(&root).await.unwrap();
    store.set_head(DEFAULT_BRANCH_NAME, "root").await.unwrap();

    // Create branch feat_1 and advance
    branch_mgr
        .fork_branch(DEFAULT_BRANCH_NAME, "feat_1")
        .await
        .unwrap();
    let n_f1 = SessionNode::with_parent("n_f1", "root", Message::assistant("m1", "Worker 1"), 2000);
    store.put_node(&n_f1).await.unwrap();
    store.set_head("feat_1", "n_f1").await.unwrap();

    // Advance main
    let n_main = SessionNode::with_parent(
        "n_main",
        "root",
        Message::assistant("m2", "Main step"),
        2500,
    );
    store.put_node(&n_main).await.unwrap();
    store.set_head(DEFAULT_BRANCH_NAME, "n_main").await.unwrap();

    // Merge feat_1 into main
    let merge_msg = Message::assistant("m_merge", "Integrated Worker 1 findings");
    let merge_node = branch_mgr
        .merge_branches("feat_1", DEFAULT_BRANCH_NAME, merge_msg)
        .await
        .unwrap();

    assert!(merge_node.is_merge());
    assert_eq!(merge_node.parent_ids.len(), 2);
    assert_eq!(merge_node.parent_ids[0], "n_main");
    assert_eq!(merge_node.parent_ids[1], "n_f1");

    // Verify main head is now the merge node
    let head_main = store.get_head(DEFAULT_BRANCH_NAME).await.unwrap();
    assert_eq!(head_main.as_deref(), Some(merge_node.id.as_str()));

    // Verify get_children on both branches shows the merge node
    let children_f1 = store.get_children("n_f1").await.unwrap();
    assert!(children_f1.iter().any(|c| c.id == merge_node.id));

    let children_main = store.get_children("n_main").await.unwrap();
    assert!(children_main.iter().any(|c| c.id == merge_node.id));
}

#[tokio::test]
async fn test_branch_manager_lifecycle_and_pruning() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = BranchManager::new(store.clone());

    let root = SessionNode::root("root", Message::user("m0", "Start"), 1000);
    store.put_node(&root).await.unwrap();
    branch_mgr.create_branch("main", "root").await.unwrap();
    branch_mgr.create_branch("feature", "root").await.unwrap();

    assert_eq!(branch_mgr.active_branch().await, "main");
    assert_eq!(
        branch_mgr.active_head().await.unwrap(),
        Some("root".to_string())
    );

    // Switch active branch
    branch_mgr.switch_branch("feature").await.unwrap();
    assert_eq!(branch_mgr.active_branch().await, "feature");

    // Attempt to prune active branch must be rejected
    let prune_active = branch_mgr.prune_branch("feature").await;
    assert!(prune_active.is_err());

    // Switch back to main and prune feature
    branch_mgr.switch_branch("main").await.unwrap();
    assert!(branch_mgr.prune_branch("feature").await.is_ok());

    let branches = branch_mgr.list_branches().await.unwrap();
    assert_eq!(branches, vec!["main"]);

    // Attempt to prune default branch must be rejected
    assert!(store.prune_branch("main").await.is_err());
}

#[tokio::test]
async fn test_git_commit_indexing_and_lookup() {
    let store = MemorySessionStore::new();

    let root = SessionNode::root("root", Message::user("m0", "Repo init"), 1000);
    let commit_node =
        SessionNode::with_parent("node_c1", "root", Message::user("m1", "Commit 1"), 2000)
            .with_git_commit("c0ffee123456");

    store.put_node(&root).await.unwrap();
    store.put_node(&commit_node).await.unwrap();

    let found = store.get_node_by_commit("c0ffee123456").await.unwrap();
    assert_eq!(found, Some(commit_node.clone()));

    let not_found = store.get_node_by_commit("deadbeef0000").await.unwrap();
    assert_eq!(not_found, None);
}

#[tokio::test]
async fn test_file_session_store_transactional_persistence_and_reload() {
    let tmp_dir = std::env::temp_dir().join(format!("kai_test_session_{}", std::process::id()));
    let session_id = "sess_persist_01";

    {
        let store = FileSessionStore::new(&tmp_dir, session_id, true).unwrap();

        let root = SessionNode::root("root", Message::user("m0", "Persistent root"), 1000);
        store.put_node(&root).await.unwrap();
        store.set_head("main", "root").await.unwrap();

        let child =
            SessionNode::with_parent("child", "root", Message::assistant("m1", "Response"), 2000);
        store.put_node(&child).await.unwrap();
        store.set_head("main", "child").await.unwrap();

        // Verify session file exists on disk
        let session_file = store.session_file_path();
        assert!(session_file.exists());

        // Verify no leftover .tmp files
        let read_dir = std::fs::read_dir(&tmp_dir).unwrap();
        for entry in read_dir {
            let path = entry.unwrap().path();
            let file_name = path.file_name().unwrap().to_string_lossy();
            assert!(!file_name.contains(".tmp."));
        }
    }

    // Reload from disk in a fresh instance
    {
        let reloaded = FileSessionStore::new(&tmp_dir, session_id, false).unwrap();
        assert_eq!(reloaded.inner().node_count().await, 2);

        let root = reloaded.get_node("root").await.unwrap();
        assert!(root.is_some());
        assert_eq!(root.unwrap().message.text_content(), "Persistent root");

        let head = reloaded.get_head("main").await.unwrap();
        assert_eq!(head.as_deref(), Some("child"));

        // Delete session and verify cleanup
        reloaded.delete_session(session_id).await.unwrap();
        let session_file = reloaded.session_file_path();
        assert!(!session_file.exists());
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn test_auto_compactor_threshold_and_relinking() {
    let store = MemorySessionStore::new();

    // 1. Build a linear chain of 8 nodes
    let mut last_id = "node_0".to_string();
    let root = SessionNode::root(&last_id, Message::user("m_0", "Turn 0: Start"), 1000);
    store.put_node(&root).await.unwrap();

    for i in 1..8 {
        let node_id = format!("node_{i}");
        let node = SessionNode::with_parent(
            &node_id,
            &last_id,
            Message::assistant(format!("m_{i}"), format!("Turn {i}: Action")),
            1000 + (i as u64) * 100,
        );
        store.put_node(&node).await.unwrap();
        last_id = node_id;
    }

    let history_before = store.get_branch_history(&last_id).await.unwrap();
    assert_eq!(history_before.len(), 8);

    // 2. Configure compactor with max_turns = 5, keep_recent = 2
    let compactor = AutoCompactor::new().with_max_turns(5).with_keep_recent(2);

    assert!(compactor.should_compact(&history_before));

    // 3. Perform compaction
    let compacted_leaf = compactor
        .compact_branch(&store, &last_id)
        .await
        .unwrap()
        .expect("Compaction must produce a new leaf node");

    // 4. Verify new history length: 1 (compaction summary node) + 2 (recent turns) = 3 nodes
    let history_after = store.get_branch_history(&compacted_leaf.id).await.unwrap();
    assert_eq!(history_after.len(), 3);

    assert!(history_after[0].id.starts_with("compact_"));
    assert!(history_after[0]
        .message
        .text_content()
        .contains("[Compacted history: 6 turns condensed]"));

    // Recent turns preserved with original text
    assert_eq!(history_after[1].message.text_content(), "Turn 6: Action");
    assert_eq!(history_after[2].message.text_content(), "Turn 7: Action");
}

#[tokio::test]
async fn test_branch_manager_append_and_active_history() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = BranchManager::new(store.clone());

    // 1. Appending to empty branch creates root node and sets head
    let t0 = branch_mgr
        .append_turn("turn_0", Message::user("m0", "User start"))
        .await
        .unwrap();
    assert!(t0.is_root());
    assert_eq!(
        branch_mgr.active_head().await.unwrap().as_deref(),
        Some("turn_0")
    );

    // 2. Appending second turn links to turn_0
    let t1 = branch_mgr
        .append_turn("turn_1", Message::assistant("m1", "Assistant reply"))
        .await
        .unwrap();
    assert_eq!(t1.parent_ids, vec!["turn_0"]);
    assert_eq!(
        branch_mgr.active_head().await.unwrap().as_deref(),
        Some("turn_1")
    );

    // 3. active_history returns linear sequence
    let hist = branch_mgr.active_history().await.unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].id, "turn_0");
    assert_eq!(hist[1].id, "turn_1");

    // 4. Duplicate branch creation rejected
    let dup_err = branch_mgr
        .create_branch(DEFAULT_BRANCH_NAME, "turn_0")
        .await;
    assert!(dup_err.is_err());
    assert!(dup_err.unwrap_err().to_string().contains("already exists"));

    // 5. Append on a non-active branch
    branch_mgr.create_branch("feature", "turn_0").await.unwrap();
    let f1 = branch_mgr
        .append_to_branch(
            "feature",
            "feat_turn_1",
            Message::assistant("mf1", "Feature step"),
        )
        .await
        .unwrap();
    assert_eq!(f1.parent_ids, vec!["turn_0"]);
    let feat_hist = branch_mgr.branch_history("feature").await.unwrap();
    assert_eq!(feat_hist.len(), 2);
    assert_eq!(feat_hist[1].id, "feat_turn_1");
}

struct MockCompressor {
    token_multiplier: usize,
}

impl ContextProcessor for MockCompressor {
    fn estimate_tokens(&self, messages: &[Message]) -> usize {
        messages.len() * self.token_multiplier
    }

    fn process<'a>(
        &'a self,
        messages: &'a [Message],
        _target_tokens: usize,
    ) -> BoxFuture<'a, Result<Vec<Message>>> {
        Box::pin(async move {
            let summary_text = format!("Compressed {} messages intelligently", messages.len());
            Ok(vec![Message::system("compressed_msg", summary_text)])
        })
    }
}

#[tokio::test]
async fn test_auto_compactor_with_context_processor() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = BranchManager::new(store.clone());

    for i in 0..10 {
        branch_mgr
            .append_turn(
                format!("turn_{i}"),
                Message::user(format!("m_{i}"), format!("Content {i}")),
            )
            .await
            .unwrap();
    }

    let history = branch_mgr.active_history().await.unwrap();
    assert_eq!(history.len(), 10);

    // Attach mock compressor: 10 messages * 1000 = 10000 tokens > 5000 max_tokens
    let processor = Arc::new(MockCompressor {
        token_multiplier: 1000,
    });
    let compactor = AutoCompactor::new()
        .with_max_turns(50) // High turns, so token budget triggers compaction
        .with_max_tokens(5000)
        .with_keep_recent(3)
        .with_context_processor(processor);

    assert!(compactor.should_compact(&history));

    let compacted = branch_mgr
        .compact_active_branch(&compactor)
        .await
        .unwrap()
        .expect("Must compact due to token limit");

    let history_after = branch_mgr.active_history().await.unwrap();
    assert_eq!(history_after.len(), 4); // 1 summary node + 3 recent
    assert_eq!(
        branch_mgr.active_head().await.unwrap().as_deref(),
        Some(compacted.id.as_str())
    );
    assert!(history_after[0]
        .message
        .text_content()
        .contains("Compressed 7 messages intelligently"));
}

#[tokio::test]
async fn test_file_session_store_path_traversal_protection() {
    let tmp_dir = std::env::temp_dir().join(format!("kai_test_sec_{}", std::process::id()));

    // Slanted path separator or traversal patterns must be rejected
    assert!(FileSessionStore::new(&tmp_dir, "../evil_session", false).is_err());
    assert!(FileSessionStore::new(&tmp_dir, "sub/dir/session", false).is_err());
    assert!(FileSessionStore::new(&tmp_dir, "   ", false).is_err());

    let valid_store = FileSessionStore::new(&tmp_dir, "valid_session", false).unwrap();
    assert_eq!(valid_store.session_id(), "valid_session");
    assert_eq!(valid_store.storage_dir(), &tmp_dir);

    // Delete with invalid session ID is rejected
    assert!(valid_store.delete_session("../evil").await.is_err());

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn test_graph_stale_children_cleanup_on_parent_change() {
    let store = MemorySessionStore::new();

    let root = SessionNode::root("root", Message::user("m0", "Root"), 1000);
    let p1 = SessionNode::with_parent("p1", "root", Message::user("mp1", "P1"), 2000);
    let p2 = SessionNode::with_parent("p2", "root", Message::user("mp2", "P2"), 2100);
    let child = SessionNode::with_parent("child", "p1", Message::user("mc", "Child of P1"), 3000);

    store.put_node(&root).await.unwrap();
    store.put_node(&p1).await.unwrap();
    store.put_node(&p2).await.unwrap();
    store.put_node(&child).await.unwrap();

    let p1_children = store.get_children("p1").await.unwrap();
    assert_eq!(p1_children.len(), 1);
    assert_eq!(p1_children[0].id, "child");

    // Reparent child to p2
    let updated_child =
        SessionNode::with_parent("child", "p2", Message::user("mc", "Child of P2"), 4000);
    store.put_node(&updated_child).await.unwrap();

    // p1 should no longer list child
    let p1_children_after = store.get_children("p1").await.unwrap();
    assert_eq!(p1_children_after.len(), 0);

    // p2 should now list child
    let p2_children = store.get_children("p2").await.unwrap();
    assert_eq!(p2_children.len(), 1);
    assert_eq!(p2_children[0].id, "child");
}

#[tokio::test]
async fn test_trajectory_exporter_jsonl_and_sharegpt() {
    use kai_session::TrajectoryExporter;

    let exporter = TrajectoryExporter::new();

    let root = SessionNode::root("n1", Message::system("m1", "You are an assistant"), 1000);
    let user_turn = SessionNode::with_parent("n2", "n1", Message::user("m2", "Deploy code"), 2000);
    let assist_turn =
        SessionNode::with_parent("n3", "n2", Message::assistant("m3", "Deploying now"), 3000);

    let history = vec![root, user_turn, assist_turn];

    // 1. JSONL Export
    let jsonl = exporter.export_jsonl(&history).unwrap();
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("\"role\":\"system\""));
    assert!(lines[1].contains("\"role\":\"user\""));
    assert!(lines[2].contains("\"role\":\"assistant\""));

    // 2. ShareGPT Export
    let sharegpt = exporter.export_sharegpt(&history).unwrap();
    let convos = sharegpt["conversations"].as_array().unwrap();
    assert_eq!(convos.len(), 3);
    assert_eq!(convos[0]["from"], "system");
    assert_eq!(convos[1]["from"], "human");
    assert_eq!(convos[2]["from"], "gpt");
    assert_eq!(convos[1]["value"], "Deploy code");
}

#[tokio::test]
async fn test_session_search_bm25_index() {
    use kai_session::SessionSearchIndex;

    let mut index = SessionSearchIndex::new();

    let n1 = SessionNode::root(
        "node_1",
        Message::user("m1", "Optimize Rust memory allocations and compile times"),
        1000,
    );
    let n2 = SessionNode::root(
        "node_2",
        Message::user("m2", "Configure Docker containers and kubernetes ingress"),
        2000,
    );
    let n3 = SessionNode::root(
        "node_3",
        Message::user("m3", "Debug database connection pool timeout in postgresql"),
        3000,
    );
    let n4 = SessionNode::root(
        "node_4",
        Message::user("m4", "Rust compiler error with lifetime parameter 'a"),
        4000,
    );

    index.index_history(&[n1, n2, n3, n4]);

    // Search for 'rust' and 'compile'
    let results = index.search("rust compile", 10);
    assert!(!results.is_empty());
    assert_eq!(results[0].node_id, "node_1");
    assert!(results[0].snippet.contains("Rust"));
    assert!(results[0].matched_terms.contains(&"rust".to_string()));

    // Search for 'database'
    let db_results = index.search("database postgresql", 5);
    assert_eq!(db_results.len(), 1);
    assert_eq!(db_results[0].node_id, "node_3");

    // Search non-existent term
    let empty_results = index.search("nonexistentwordxyz", 5);
    assert!(empty_results.is_empty());
}

#[tokio::test]
async fn test_session_search_idempotency_and_linear_indexing() {
    use kai_session::SessionSearchIndex;

    let mut index = SessionSearchIndex::new();

    let node = SessionNode::root(
        "unique_node",
        Message::user("u1", "Rust memory safety guarantees"),
        1000,
    );

    // Index once
    index.index_node(&node);
    let res1 = index.search("safety", 5);
    assert_eq!(res1.len(), 1);
    let score1 = res1[0].score;

    // Index twice (same node) - must be idempotent
    index.index_node(&node);
    let res2 = index.search("safety", 5);
    assert_eq!(res2.len(), 1);
    assert!((res2[0].score - score1).abs() < f64::EPSILON);

    // Re-index with updated text
    let updated_node = SessionNode::root(
        "unique_node",
        Message::user("u1", "Go garbage collector concurrency"),
        2000,
    );
    index.index_node(&updated_node);
    assert!(index.search("safety", 5).is_empty());
    let go_res = index.search("concurrency", 5);
    assert_eq!(go_res.len(), 1);
    assert_eq!(go_res[0].node_id, "unique_node");

    // Remove node
    index.remove_node("unique_node");
    assert!(index.search("concurrency", 5).is_empty());
}

#[tokio::test]
async fn test_trajectory_exporter_structured_tool_calls() {
    use kai_core::message::{ToolCall, ToolResult};
    use kai_session::TrajectoryExporter;
    use serde_json::json;

    let exporter = TrajectoryExporter::new();

    let call = ToolCall::new(
        "call_01",
        "read_window",
        json!({ "offset": 1, "limit": 10 }),
    );
    let msg_call = Message::tool_calls("msg_assistant_1", vec![call]);

    let res = ToolResult::success("call_01", "fn main() {}");
    let msg_res = Message::tool_results("msg_tool_1", vec![res]);

    let n1 = SessionNode::root("n1", msg_call, 1000);
    let n2 = SessionNode::with_parent("n2", "n1", msg_res, 2000);

    let sharegpt = exporter.export_sharegpt(&[n1, n2]).unwrap();
    let convos = sharegpt["conversations"].as_array().unwrap();
    assert_eq!(convos.len(), 2);

    assert_eq!(convos[0]["from"], "gpt");
    let val_call = convos[0]["value"].as_str().unwrap();
    assert!(val_call.contains("<tool_call>"));
    assert!(val_call.contains("\"name\":\"read_window\""));
    assert!(val_call.contains("\"limit\":10"));

    assert_eq!(convos[1]["from"], "tool");
    let val_res = convos[1]["value"].as_str().unwrap();
    assert!(val_res.contains("<tool_response id=\"call_01\">fn main() {}</tool_response>"));
}

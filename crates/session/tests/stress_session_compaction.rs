//! High-turn compaction and parallel branch divergence stress test suite.
//!
//! Stresses deep DAG chains (100+ nodes), multi-branch fan-out and merge trees,
//! and concurrent multi-threaded store operations.

use std::sync::Arc;

use kai_core::message::Message;
use kai_session::{AutoCompactor, BranchManager, MemorySessionStore, DEFAULT_BRANCH_NAME};

#[tokio::test]
async fn test_stress_100_turn_linear_compaction() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = BranchManager::new(store.clone());

    // 1. Build a deep linear chain of 100 turns
    for i in 0..100 {
        let msg = if i % 2 == 0 {
            Message::user(format!("m_{i}"), format!("User turn {i}: Execute task"))
        } else {
            Message::assistant(
                format!("m_{i}"),
                format!("Assistant response {i}: Completed"),
            )
        };
        branch_mgr
            .append_turn(format!("turn_{i}"), msg)
            .await
            .unwrap();
    }

    let history_before = branch_mgr.active_history().await.unwrap();
    assert_eq!(history_before.len(), 100);

    // 2. Compactor with max_turns = 20, keep_recent = 5
    let compactor = AutoCompactor::new().with_max_turns(20).with_keep_recent(5);

    assert!(compactor.should_compact(&history_before));

    // 3. Perform compaction
    let compacted_leaf = branch_mgr
        .compact_active_branch(&compactor)
        .await
        .unwrap()
        .expect("Compaction must succeed");

    // 4. Verify post-compaction history: 1 summary node + 5 recent nodes = 6 nodes
    let history_after = branch_mgr.active_history().await.unwrap();
    assert_eq!(history_after.len(), 6);
    assert_eq!(history_after.last().unwrap().id, compacted_leaf.id);

    // Summary node verifies 95 turns were condensed
    assert!(history_after[0]
        .message
        .text_content()
        .contains("[Compacted history: 95 turns condensed]"));

    // Recent 5 turns preserved in order (turns 95, 96, 97, 98, 99)
    assert!(history_after[1]
        .message
        .text_content()
        .contains("Assistant response 95"));
    assert!(history_after[5]
        .message
        .text_content()
        .contains("Assistant response 99"));
}

#[tokio::test]
async fn test_stress_parallel_branch_divergence_and_multi_merges() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = BranchManager::new(store.clone());

    // Root initialization
    let root = branch_mgr
        .append_turn("root", Message::user("m0", "Project Genesis"))
        .await
        .unwrap();

    // Fork 5 parallel worker branches from root
    let branch_names = ["feat_ui", "feat_db", "feat_net", "feat_auth", "feat_crypto"];

    for name in &branch_names {
        branch_mgr
            .fork_branch(DEFAULT_BRANCH_NAME, name)
            .await
            .unwrap();
        // Each worker performs 5 discrete turns
        for turn_idx in 0..5 {
            branch_mgr
                .append_to_branch(
                    name,
                    format!("{name}_turn_{turn_idx}"),
                    Message::assistant(
                        format!("m_{name}_{turn_idx}"),
                        format!("Branch {name} work step {turn_idx}"),
                    ),
                )
                .await
                .unwrap();
        }
    }

    // Verify all 5 branches have distinct linear histories of length 6 (root + 5 turns)
    for name in &branch_names {
        let hist = branch_mgr.branch_history(name).await.unwrap();
        assert_eq!(hist.len(), 6);
        assert_eq!(hist[0].id, root.id);
    }

    // Sequentially merge all 5 branches back into main
    for name in &branch_names {
        let merge_msg = Message::assistant(
            format!("m_merge_{name}"),
            format!("Integrated changes from {name}"),
        );
        let merge_node = branch_mgr
            .merge_branches(name, DEFAULT_BRANCH_NAME, merge_msg)
            .await
            .unwrap();
        assert!(merge_node.is_merge());
        assert_eq!(merge_node.parent_ids.len(), 2);
    }

    // Verify main head is now the final merge node
    let main_head = branch_mgr.active_head().await.unwrap();
    assert!(main_head.is_some());

    // Verify list_branches contains all 5 plus main
    let all_branches = branch_mgr.list_branches().await.unwrap();
    assert_eq!(all_branches.len(), 6);
}

#[tokio::test]
async fn test_stress_concurrent_multi_threaded_dag_operations() {
    let store = Arc::new(MemorySessionStore::new());
    let branch_mgr = Arc::new(BranchManager::new(store.clone()));

    // Root node
    branch_mgr
        .append_turn("root", Message::user("m0", "Root Task"))
        .await
        .unwrap();

    let mut handles = Vec::new();

    // 8 concurrent tasks creating branches and appending nodes
    for worker_id in 0..8 {
        let bm = branch_mgr.clone();
        let handle = tokio::spawn(async move {
            let branch_name = format!("worker_branch_{worker_id}");
            bm.fork_branch(DEFAULT_BRANCH_NAME, &branch_name)
                .await
                .unwrap();

            for step in 0..20 {
                bm.append_to_branch(
                    &branch_name,
                    format!("w_{worker_id}_step_{step}"),
                    Message::assistant(format!("m_{worker_id}_{step}"), format!("Step {step}")),
                )
                .await
                .unwrap();
            }

            let hist = bm.branch_history(&branch_name).await.unwrap();
            assert_eq!(hist.len(), 21); // root + 20 steps
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.unwrap();
    }

    // Total nodes in store: 1 root + (8 workers * 20 steps) = 161 nodes
    assert_eq!(store.node_count().await, 161);
}

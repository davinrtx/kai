//! High-concurrency event bus and adversarial boundary stress test suite for `kai-core`.
//!
//! Stresses event broadcasting across concurrent tasks, verifies global steering
//! propagation races, and validates multi-byte UTF-8 truncation boundary safety.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kai_core::event::{global_steering_channel, Event, EventBus, SteeringState};
use kai_core::message::{
    truncate_items, truncate_output, ContentBlock, Message, Role, MAX_TOOL_OUTPUT_BYTES,
    MAX_TOOL_OUTPUT_ITEMS, TRUNCATION_BYTE_NOTICE,
};

#[tokio::test]
async fn test_stress_event_bus_high_volume_broadcast() {
    let bus = Arc::new(EventBus::new(256));
    let mut sub1 = bus.subscribe();
    let mut sub2 = bus.subscribe();

    let total_producers = 20;
    let events_per_producer = 50;
    let expected_events = total_producers * events_per_producer;

    // Launch concurrent publishers
    let mut handles = Vec::new();
    for p_id in 0..total_producers {
        let b = bus.clone();
        let handle = tokio::spawn(async move {
            for e_id in 0..events_per_producer {
                let event = Event::AgentStarted {
                    agent_id: format!("sess_{p_id}_{e_id}"),
                    task: "main".to_string(),
                };
                // Best-effort send into bounded channel
                let _ = b.publish(event);
                tokio::task::yield_now().await;
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.unwrap();
    }

    // Drain events from subscribers (accounting for bounded channel lag if any)
    let count1 = Arc::new(AtomicUsize::new(0));
    let count1_clone = count1.clone();
    let reader1 = tokio::spawn(async move {
        while tokio::time::timeout(Duration::from_millis(50), sub1.recv())
            .await
            .is_ok()
        {
            count1_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    let count2 = Arc::new(AtomicUsize::new(0));
    let count2_clone = count2.clone();
    let reader2 = tokio::spawn(async move {
        while tokio::time::timeout(Duration::from_millis(50), sub2.recv())
            .await
            .is_ok()
        {
            count2_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    let _ = tokio::join!(reader1, reader2);

    let received1 = count1.load(Ordering::SeqCst);
    let received2 = count2.load(Ordering::SeqCst);

    // Both subscribers should have captured events without deadlocks
    assert!(received1 > 0);
    assert!(received2 > 0);
    assert!(received1 <= expected_events);
    assert!(received2 <= expected_events);
}

#[tokio::test]
async fn test_stress_global_steering_concurrency_race() {
    let (tx, rx) = global_steering_channel();
    let counter = Arc::new(AtomicUsize::new(0));
    let interrupted_counter = Arc::new(AtomicUsize::new(0));

    let mut workers = Vec::new();
    for _ in 0..30 {
        let local_rx = rx.clone();
        let c = counter.clone();
        let ic = interrupted_counter.clone();
        let h = tokio::spawn(async move {
            for _ in 0..200 {
                if *local_rx.borrow() == SteeringState::Terminated {
                    ic.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                c.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        });
        workers.push(h);
    }

    // Allow workers to make slight progress, then terminate
    tokio::time::sleep(Duration::from_millis(1)).await;
    let _ = tx.send(SteeringState::Terminated);

    for w in workers {
        w.await.unwrap();
    }

    // At least some workers must have caught the interruption signal
    assert!(interrupted_counter.load(Ordering::SeqCst) > 0);
}

#[test]
fn test_stress_utf8_multibyte_truncation_safety() {
    // 4-byte UTF-8 character: 🦀 (U+1F980)
    let crab = "🦀";
    assert_eq!(crab.len(), 4);

    // Construct a payload where MAX_TOOL_OUTPUT_BYTES lands exactly in the middle of a 4-byte character
    let notice_len = TRUNCATION_BYTE_NOTICE.len();
    let target_body_len = MAX_TOOL_OUTPUT_BYTES - notice_len;

    // Fill with 'a' until 2 bytes before the boundary, then add 4-byte emojis exceeding MAX_TOOL_OUTPUT_BYTES
    let prefix_len = target_body_len - 2;
    let mut payload = "a".repeat(prefix_len);
    payload.push_str(&"🦀".repeat(50)); // Exceeds 4096 bytes and forces boundary alignment

    // Truncate output - must not panic on character boundary!
    let truncated = truncate_output(&payload);

    assert!(truncated.len() <= MAX_TOOL_OUTPUT_BYTES);
    assert!(truncated.contains(TRUNCATION_BYTE_NOTICE));
    assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());

    // Test with 3-byte CJK characters: "中" (3 bytes)
    let cjk = "中".repeat(2000);
    let cjk_truncated = truncate_output(&cjk);
    assert!(cjk_truncated.len() <= MAX_TOOL_OUTPUT_BYTES);
    assert!(cjk_truncated.contains(TRUNCATION_BYTE_NOTICE));
    assert!(std::str::from_utf8(cjk_truncated.as_bytes()).is_ok());
}

#[test]
fn test_stress_item_truncation_limits() {
    let items: Vec<String> = (0..200).map(|i| format!("item_{i}")).collect();
    let (truncated, _notice) = truncate_items(&items);

    assert_eq!(truncated.len(), MAX_TOOL_OUTPUT_ITEMS);
    assert_eq!(truncated[0], "item_0");
    assert_eq!(
        truncated[MAX_TOOL_OUTPUT_ITEMS - 1],
        format!("item_{}", MAX_TOOL_OUTPUT_ITEMS - 1)
    );
}

#[test]
fn test_stress_nested_message_serialization_invariants() {
    let mut blocks = Vec::new();
    for i in 0..100 {
        blocks.push(ContentBlock::text(format!("Block analysis step {i}")));
        blocks.push(ContentBlock::thinking(format!("Deep reasoning step {i}")));
    }

    let msg = Message::new("msg_stress", Role::Assistant, blocks);
    assert_eq!(msg.content.len(), 200);

    let serialized = serde_json::to_string(&msg).expect("serialization must succeed");
    let deserialized: Message =
        serde_json::from_str(&serialized).expect("deserialization must succeed");

    assert_eq!(deserialized.id, msg.id);
    assert_eq!(deserialized.content.len(), 200);
    assert!(deserialized
        .text_content()
        .contains("Block analysis step 99"));
}

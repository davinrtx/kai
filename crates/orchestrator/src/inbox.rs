//! Asynchronous task inbox backed by bounded MPSC channels.
//!
//! Provides [`TaskInbox`], [`InboxSender`], and [`InboxReceiver`] for deterministic,
//! non-blocking, and backpressure-regulated message passing across agent boundaries.

use kai_core::error::{KaiError, OrchestratorError, Result};
use kai_core::message::Message;
use tokio::sync::{mpsc, Mutex};

/// Default capacity for bounded task inbox channels.
pub const DEFAULT_INBOX_CAPACITY: usize = 128;

/// Transmitting end of a [`TaskInbox`].
#[derive(Debug, Clone)]
pub struct InboxSender {
    tx: mpsc::Sender<Message>,
}

impl InboxSender {
    /// Sends a message into the inbox, waiting asynchronously if the channel is full.
    pub async fn send(&self, message: Message) -> Result<()> {
        self.tx
            .send(message)
            .await
            .map_err(|_| KaiError::Orchestrator(OrchestratorError::TaskInboxClosed))
    }

    /// Attempts to send a message immediately without waiting for capacity.
    pub fn try_send(&self, message: Message) -> Result<()> {
        self.tx.try_send(message).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => {
                KaiError::Orchestrator(OrchestratorError::Interrupted {
                    reason: "Task inbox is full (backpressure capacity reached)".to_string(),
                })
            }
            mpsc::error::TrySendError::Closed(_) => {
                KaiError::Orchestrator(OrchestratorError::TaskInboxClosed)
            }
        })
    }

    /// Returns the remaining capacity of the bounded queue.
    pub fn capacity(&self) -> usize {
        self.tx.capacity()
    }

    /// Returns true if the channel receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Receiving end of a [`TaskInbox`].
#[derive(Debug)]
pub struct InboxReceiver {
    rx: mpsc::Receiver<Message>,
}

impl InboxReceiver {
    /// Awaits the next message from the inbox.
    ///
    /// Returns [`None`] if the channel has been closed and all messages drained.
    pub async fn recv(&mut self) -> Option<Message> {
        self.rx.recv().await
    }

    /// Collects a batch of available messages up to `max_items`.
    ///
    /// Awaits the first message if the inbox is empty, then immediately drains any
    /// additional pending messages without blocking.
    pub async fn recv_batch(&mut self, max_items: usize) -> Vec<Message> {
        if max_items == 0 {
            return Vec::new();
        }

        let mut batch = Vec::with_capacity(max_items.min(32));

        if let Some(first) = self.rx.recv().await {
            batch.push(first);

            while batch.len() < max_items {
                match self.rx.try_recv() {
                    Ok(msg) => batch.push(msg),
                    Err(_) => break,
                }
            }
        }

        batch
    }

    /// Drains all immediately available messages in the queue without blocking.
    pub fn drain_pending(&mut self) -> Vec<Message> {
        let mut batch = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            batch.push(msg);
        }
        batch
    }

    /// Closes the receiving channel, preventing new messages from being sent.
    pub fn close(&mut self) {
        self.rx.close();
    }
}

/// Asynchronous mailbox coordinating message ingestion for an agent.
pub struct TaskInbox {
    sender: InboxSender,
    receiver: Mutex<InboxReceiver>,
}

impl Default for TaskInbox {
    fn default() -> Self {
        Self::new(DEFAULT_INBOX_CAPACITY)
    }
}

impl TaskInbox {
    /// Constructs a new bounded [`TaskInbox`] with the designated capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        Self {
            sender: InboxSender { tx },
            receiver: Mutex::new(InboxReceiver { rx }),
        }
    }

    /// Creates a detached sender/receiver pair for cross-task coordination.
    pub fn channel(capacity: usize) -> (InboxSender, InboxReceiver) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (InboxSender { tx }, InboxReceiver { rx })
    }

    /// Obtains a clone of the transmitting end.
    pub fn sender(&self) -> InboxSender {
        self.sender.clone()
    }

    /// Enqueues a message into the inbox.
    pub async fn enqueue(&self, message: Message) -> Result<()> {
        self.sender.send(message).await
    }

    /// Attempts to enqueue a message without waiting.
    pub fn try_enqueue(&self, message: Message) -> Result<()> {
        self.sender.try_send(message)
    }

    /// Dequeues the next message, awaiting arrival if currently empty.
    pub async fn dequeue(&self) -> Option<Message> {
        let mut guard = self.receiver.lock().await;
        guard.recv().await
    }

    /// Dequeues a batch of up to `max_items` messages.
    pub async fn dequeue_batch(&self, max_items: usize) -> Vec<Message> {
        let mut guard = self.receiver.lock().await;
        guard.recv_batch(max_items).await
    }

    /// Drains all pending messages immediately without blocking.
    pub async fn drain_all(&self) -> Vec<Message> {
        let mut guard = self.receiver.lock().await;
        guard.drain_pending()
    }

    /// Closes the underlying channel.
    pub async fn close(&self) {
        let mut guard = self.receiver.lock().await;
        guard.close();
    }

    /// Returns the remaining capacity of the inbox.
    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    /// Returns true if the inbox channel is closed.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

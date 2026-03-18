//! bus — MessageBus trait for pub/sub with consumer group semantics.
//!
//! Supports Redis Streams (production) and in-memory (testing/backtest).
//! Provides fault-tolerant message delivery with acknowledgment, replay, and claim support.
//! Uses consumer groups to ensure each message is processed by exactly one consumer.

use chrono::{DateTime, Utc};
use std::time::Duration;

pub use types::MessageId;

/// Result type for bus operations.
pub type Result<T> = std::result::Result<T, anyhow::Error>;

/// ─── Message Bus ───
///
/// Fault-tolerant pub/sub with consumer groups (Redis Streams model).
/// Each message is delivered to exactly one consumer in a consumer group.
/// Supports acknowledgment, pending message replay, and claim semantics.
#[async_trait::async_trait]
pub trait MessageBus: Send + Sync {
    /// Publish payload to a topic. Returns message ID.
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId>;

    /// Subscribe to a topic with consumer group semantics.
    /// Each message in the group is delivered to exactly one consumer.
    /// Returns a receiver for new messages.
    ///
    /// # Arguments
    /// * `topic` - Topic name (e.g., "signals", "fills", "risk_events")
    /// * `group` - Consumer group name (e.g., "risk_gate", "dashboard")
    /// * `consumer` - Consumer ID within the group (e.g., "risk_gate_1")
    async fn subscribe(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<BusMessage>>;

    /// Acknowledge message processing.
    /// Must be called after successfully processing a message.
    /// Prevents message from being replayed on restart.
    async fn ack(&self, topic: &str, group: &str, msg_id: &MessageId) -> Result<()>;

    /// Get pending (unacknowledged) messages older than `min_idle`.
    /// Used to recover stuck messages on startup.
    async fn pending(
        &self,
        topic: &str,
        group: &str,
        min_idle: Duration,
    ) -> Result<Vec<BusMessage>>;

    /// Claim stuck messages for reprocessing.
    /// Used when a consumer crashes and another consumer takes over.
    async fn claim(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
        msg_ids: &[MessageId],
    ) -> Result<Vec<BusMessage>>;

    /// Get topic metadata (length, consumer groups, oldest/newest message).
    async fn topic_info(&self, topic: &str) -> Result<TopicInfo>;
}

/// Single message received from bus.
#[derive(Debug, Clone)]
pub struct BusMessage {
    /// Unique message ID (assigned by broker)
    pub id: MessageId,
    /// Topic name
    pub topic: String,
    /// Message payload (typically JSON)
    pub payload: Vec<u8>,
    /// When message was published
    pub published_at: DateTime<Utc>,
}

/// Metadata about a topic.
#[derive(Debug, Clone)]
pub struct TopicInfo {
    /// Total number of messages in topic
    pub length: u64,
    /// Consumer groups subscribed to this topic
    pub consumer_groups: Vec<ConsumerGroupInfo>,
    /// Oldest message timestamp (if any)
    pub oldest_message: Option<DateTime<Utc>>,
    /// Newest message timestamp (if any)
    pub newest_message: Option<DateTime<Utc>>,
}

/// Metadata about a consumer group.
#[derive(Debug, Clone)]
pub struct ConsumerGroupInfo {
    /// Group name
    pub name: String,
    /// Number of active consumers in group
    pub consumers: u32,
    /// Number of pending (unacknowledged) messages
    pub pending: u64,
    /// Last delivered message ID
    pub last_delivered: Option<MessageId>,
}

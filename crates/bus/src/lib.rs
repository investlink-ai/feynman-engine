//! bus — MessageBus trait for pub/sub with consumer group semantics.
//!
//! Supports Redis Streams (production) and in-memory (testing/backtest).
//! Provides fault-tolerant message delivery with acknowledgment, replay, and claim support.
//!
//! **Invariant:** All channels are bounded. No `mpsc::unbounded_channel()`.

use chrono::{DateTime, Utc};
use std::time::Duration;

pub use types::MessageId;

/// Typed errors for bus operations (thiserror for libs).
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("bus connection failed: {0}")]
    ConnectionFailed(String),

    #[error("topic not found: {0}")]
    TopicNotFound(String),

    #[error("publish failed on topic {topic}: {reason}")]
    PublishFailed { topic: String, reason: String },

    #[error("consumer group {group} not found on topic {topic}")]
    GroupNotFound { topic: String, group: String },

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, BusError>;

/// Fault-tolerant pub/sub with consumer groups (Redis Streams model).
///
/// Each message is delivered to exactly one consumer in a consumer group.
/// All channels returned are **bounded** (capacity specified by implementation).
#[async_trait::async_trait]
pub trait MessageBus: Send + Sync {
    /// Publish payload to a topic. Returns message ID.
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId>;

    /// Subscribe to a topic with consumer group semantics.
    ///
    /// Returns a **bounded** receiver. The implementation determines capacity
    /// (typically 1000-10000 messages). Backpressure is applied if the
    /// consumer falls behind.
    ///
    /// # Arguments
    /// * `topic` - Topic name (e.g., "signals", "fills", "risk_events")
    /// * `group` - Consumer group name (e.g., "risk_gate", "dashboard")
    /// * `consumer` - Consumer ID within the group
    async fn subscribe(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<BusMessage>>;

    /// Acknowledge message processing.
    async fn ack(&self, topic: &str, group: &str, msg_id: &MessageId) -> Result<()>;

    /// Get pending (unacknowledged) messages older than `min_idle`.
    async fn pending(
        &self,
        topic: &str,
        group: &str,
        min_idle: Duration,
    ) -> Result<Vec<BusMessage>>;

    /// Claim stuck messages for reprocessing.
    async fn claim(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
        msg_ids: &[MessageId],
    ) -> Result<Vec<BusMessage>>;

    /// Get topic metadata.
    async fn topic_info(&self, topic: &str) -> Result<TopicInfo>;

    /// Check if the bus connection is healthy.
    async fn health_check(&self) -> Result<()>;
}

/// Single message received from bus.
#[derive(Debug, Clone)]
pub struct BusMessage {
    pub id: MessageId,
    pub topic: String,
    pub payload: Vec<u8>,
    pub published_at: DateTime<Utc>,
}

/// Metadata about a topic.
#[derive(Debug, Clone)]
pub struct TopicInfo {
    pub length: u64,
    pub consumer_groups: Vec<ConsumerGroupInfo>,
    pub oldest_message: Option<DateTime<Utc>>,
    pub newest_message: Option<DateTime<Utc>>,
}

/// Metadata about a consumer group.
#[derive(Debug, Clone)]
pub struct ConsumerGroupInfo {
    pub name: String,
    pub consumers: u32,
    pub pending: u64,
    pub last_delivered: Option<MessageId>,
}

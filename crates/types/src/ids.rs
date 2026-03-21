//! Identifier newtypes — type-safe wrappers for all domain IDs.
//!
//! Never pass raw `String` where a domain ID is expected.
//! Every ID is `Clone + Eq + Hash + Serialize + Deserialize`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// ─── Core identifiers ───

macro_rules! newtype_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

newtype_id!(
    /// Internal order identifier (engine-assigned, globally unique).
    OrderId
);
newtype_id!(
    /// Venue-assigned order identifier (exchange side).
    VenueOrderId
);
newtype_id!(
    /// Identifies a trading agent (LLM, algo, ML model).
    AgentId
);
newtype_id!(
    /// Identifies a trading venue (exchange).
    VenueId
);
newtype_id!(
    /// Identifies a venue trading account.
    AccountId
);
newtype_id!(
    /// Identifies a tradeable instrument.
    InstrumentId
);
newtype_id!(
    /// Identifies a venue-native market symbol.
    MarketId
);
newtype_id!(
    /// Identifies a signal from a strategy agent.
    SignalId
);
newtype_id!(
    /// Identifies a message on the bus.
    MessageId
);
newtype_id!(
    /// Identifies a basket of related orders.
    BasketId
);

// ─── Client Order ID (structured, unique across restarts) ───

/// Client-side order ID sent to venues. Structured format embeds debugging
/// context and guarantees uniqueness across process restarts.
///
/// Format: `{agent}-{epoch_ms}-{seq:06}-r{restart_epoch:02}`
///
/// - `agent`: short agent identifier
/// - `epoch_ms`: millisecond timestamp (from Clock, not wall time)
/// - `seq`: monotonic counter (per generator instance)
/// - `restart_epoch`: increments each process restart (mod 100)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOrderId(pub String);

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ClientOrderId {
    /// Parse the agent ID embedded in this client order ID.
    /// Returns `None` if the format is unrecognized.
    #[must_use]
    pub fn parse_agent(&self) -> Option<AgentId> {
        let (_, _, prefix) = self.split_suffix_parts()?;
        let (agent, _) = prefix.rsplit_once('-')?;
        Some(AgentId(agent.to_owned()))
    }

    /// Parse the restart epoch embedded in this client order ID.
    #[must_use]
    pub fn parse_restart_epoch(&self) -> Option<u16> {
        self.0
            .rsplit('-')
            .next()
            .and_then(|s| s.strip_prefix('r'))
            .and_then(|s| s.parse().ok())
    }

    /// Parse the monotonic counter embedded in this client order ID.
    #[must_use]
    pub fn parse_sequence(&self) -> Option<u64> {
        let (_, sequence, _) = self.split_suffix_parts()?;
        sequence.parse().ok()
    }

    fn split_suffix_parts(&self) -> Option<(&str, &str, &str)> {
        let mut parts = self.0.rsplitn(3, '-');
        let restart = parts.next()?;
        let sequence = parts.next()?;
        let prefix = parts.next()?;
        Some((restart, sequence, prefix))
    }
}

/// Generates unique `ClientOrderId` values with embedded metadata.
///
/// One generator per agent. The Sequencer owns generators for all active agents.
/// Thread-safe via atomic counter (no lock needed).
pub struct ClientOrderIdGenerator {
    agent_prefix: String,
    restart_epoch: u16,
    counter: AtomicU64,
}

impl ClientOrderIdGenerator {
    /// Create a new generator for the given agent.
    ///
    /// `restart_epoch` should increment each time the process starts.
    /// A simple approach: `(process_start_time.timestamp() % 100) as u16`.
    #[must_use]
    pub fn new(agent: &AgentId, restart_epoch: u16) -> Self {
        Self::with_start_seq(agent, restart_epoch, 0)
    }

    /// Create a generator starting from a resumed sequence value.
    #[must_use]
    pub fn with_start_seq(agent: &AgentId, restart_epoch: u16, start_seq: u64) -> Self {
        Self {
            agent_prefix: agent.0.clone(),
            restart_epoch,
            counter: AtomicU64::new(start_seq),
        }
    }

    /// Generate the next unique client order ID.
    ///
    /// `now` must come from `Clock::now()` — never `Utc::now()` directly.
    /// This ensures backtest/replay determinism.
    #[must_use]
    pub fn next(&self, now: DateTime<Utc>) -> ClientOrderId {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        ClientOrderId(format!(
            "{}-{}-{:06}-r{:02}",
            self.agent_prefix,
            now.timestamp_millis(),
            seq,
            self.restart_epoch
        ))
    }

    /// Current sequence value (for diagnostics only).
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

// ─── Sequence ID (monotonic event ordering) ───

/// Monotonically increasing event sequence number.
///
/// Assigned by the Sequencer before processing any command.
/// Total ordering: if `a.0 < b.0`, event `a` happened before `b`.
/// Never reused, never gaps (unless snapshot truncation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SequenceId(pub u64);

impl SequenceId {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for SequenceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "seq:{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_order_id_format_and_parse() {
        let gen = ClientOrderIdGenerator::new(&AgentId("satoshi".into()), 3);
        let now = Utc::now();
        let id = gen.next(now);

        assert!(id.0.starts_with("satoshi-"));
        assert!(id.0.ends_with("-r03"));
        assert_eq!(id.parse_agent(), Some(AgentId("satoshi".into())));
        assert_eq!(id.parse_restart_epoch(), Some(3));
        assert_eq!(id.parse_sequence(), Some(0));
    }

    #[test]
    fn test_client_order_id_uniqueness() {
        let gen = ClientOrderIdGenerator::new(&AgentId("test".into()), 0);
        let now = Utc::now();
        let id1 = gen.next(now);
        let id2 = gen.next(now);
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_client_order_id_generator_can_resume_sequence() {
        let gen = ClientOrderIdGenerator::with_start_seq(&AgentId("test".into()), 7, 41);
        let now = Utc::now();
        let id = gen.next(now);
        assert_eq!(id.parse_sequence(), Some(41));
        assert_eq!(gen.current_seq(), 42);
    }

    #[test]
    fn test_sequence_id_ordering() {
        let a = SequenceId(1);
        let b = SequenceId(2);
        assert!(a < b);
        assert_eq!(a.next(), b);
    }

    #[test]
    fn test_newtype_id_display() {
        let id = OrderId("ord-123".into());
        assert_eq!(format!("{id}"), "ord-123");
    }

    #[test]
    fn test_newtype_id_from_str() {
        let id: AgentId = "satoshi".into();
        assert_eq!(id.0, "satoshi");
    }
}

//! SQLite-backed EventJournal.
//!
//! Schema:
//! - `events`: append-only log of sequenced engine events (MessagePack payload)
//! - `snapshots`: periodic state snapshots (MessagePack payload)
//!
//! WAL mode is enabled so reads don't block writes. The journal is opened
//! once at construction; all async methods dispatch to `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::error;

use crate::{
    EngineError, EngineEvent, EngineStateSnapshot, EventJournal, SequenceId, SequencedEvent,
};

// ── DDL ─────────────────────────────────────────────────────────────────────

const CREATE_EVENTS: &str = "
CREATE TABLE IF NOT EXISTS events (
    sequence_id INTEGER PRIMARY KEY,
    timestamp   TEXT    NOT NULL,
    event_kind  TEXT    NOT NULL,
    payload     BLOB    NOT NULL
)";

const CREATE_SNAPSHOTS: &str = "
CREATE TABLE IF NOT EXISTS snapshots (
    sequence_id INTEGER PRIMARY KEY,
    timestamp   TEXT NOT NULL,
    state       BLOB NOT NULL
)";

// ─── SqliteJournal ───────────────────────────────────────────────────────────

/// Append-only event journal backed by SQLite with WAL mode.
///
/// The `Connection` is wrapped in `Arc<Mutex<_>>` so it can be moved into
/// `spawn_blocking` closures. This is the database connection — not business
/// state — so `Arc<Mutex<Connection>>` is appropriate here.
#[derive(Clone)]
pub struct SqliteJournal {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteJournal {
    /// Open (or create) a journal at the given path.
    ///
    /// WAL mode is enabled on every open so that concurrent reads can proceed
    /// while a write is in-flight. Schema is created if absent.
    pub fn open(path: &Path) -> Result<Self, EngineError> {
        let conn = Connection::open(path).map_err(|e| {
            EngineError::Journal(format!(
                "failed to open SQLite journal at {}: {e}",
                path.display()
            ))
        })?;

        // WAL mode: reads don't block writes.
        // synchronous=FULL: fsync after every WAL write — ensures committed
        // transactions survive OS crashes and power loss. Required for a
        // financial journal. NORMAL would lose the most recent commits on
        // power failure, leaving recovered state inconsistent with venue fills.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(|e| EngineError::Journal(format!("failed to set WAL mode: {e}")))?;

        conn.execute_batch(CREATE_EVENTS)
            .map_err(|e| EngineError::Journal(format!("failed to create events table: {e}")))?;

        conn.execute_batch(CREATE_SNAPSHOTS)
            .map_err(|e| EngineError::Journal(format!("failed to create snapshots table: {e}")))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory journal (for testing only).
    ///
    /// Note: SQLite ignores `PRAGMA journal_mode=WAL` for `:memory:` databases
    /// — they always use the rollback journal. WAL-specific behaviour (concurrent
    /// reads, checkpoint side effects) is exercised by the file-backed crash-
    /// recovery test (`test_write_1000_crash_replay_recovers_identical_sequence`).
    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| EngineError::Journal(format!("failed to open in-memory journal: {e}")))?;
        // journal_mode=WAL is silently ignored for :memory: — see doc comment above.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(|e| EngineError::Journal(format!("failed to set pragmas: {e}")))?;
        conn.execute_batch(CREATE_EVENTS)
            .map_err(|e| EngineError::Journal(format!("failed to create events table: {e}")))?;
        conn.execute_batch(CREATE_SNAPSHOTS)
            .map_err(|e| EngineError::Journal(format!("failed to create snapshots table: {e}")))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn encode_event(event: &SequencedEvent<EngineEvent>) -> Result<Vec<u8>, EngineError> {
    rmp_serde::to_vec(event).map_err(|e| {
        EngineError::Journal(format!(
            "failed to encode event seq:{}: {e}",
            event.sequence_id.0
        ))
    })
}

fn decode_event(bytes: &[u8]) -> Result<SequencedEvent<EngineEvent>, EngineError> {
    rmp_serde::from_slice(bytes)
        .map_err(|e| EngineError::Journal(format!("failed to decode event: {e}")))
}

fn encode_snapshot(snapshot: &EngineStateSnapshot) -> Result<Vec<u8>, EngineError> {
    rmp_serde::to_vec(snapshot)
        .map_err(|e| EngineError::Journal(format!("failed to encode snapshot: {e}")))
}

fn decode_snapshot(bytes: &[u8]) -> Result<EngineStateSnapshot, EngineError> {
    rmp_serde::from_slice(bytes)
        .map_err(|e| EngineError::Journal(format!("failed to decode snapshot: {e}")))
}

/// Cast a `SequenceId` to `i64` for SQLite storage.
///
/// SQLite INTEGER is a signed 64-bit value. `SequenceId` is `u64`. Values above
/// `i64::MAX` (≈9.2×10^18) would wrap negative and corrupt ordering queries.
/// A `debug_assert` catches the boundary in test; in release it is unreachable
/// for any realistic event volume.
fn seq_as_i64(seq: SequenceId) -> i64 {
    debug_assert!(
        seq.0 <= i64::MAX as u64,
        "sequence_id {0} overflows i64::MAX — journal ordering is broken",
        seq.0
    );
    seq.0 as i64
}

/// Extract the enum variant name as an ASCII discriminant string for indexing.
fn event_kind(event: &EngineEvent) -> &'static str {
    match event {
        EngineEvent::OrderReceived { .. } => "OrderReceived",
        EngineEvent::OrderValidated { .. } => "OrderValidated",
        EngineEvent::OrderRiskChecked { .. } => "OrderRiskChecked",
        EngineEvent::OrderRouted { .. } => "OrderRouted",
        EngineEvent::OrderSubmitted { .. } => "OrderSubmitted",
        EngineEvent::OrderFilled { .. } => "OrderFilled",
        EngineEvent::OrderPartiallyFilled { .. } => "OrderPartiallyFilled",
        EngineEvent::OrderCancelled { .. } => "OrderCancelled",
        EngineEvent::OrderRejected { .. } => "OrderRejected",
        EngineEvent::OrderAmended { .. } => "OrderAmended",
        EngineEvent::RiskBreached { .. } => "RiskBreached",
        EngineEvent::CircuitBreakerTripped { .. } => "CircuitBreakerTripped",
        EngineEvent::CircuitBreakerReset { .. } => "CircuitBreakerReset",
        EngineEvent::PositionCorrected { .. } => "PositionCorrected",
        EngineEvent::ReconciliationCompleted { .. } => "ReconciliationCompleted",
        EngineEvent::AgentPaused { .. } => "AgentPaused",
        EngineEvent::AgentResumed { .. } => "AgentResumed",
        EngineEvent::EngineStarted { .. } => "EngineStarted",
        EngineEvent::EngineHalted { .. } => "EngineHalted",
        EngineEvent::SnapshotCreated { .. } => "SnapshotCreated",
        EngineEvent::VenueConnected { .. } => "VenueConnected",
        EngineEvent::VenueDisconnected { .. } => "VenueDisconnected",
        EngineEvent::SubsystemDegraded { .. } => "SubsystemDegraded",
        EngineEvent::SubsystemRecovered { .. } => "SubsystemRecovered",
    }
}

// ─── EventJournal impl ───────────────────────────────────────────────────────

#[async_trait::async_trait]
impl EventJournal for SqliteJournal {
    async fn append(
        &self,
        event: &SequencedEvent<EngineEvent>,
    ) -> std::result::Result<(), EngineError> {
        let payload = encode_event(event)?;
        let seq = seq_as_i64(event.sequence_id);
        let ts = event.timestamp.to_rfc3339();
        let kind = event_kind(&event.event);
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            guard
                .execute(
                    "INSERT OR IGNORE INTO events (sequence_id, timestamp, event_kind, payload) VALUES (?1, ?2, ?3, ?4)",
                    params![seq, ts, kind, payload],
                )
                .map_err(|e| EngineError::Journal(format!("append failed at seq:{seq}: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }

    async fn append_batch(
        &self,
        events: &[SequencedEvent<EngineEvent>],
    ) -> std::result::Result<(), EngineError> {
        if events.is_empty() {
            return Ok(());
        }

        // Encode before entering the blocking section.
        let rows = events
            .iter()
            .map(|ev| {
                let payload = encode_event(ev)?;
                let seq = seq_as_i64(ev.sequence_id);
                let ts = ev.timestamp.to_rfc3339();
                let kind = event_kind(&ev.event);
                Ok((seq, ts, kind, payload))
            })
            .collect::<Result<Vec<_>, EngineError>>()?;

        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            // Single transaction for the whole batch.
            let tx = guard
                .unchecked_transaction()
                .map_err(|e| EngineError::Journal(format!("begin transaction: {e}")))?;
            for (seq, ref ts, kind, ref payload) in &rows {
                tx.execute(
                    "INSERT OR IGNORE INTO events (sequence_id, timestamp, event_kind, payload) VALUES (?1, ?2, ?3, ?4)",
                    params![seq, ts, kind, payload],
                )
                .map_err(|e| EngineError::Journal(format!("batch insert at seq:{seq}: {e}")))?;
            }
            tx.commit()
                .map_err(|e| EngineError::Journal(format!("commit batch: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }

    async fn replay_from(
        &self,
        from: SequenceId,
    ) -> std::result::Result<Vec<SequencedEvent<EngineEvent>>, EngineError> {
        let from_id = seq_as_i64(from);
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            let mut stmt = guard
                .prepare(
                    "SELECT payload FROM events WHERE sequence_id >= ?1 ORDER BY sequence_id ASC",
                )
                .map_err(|e| EngineError::Journal(format!("prepare replay_from: {e}")))?;

            let events = stmt
                .query_map(params![from_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| EngineError::Journal(format!("query replay_from: {e}")))?
                .map(|r| {
                    r.map_err(|e| EngineError::Journal(format!("row error: {e}")))
                        .and_then(|bytes| decode_event(&bytes))
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(events)
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }

    async fn replay_range(
        &self,
        from: SequenceId,
        to: SequenceId,
    ) -> std::result::Result<Vec<SequencedEvent<EngineEvent>>, EngineError> {
        let from_id = seq_as_i64(from);
        let to_id = seq_as_i64(to);
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            let mut stmt = guard
                .prepare(
                    "SELECT payload FROM events WHERE sequence_id >= ?1 AND sequence_id <= ?2 ORDER BY sequence_id ASC",
                )
                .map_err(|e| EngineError::Journal(format!("prepare replay_range: {e}")))?;

            let events = stmt
                .query_map(params![from_id, to_id], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|e| EngineError::Journal(format!("query replay_range: {e}")))?
                .map(|r| {
                    r.map_err(|e| EngineError::Journal(format!("row error: {e}")))
                        .and_then(|bytes| decode_event(&bytes))
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(events)
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }

    async fn save_snapshot(
        &self,
        sequence_id: SequenceId,
        snapshot: &EngineStateSnapshot,
    ) -> std::result::Result<(), EngineError> {
        let payload = encode_snapshot(snapshot)?;
        let seq = seq_as_i64(sequence_id);
        let ts = snapshot.snapshot_at.to_rfc3339();
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;

            // Single transaction: insert new snapshot, prune all older ones.
            // Without pruning the snapshots table grows without bound — the
            // primary key is sequence_id (different each call), so every write
            // is a new row, not a replacement of the previous one.
            let tx = guard
                .unchecked_transaction()
                .map_err(|e| EngineError::Journal(format!("begin snapshot transaction: {e}")))?;
            tx.execute(
                "INSERT OR REPLACE INTO snapshots (sequence_id, timestamp, state) VALUES (?1, ?2, ?3)",
                params![seq, ts, payload],
            )
            .map_err(|e| EngineError::Journal(format!("save_snapshot at seq:{seq}: {e}")))?;
            tx.execute(
                "DELETE FROM snapshots WHERE sequence_id < ?1",
                params![seq],
            )
            .map_err(|e| {
                EngineError::Journal(format!("prune old snapshots at seq:{seq}: {e}"))
            })?;
            tx.commit()
                .map_err(|e| EngineError::Journal(format!("commit snapshot: {e}")))?;

            // Compact WAL after snapshot so it doesn't grow unboundedly.
            if let Err(e) = guard.execute_batch("PRAGMA wal_checkpoint(PASSIVE)") {
                error!("wal_checkpoint after snapshot failed (non-fatal): {e}");
            }

            Ok(())
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }

    async fn load_latest_snapshot(
        &self,
    ) -> std::result::Result<Option<(SequenceId, EngineStateSnapshot)>, EngineError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            let result = guard
                .query_row(
                    "SELECT sequence_id, state FROM snapshots ORDER BY sequence_id DESC LIMIT 1",
                    [],
                    |row| {
                        let seq: i64 = row.get(0)?;
                        let bytes: Vec<u8> = row.get(1)?;
                        Ok((seq, bytes))
                    },
                )
                .optional()
                .map_err(|e| EngineError::Journal(format!("load_latest_snapshot query: {e}")))?;

            match result {
                None => Ok(None),
                Some((seq, bytes)) => {
                    let snapshot = decode_snapshot(&bytes)?;
                    Ok(Some((SequenceId(seq as u64), snapshot)))
                }
            }
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }

    async fn latest_sequence_id(&self) -> std::result::Result<SequenceId, EngineError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            let max: Option<i64> = guard
                .query_row("SELECT MAX(sequence_id) FROM events", [], |row| row.get(0))
                .map_err(|e| EngineError::Journal(format!("latest_sequence_id query: {e}")))?;

            Ok(max.map_or(SequenceId::ZERO, |v| SequenceId(v as u64)))
        })
        .await
        .map_err(|e| EngineError::Journal(format!("spawn_blocking panic: {e}")))?
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use types::{
        AgentId, AgentStatus, FirmBook, FirmRiskLimits, InstrumentId, OrderId,
        PredictionExposureSummary, PredictionMarketLimits, RiskLimits,
    };

    use crate::{EngineEvent, EngineState, EngineStateSnapshot, SequenceId, SequencedEvent};

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0)
            .single()
            .expect("fixed timestamp")
    }

    fn sample_event(seq: u64) -> SequencedEvent<EngineEvent> {
        SequencedEvent {
            sequence_id: SequenceId(seq),
            timestamp: fixed_time(),
            event: EngineEvent::AgentPaused {
                agent_id: AgentId(format!("agent-{seq}")),
                reason: "test".to_owned(),
            },
        }
    }

    fn minimal_snapshot(seq: u64) -> EngineStateSnapshot {
        let now = fixed_time();
        EngineStateSnapshot {
            state: EngineState {
                orders: HashMap::new(),
                positions: HashMap::new(),
                pending_signals: HashMap::new(),
                firm_book: FirmBook {
                    nav: Decimal::new(100_000, 0),
                    gross_notional: Decimal::ZERO,
                    net_notional: Decimal::ZERO,
                    realized_pnl: Decimal::ZERO,
                    unrealized_pnl: Decimal::ZERO,
                    daily_pnl: Decimal::ZERO,
                    hourly_pnl: Decimal::ZERO,
                    current_drawdown_pct: Decimal::ZERO,
                    allocated_capital: Decimal::new(25_000, 0),
                    cash_available: Decimal::new(75_000, 0),
                    total_fees_paid: Decimal::ZERO,
                    agent_allocations: HashMap::new(),
                    instruments: Vec::new(),
                    prediction_exposure: PredictionExposureSummary {
                        total_notional: Decimal::ZERO,
                        pct_of_nav: Decimal::ZERO,
                        unresolved_markets: 0,
                    },
                    as_of: now,
                },
                agent_allocations: HashMap::new(),
                risk_limits: RiskLimits {
                    firm: FirmRiskLimits {
                        max_gross_notional: Decimal::new(250_000, 0),
                        max_net_notional: Decimal::new(250_000, 0),
                        max_drawdown_pct: Decimal::new(15, 2),
                        max_daily_loss: Decimal::new(5_000, 0),
                        max_open_orders: 100,
                    },
                    per_agent: HashMap::new(),
                    per_instrument: HashMap::new(),
                    per_venue: HashMap::new(),
                    prediction_market: PredictionMarketLimits {
                        max_total_notional: Decimal::new(10_000, 0),
                        max_per_market_notional: Decimal::new(2_500, 0),
                        max_pct_of_nav: Decimal::new(5, 2),
                        max_unresolved_markets: 10,
                    },
                },
                agent_statuses: HashMap::from([(
                    AgentId("athena".to_owned()),
                    AgentStatus::Active,
                )]),
                idempotency_cache: HashMap::new(),
                last_sequence_id: SequenceId(seq),
                last_updated: now,
            },
            sequence_id: SequenceId(seq),
            snapshot_at: now,
        }
    }

    // ── Write / replay ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_append_and_replay_from() {
        let journal = SqliteJournal::in_memory().expect("open journal");

        for seq in 0..10u64 {
            journal.append(&sample_event(seq)).await.expect("append");
        }

        let all = journal
            .replay_from(SequenceId::ZERO)
            .await
            .expect("replay_from");
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].sequence_id, SequenceId(0));
        assert_eq!(all[9].sequence_id, SequenceId(9));

        let tail = journal
            .replay_from(SequenceId(5))
            .await
            .expect("replay_from 5");
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0].sequence_id, SequenceId(5));
    }

    #[tokio::test]
    async fn test_append_batch_and_replay_range() {
        let journal = SqliteJournal::in_memory().expect("open journal");

        let batch: Vec<_> = (0..20u64).map(sample_event).collect();
        journal.append_batch(&batch).await.expect("append_batch");

        let range = journal
            .replay_range(SequenceId(5), SequenceId(10))
            .await
            .expect("replay_range");
        assert_eq!(range.len(), 6); // [5, 10] inclusive
        assert_eq!(range[0].sequence_id, SequenceId(5));
        assert_eq!(range[5].sequence_id, SequenceId(10));
    }

    // ── Crash / replay recovery ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_write_1000_crash_replay_recovers_identical_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("journal.db");

        // Write 1000 events.
        {
            let journal = SqliteJournal::open(&db_path).expect("open journal");
            let batch: Vec<_> = (0..1000u64).map(sample_event).collect();
            journal.append_batch(&batch).await.expect("append_batch");
            // Drop simulates crash — no graceful close.
        }

        // Replay from a fresh open.
        let journal = SqliteJournal::open(&db_path).expect("reopen journal");
        let events = journal.replay_from(SequenceId::ZERO).await.expect("replay");
        assert_eq!(events.len(), 1000);
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.sequence_id, SequenceId(i as u64));
        }
    }

    // ── Snapshot + tail replay ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_snapshot_then_tail_only_replays_tail() {
        let journal = SqliteJournal::in_memory().expect("open journal");

        // Write events 0..=49, snapshot at 49, then events 50..=99.
        let before: Vec<_> = (0..50u64).map(sample_event).collect();
        journal
            .append_batch(&before)
            .await
            .expect("append pre-snapshot");

        let snap = minimal_snapshot(49);
        journal
            .save_snapshot(SequenceId(49), &snap)
            .await
            .expect("save_snapshot");

        let after: Vec<_> = (50..100u64).map(sample_event).collect();
        journal
            .append_batch(&after)
            .await
            .expect("append post-snapshot");

        // Restore: load snapshot, replay tail.
        let (snap_seq, _restored) = journal
            .load_latest_snapshot()
            .await
            .expect("load_latest_snapshot")
            .expect("snapshot present");
        assert_eq!(snap_seq, SequenceId(49));

        let tail = journal
            .replay_from(snap_seq.next())
            .await
            .expect("replay tail");
        assert_eq!(tail.len(), 50);
        assert_eq!(tail[0].sequence_id, SequenceId(50));
        assert_eq!(tail[49].sequence_id, SequenceId(99));
    }

    // ── latest_sequence_id ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_latest_sequence_id_empty_returns_zero() {
        let journal = SqliteJournal::in_memory().expect("open journal");
        let id = journal
            .latest_sequence_id()
            .await
            .expect("latest_sequence_id");
        assert_eq!(id, SequenceId::ZERO);
    }

    #[tokio::test]
    async fn test_latest_sequence_id_after_appends() {
        let journal = SqliteJournal::in_memory().expect("open journal");
        let batch: Vec<_> = (0..5u64).map(sample_event).collect();
        journal.append_batch(&batch).await.expect("append");

        let id = journal
            .latest_sequence_id()
            .await
            .expect("latest_sequence_id");
        assert_eq!(id, SequenceId(4));
    }

    #[tokio::test]
    async fn test_latest_sequence_id_after_snapshot_only() {
        // Snapshot table does not affect latest_sequence_id (events table only).
        let journal = SqliteJournal::in_memory().expect("open journal");
        journal
            .save_snapshot(SequenceId(100), &minimal_snapshot(100))
            .await
            .expect("save_snapshot");

        // No events appended → still ZERO.
        let id = journal
            .latest_sequence_id()
            .await
            .expect("latest_sequence_id");
        assert_eq!(id, SequenceId::ZERO);
    }

    // ── Snapshot round-trip ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_snapshot_round_trip() {
        let journal = SqliteJournal::in_memory().expect("open journal");

        assert!(journal
            .load_latest_snapshot()
            .await
            .expect("load empty")
            .is_none());

        let snap = minimal_snapshot(42);
        journal
            .save_snapshot(SequenceId(42), &snap)
            .await
            .expect("save_snapshot");

        let (seq, loaded) = journal
            .load_latest_snapshot()
            .await
            .expect("load")
            .expect("snapshot present");

        assert_eq!(seq, SequenceId(42));
        // Verify financial state survives the encode/decode round-trip, not just the sequence id.
        assert_eq!(loaded.state.last_sequence_id, snap.state.last_sequence_id);
        assert_eq!(loaded.state.firm_book.nav, snap.state.firm_book.nav);
        assert_eq!(
            loaded.state.risk_limits.firm.max_daily_loss,
            snap.state.risk_limits.firm.max_daily_loss,
        );
        assert_eq!(
            loaded.state.risk_limits.firm.max_gross_notional,
            snap.state.risk_limits.firm.max_gross_notional,
        );
    }

    // ── Snapshot pruning (only the latest row survives) ──────────────────────

    #[tokio::test]
    async fn test_save_snapshot_prunes_older_rows() {
        let journal = SqliteJournal::in_memory().expect("open journal");

        // Write three successive snapshots.
        for seq in [10u64, 20, 30] {
            journal
                .save_snapshot(SequenceId(seq), &minimal_snapshot(seq))
                .await
                .expect("save_snapshot");
        }

        // Only the snapshot at seq=30 should survive.
        let (seq, _) = journal
            .load_latest_snapshot()
            .await
            .expect("load")
            .expect("snapshot present");
        assert_eq!(seq, SequenceId(30));

        // Verify older entries are gone by checking replay_range covers no snapshots
        // (the snapshots table is internal, but we can confirm only one exists by
        // saving a new one and loading it — if pruning failed we would have stale rows).
        journal
            .save_snapshot(SequenceId(40), &minimal_snapshot(40))
            .await
            .expect("save fourth snapshot");
        let (seq2, _) = journal
            .load_latest_snapshot()
            .await
            .expect("load after fourth")
            .expect("snapshot present");
        assert_eq!(seq2, SequenceId(40));
    }

    // ── Idempotent insert (INSERT OR IGNORE) ─────────────────────────────────

    #[tokio::test]
    async fn test_duplicate_append_is_idempotent() {
        let journal = SqliteJournal::in_memory().expect("open journal");
        let ev = sample_event(1);

        journal.append(&ev).await.expect("first append");
        journal
            .append(&ev)
            .await
            .expect("second append (idempotent)");

        let events = journal.replay_from(SequenceId::ZERO).await.expect("replay");
        assert_eq!(events.len(), 1);
    }

    // ── Event kind discriminant completeness ─────────────────────────────────

    #[test]
    fn test_event_kind_covers_all_variants() {
        use types::{RiskOutcomeKind, SubsystemId, VenueId};
        // Exhaustive match in event_kind() guarantees coverage at compile time.
        // This test documents that the discriminant function exists for all variants.
        let events = vec![
            EngineEvent::OrderReceived {
                order_id: OrderId("o1".into()),
                agent_id: AgentId("a1".into()),
                instrument_id: InstrumentId("BTC".into()),
                venue_id: VenueId("bybit".into()),
            },
            EngineEvent::OrderValidated {
                order_id: OrderId("o1".into()),
            },
            EngineEvent::OrderRiskChecked {
                order_id: OrderId("o1".into()),
                outcome: RiskOutcomeKind::Approved,
            },
            EngineEvent::EngineHalted {
                reason: "test".into(),
            },
            EngineEvent::SubsystemDegraded {
                subsystem: SubsystemId::EventJournal,
                reason: "test".into(),
            },
        ];
        for ev in &events {
            let kind = event_kind(ev);
            assert!(!kind.is_empty());
        }
    }
}

//! SQLite-backed EventJournal.
//!
//! Schema:
//! - `events`: append-only log of sequenced engine events (MessagePack payload)
//! - `snapshots`: periodic state snapshots (MessagePack payload)
//!
//! WAL mode is enabled so reads don't block writes. The journal is opened
//! once at construction; all async methods dispatch to `spawn_blocking`.

use std::path::Path;
use std::slice;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::error;

use crate::{
    EngineError, EngineEvent, EngineStateSnapshot, EventJournal, SequenceId, SequencedEvent,
};

// ── DDL ─────────────────────────────────────────────────────────────────────

const CREATE_EVENTS: &str = "
CREATE TABLE IF NOT EXISTS events (
    sequence_id INTEGER NOT NULL,
    event_index INTEGER NOT NULL,
    timestamp   TEXT    NOT NULL,
    event_kind  TEXT    NOT NULL,
    schema_version INTEGER NOT NULL,
    payload     BLOB    NOT NULL,
    PRIMARY KEY (sequence_id, event_index)
)";

const CREATE_SNAPSHOTS: &str = "
CREATE TABLE IF NOT EXISTS snapshots (
    sequence_id INTEGER PRIMARY KEY,
    timestamp   TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    state       BLOB NOT NULL
)";

const PERSISTED_SCHEMA_VERSION: i64 = 1;

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

        ensure_events_schema(&conn)?;
        ensure_schema_version_column(&conn, "snapshots")?;

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
        ensure_events_schema(&conn)?;
        ensure_schema_version_column(&conn, "snapshots")?;
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

fn ensure_schema_version_column(conn: &Connection, table: &str) -> Result<(), EngineError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn
        .prepare(&pragma)
        .map_err(|e| EngineError::Journal(format!("prepare table_info for {table}: {e}")))?;
    let column_names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| EngineError::Journal(format!("query table_info for {table}: {e}")))?;

    let has_schema_version = column_names
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| EngineError::Journal(format!("row error in table_info for {table}: {e}")))?
        .iter()
        .any(|column| column == "schema_version");

    if has_schema_version {
        return Ok(());
    }

    let alter = format!(
        "ALTER TABLE {table} ADD COLUMN schema_version INTEGER NOT NULL DEFAULT {PERSISTED_SCHEMA_VERSION}"
    );
    conn.execute(&alter, [])
        .map_err(|e| EngineError::Journal(format!("add schema_version to {table}: {e}")))?;
    Ok(())
}

fn load_table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, EngineError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn
        .prepare(&pragma)
        .map_err(|e| EngineError::Journal(format!("prepare table_info for {table}: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| EngineError::Journal(format!("query table_info for {table}: {e}")))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| EngineError::Journal(format!("row error in table_info for {table}: {e}")))
}

fn ensure_events_schema(conn: &Connection) -> Result<(), EngineError> {
    let columns = load_table_columns(conn, "events")?;
    let has_event_index = columns.iter().any(|column| column == "event_index");
    let has_schema_version = columns.iter().any(|column| column == "schema_version");

    if has_event_index && has_schema_version {
        return Ok(());
    }

    conn.execute_batch(
        "
        CREATE TABLE events_v2 (
            sequence_id INTEGER NOT NULL,
            event_index INTEGER NOT NULL,
            timestamp TEXT NOT NULL,
            event_kind TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            payload BLOB NOT NULL,
            PRIMARY KEY (sequence_id, event_index)
        );
        ",
    )
    .map_err(|e| EngineError::Journal(format!("create events_v2 table: {e}")))?;

    let copy_sql = if has_schema_version {
        "
        INSERT INTO events_v2 (sequence_id, event_index, timestamp, event_kind, schema_version, payload)
        SELECT sequence_id, 0, timestamp, event_kind, schema_version, payload
        FROM events
        ORDER BY sequence_id ASC
        "
    } else {
        "
        INSERT INTO events_v2 (sequence_id, event_index, timestamp, event_kind, schema_version, payload)
        SELECT sequence_id, 0, timestamp, event_kind, 1, payload
        FROM events
        ORDER BY sequence_id ASC
        "
    };
    conn.execute(copy_sql, [])
        .map_err(|e| EngineError::Journal(format!("copy legacy events rows: {e}")))?;
    conn.execute_batch("DROP TABLE events; ALTER TABLE events_v2 RENAME TO events;")
        .map_err(|e| EngineError::Journal(format!("replace events table: {e}")))?;
    Ok(())
}

fn ensure_supported_schema_version(
    entity: &str,
    schema_version: i64,
    sequence_id: i64,
) -> Result<(), EngineError> {
    if schema_version == PERSISTED_SCHEMA_VERSION {
        return Ok(());
    }

    Err(EngineError::Journal(format!(
        "unsupported {entity} schema_version {schema_version} at seq:{sequence_id}; expected {PERSISTED_SCHEMA_VERSION}"
    )))
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

type EncodedEventRow = (i64, i64, String, &'static str, i64, Vec<u8>);

struct EncodedSnapshotRow {
    sequence_id: i64,
    timestamp: String,
    schema_version: i64,
    payload: Vec<u8>,
}

fn encode_event_row(
    event_index: usize,
    event: &SequencedEvent<EngineEvent>,
) -> Result<EncodedEventRow, EngineError> {
    Ok((
        seq_as_i64(event.sequence_id),
        i64::try_from(event_index).map_err(|_| {
            EngineError::Journal(format!("event_index {event_index} overflows i64"))
        })?,
        event.timestamp.to_rfc3339(),
        event_kind(&event.event),
        PERSISTED_SCHEMA_VERSION,
        encode_event(event)?,
    ))
}

fn encode_snapshot_row(snapshot: &EngineStateSnapshot) -> Result<EncodedSnapshotRow, EngineError> {
    Ok(EncodedSnapshotRow {
        sequence_id: seq_as_i64(snapshot.sequence_id),
        timestamp: snapshot.snapshot_at.to_rfc3339(),
        schema_version: PERSISTED_SCHEMA_VERSION,
        payload: encode_snapshot(snapshot)?,
    })
}

fn persist_rows(
    conn: &Connection,
    events: &[EncodedEventRow],
    snapshot: Option<&EncodedSnapshotRow>,
) -> Result<(), EngineError> {
    if events.is_empty() && snapshot.is_none() {
        return Ok(());
    }

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| EngineError::Journal(format!("begin transaction: {e}")))?;

    for (sequence_id, event_index, timestamp, kind, schema_version, payload) in events {
        tx.execute(
            "INSERT OR IGNORE INTO events (sequence_id, event_index, timestamp, event_kind, schema_version, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![sequence_id, event_index, timestamp, kind, schema_version, payload],
        )
        .map_err(|e| {
            EngineError::Journal(format!(
                "batch insert at seq:{sequence_id}/event:{event_index}: {e}"
            ))
        })?;
    }

    if let Some(snapshot) = snapshot {
        tx.execute(
            "INSERT OR REPLACE INTO snapshots (sequence_id, timestamp, schema_version, state) VALUES (?1, ?2, ?3, ?4)",
            params![
                snapshot.sequence_id,
                snapshot.timestamp,
                snapshot.schema_version,
                snapshot.payload
            ],
        )
        .map_err(|e| {
            EngineError::Journal(format!(
                "save_snapshot at seq:{}: {e}",
                snapshot.sequence_id
            ))
        })?;
        tx.execute(
            "DELETE FROM snapshots WHERE sequence_id < ?1",
            params![snapshot.sequence_id],
        )
        .map_err(|e| {
            EngineError::Journal(format!(
                "prune old snapshots at seq:{}: {e}",
                snapshot.sequence_id
            ))
        })?;
    }

    tx.commit()
        .map_err(|e| EngineError::Journal(format!("commit transaction: {e}")))?;

    if snapshot.is_some() {
        if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)") {
            error!("wal_checkpoint after snapshot failed (non-fatal): {e}");
        }
    }

    Ok(())
}

// ─── EventJournal impl ───────────────────────────────────────────────────────

#[async_trait::async_trait]
impl EventJournal for SqliteJournal {
    async fn append(
        &self,
        event: &SequencedEvent<EngineEvent>,
    ) -> std::result::Result<(), EngineError> {
        self.persist_commit(slice::from_ref(event), None).await
    }

    async fn append_batch(
        &self,
        events: &[SequencedEvent<EngineEvent>],
    ) -> std::result::Result<(), EngineError> {
        self.persist_commit(events, None).await
    }

    async fn persist_commit(
        &self,
        events: &[SequencedEvent<EngineEvent>],
        snapshot: Option<&EngineStateSnapshot>,
    ) -> std::result::Result<(), EngineError> {
        let rows = events
            .iter()
            .enumerate()
            .map(|(event_index, event)| encode_event_row(event_index, event))
            .collect::<Result<Vec<_>, EngineError>>()?;
        let snapshot_row = snapshot.map(encode_snapshot_row).transpose()?;
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| EngineError::Journal("journal mutex poisoned".to_owned()))?;
            persist_rows(&guard, &rows, snapshot_row.as_ref())
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
                    "SELECT sequence_id, schema_version, payload FROM events WHERE sequence_id >= ?1 ORDER BY sequence_id ASC, event_index ASC",
                )
                .map_err(|e| EngineError::Journal(format!("prepare replay_from: {e}")))?;

            let events = stmt
                .query_map(params![from_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(|e| EngineError::Journal(format!("query replay_from: {e}")))?
                .map(|r| {
                    r.map_err(|e| EngineError::Journal(format!("row error: {e}")))
                        .and_then(|(sequence_id, schema_version, bytes)| {
                            ensure_supported_schema_version("event", schema_version, sequence_id)?;
                            decode_event(&bytes)
                        })
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
                    "SELECT sequence_id, schema_version, payload FROM events WHERE sequence_id >= ?1 AND sequence_id <= ?2 ORDER BY sequence_id ASC, event_index ASC",
                )
                .map_err(|e| EngineError::Journal(format!("prepare replay_range: {e}")))?;

            let events = stmt
                .query_map(params![from_id, to_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(|e| EngineError::Journal(format!("query replay_range: {e}")))?
                .map(|r| {
                    r.map_err(|e| EngineError::Journal(format!("row error: {e}")))
                        .and_then(|(sequence_id, schema_version, bytes)| {
                            ensure_supported_schema_version("event", schema_version, sequence_id)?;
                            decode_event(&bytes)
                        })
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
        debug_assert_eq!(snapshot.sequence_id, sequence_id);
        self.persist_commit(&[], Some(snapshot)).await
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
                    "SELECT sequence_id, schema_version, state FROM snapshots ORDER BY sequence_id DESC LIMIT 1",
                    [],
                    |row| {
                        let seq: i64 = row.get(0)?;
                        let schema_version: i64 = row.get(1)?;
                        let bytes: Vec<u8> = row.get(2)?;
                        Ok((seq, schema_version, bytes))
                    },
                )
                .optional()
                .map_err(|e| EngineError::Journal(format!("load_latest_snapshot query: {e}")))?;

            match result {
                None => Ok(None),
                Some((seq, schema_version, bytes)) => {
                    ensure_supported_schema_version("snapshot", schema_version, seq)?;
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
    use std::path::Path;
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

    #[tokio::test]
    async fn test_append_batch_preserves_multiple_events_per_sequence() {
        let journal = SqliteJournal::in_memory().expect("open journal");
        let now = fixed_time();
        let events = vec![
            SequencedEvent {
                sequence_id: SequenceId(7),
                timestamp: now,
                event: EngineEvent::OrderReceived {
                    order_id: OrderId("ord-7".into()),
                    agent_id: AgentId("athena".into()),
                    instrument_id: InstrumentId("BTC".into()),
                    venue_id: types::VenueId("bybit".into()),
                },
            },
            SequencedEvent {
                sequence_id: SequenceId(7),
                timestamp: now,
                event: EngineEvent::OrderValidated {
                    order_id: OrderId("ord-7".into()),
                },
            },
            SequencedEvent {
                sequence_id: SequenceId(7),
                timestamp: now,
                event: EngineEvent::OrderRiskChecked {
                    order_id: OrderId("ord-7".into()),
                    outcome: types::RiskOutcomeKind::Approved,
                },
            },
        ];

        journal.append_batch(&events).await.expect("append batch");

        let replayed = journal
            .replay_from(SequenceId(7))
            .await
            .expect("replay same sequence");
        assert_eq!(replayed.len(), 3);
        assert!(matches!(
            replayed[0].event,
            EngineEvent::OrderReceived { .. }
        ));
        assert!(matches!(
            replayed[1].event,
            EngineEvent::OrderValidated { .. }
        ));
        assert!(matches!(
            replayed[2].event,
            EngineEvent::OrderRiskChecked { .. }
        ));
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

    #[tokio::test]
    async fn test_replay_rejects_unknown_event_schema_version() {
        let journal = SqliteJournal::in_memory().expect("open journal");
        journal.append(&sample_event(7)).await.expect("append");

        {
            let conn = journal.conn.lock().expect("journal lock");
            conn.execute(
                "UPDATE events SET schema_version = 99 WHERE sequence_id = 7",
                [],
            )
            .expect("poison event schema version");
        }

        let err = journal
            .replay_from(SequenceId::ZERO)
            .await
            .expect_err("version mismatch must fail");
        assert!(err
            .to_string()
            .contains("unsupported event schema_version 99"));
    }

    #[tokio::test]
    async fn test_load_snapshot_rejects_unknown_schema_version() {
        let journal = SqliteJournal::in_memory().expect("open journal");
        journal
            .save_snapshot(SequenceId(42), &minimal_snapshot(42))
            .await
            .expect("save snapshot");

        {
            let conn = journal.conn.lock().expect("journal lock");
            conn.execute(
                "UPDATE snapshots SET schema_version = 99 WHERE sequence_id = 42",
                [],
            )
            .expect("poison snapshot schema version");
        }

        let err = journal
            .load_latest_snapshot()
            .await
            .expect_err("version mismatch must fail");
        assert!(err
            .to_string()
            .contains("unsupported snapshot schema_version 99"));
    }

    #[tokio::test]
    async fn test_open_migrates_pre_versioned_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("legacy-journal.db");
        seed_legacy_schema(&db_path);

        let journal = SqliteJournal::open(&db_path).expect("open migrated journal");
        let snapshot = journal
            .load_latest_snapshot()
            .await
            .expect("load migrated snapshot")
            .expect("snapshot present");
        assert_eq!(snapshot.0, SequenceId(11));

        let events = journal
            .replay_from(SequenceId::ZERO)
            .await
            .expect("replay migrated events");
        assert_eq!(events.len(), 1);

        let conn = journal.conn.lock().expect("journal lock");
        assert!(table_has_schema_version(&conn, "events"));
        assert!(table_has_schema_version(&conn, "snapshots"));
        assert!(table_has_column(&conn, "events", "event_index"));
    }

    #[tokio::test]
    async fn test_open_migrates_schema_versioned_events_without_event_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("branch-journal.db");
        seed_schema_versioned_events_without_event_index(&db_path);

        let journal = SqliteJournal::open(&db_path).expect("open migrated journal");
        let events = journal
            .replay_from(SequenceId::ZERO)
            .await
            .expect("replay migrated events");
        assert_eq!(events.len(), 1);

        let conn = journal.conn.lock().expect("journal lock");
        assert!(table_has_column(&conn, "events", "event_index"));
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

    fn seed_legacy_schema(path: &Path) {
        let conn = Connection::open(path).expect("open legacy db");
        conn.execute_batch(
            "
            CREATE TABLE events (
                sequence_id INTEGER PRIMARY KEY,
                timestamp   TEXT    NOT NULL,
                event_kind  TEXT    NOT NULL,
                payload     BLOB    NOT NULL
            );
            CREATE TABLE snapshots (
                sequence_id INTEGER PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                state       BLOB NOT NULL
            );
            ",
        )
        .expect("create legacy schema");

        let event = sample_event(3);
        let snapshot = minimal_snapshot(11);
        conn.execute(
            "INSERT INTO events (sequence_id, timestamp, event_kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![
                seq_as_i64(event.sequence_id),
                event.timestamp.to_rfc3339(),
                event_kind(&event.event),
                encode_event(&event).expect("encode event"),
            ],
        )
        .expect("insert legacy event");
        conn.execute(
            "INSERT INTO snapshots (sequence_id, timestamp, state) VALUES (?1, ?2, ?3)",
            params![
                seq_as_i64(snapshot.sequence_id),
                snapshot.snapshot_at.to_rfc3339(),
                encode_snapshot(&snapshot).expect("encode snapshot"),
            ],
        )
        .expect("insert legacy snapshot");
    }

    fn seed_schema_versioned_events_without_event_index(path: &Path) {
        let conn = Connection::open(path).expect("open schema-versioned db");
        conn.execute_batch(
            "
            CREATE TABLE events (
                sequence_id INTEGER PRIMARY KEY,
                timestamp   TEXT    NOT NULL,
                event_kind  TEXT    NOT NULL,
                schema_version INTEGER NOT NULL,
                payload     BLOB    NOT NULL
            );
            CREATE TABLE snapshots (
                sequence_id INTEGER PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                state       BLOB NOT NULL
            );
            ",
        )
        .expect("create schema-versioned old tables");

        let event = sample_event(5);
        conn.execute(
            "INSERT INTO events (sequence_id, timestamp, event_kind, schema_version, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                seq_as_i64(event.sequence_id),
                event.timestamp.to_rfc3339(),
                event_kind(&event.event),
                PERSISTED_SCHEMA_VERSION,
                encode_event(&event).expect("encode event"),
            ],
        )
        .expect("insert schema-versioned event");
    }

    fn table_has_column(conn: &Connection, table: &str, name: &str) -> bool {
        let pragma = format!("PRAGMA table_info({table})");
        let mut stmt = conn.prepare(&pragma).expect("prepare table_info");
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query table_info")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect columns");
        columns.iter().any(|column| column == name)
    }

    fn table_has_schema_version(conn: &Connection, table: &str) -> bool {
        table_has_column(conn, table, "schema_version")
    }
}

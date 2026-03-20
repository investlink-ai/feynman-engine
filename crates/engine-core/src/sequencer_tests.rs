use crate::{
    AgentId, AgentRiskLimits, AgentStatus, EngineCore, EngineEvent, EngineState,
    EngineStateSnapshot, EventJournal, Fill, FirmBook, OrderCore, OrderRecord, OrderState,
    PriceSource, ReconciliationAction, ReconciliationReport, Result, RiskApproval,
    RiskApprovalStamp, RiskCheckResult, RiskLimits, RiskViolation, SequenceGenerator, SequenceId,
    Sequencer, SequencerCommand, TrackedPosition,
};
use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use types::{
    ClientOrderId, ExecHint, InstrumentId, OrderId, OrderType, PredictionExposureSummary, Side,
    Signal, SignalId, TimeInForce, Urgency, VenueId, VenueOrderId,
};

#[derive(Clone, Default)]
struct RecordingJournal {
    state: Arc<Mutex<JournalState>>,
}

#[derive(Default)]
struct JournalState {
    events: Vec<crate::SequencedEvent<EngineEvent>>,
    snapshot: Option<(SequenceId, EngineStateSnapshot)>,
}

impl RecordingJournal {
    fn events(&self) -> Vec<crate::SequencedEvent<EngineEvent>> {
        self.state.lock().expect("journal lock").events.clone()
    }
}

#[derive(Clone, Copy)]
enum JournalFailureMode {
    PersistCommit,
}

#[derive(Clone)]
struct FailingJournal {
    inner: RecordingJournal,
    failure_mode: JournalFailureMode,
}

#[async_trait::async_trait]
impl EventJournal for FailingJournal {
    async fn append(
        &self,
        event: &crate::SequencedEvent<EngineEvent>,
    ) -> std::result::Result<(), crate::EngineError> {
        self.inner.append(event).await
    }

    async fn append_batch(
        &self,
        events: &[crate::SequencedEvent<EngineEvent>],
    ) -> std::result::Result<(), crate::EngineError> {
        self.inner.append_batch(events).await
    }

    async fn persist_commit(
        &self,
        events: &[crate::SequencedEvent<EngineEvent>],
        snapshot: Option<&EngineStateSnapshot>,
    ) -> std::result::Result<(), crate::EngineError> {
        if matches!(self.failure_mode, JournalFailureMode::PersistCommit) {
            return Err(crate::EngineError::Journal(
                "simulated persist_commit failure".to_owned(),
            ));
        }
        self.inner.persist_commit(events, snapshot).await
    }

    async fn replay_from(
        &self,
        from: SequenceId,
    ) -> std::result::Result<Vec<crate::SequencedEvent<EngineEvent>>, crate::EngineError> {
        self.inner.replay_from(from).await
    }

    async fn replay_range(
        &self,
        from: SequenceId,
        to: SequenceId,
    ) -> std::result::Result<Vec<crate::SequencedEvent<EngineEvent>>, crate::EngineError> {
        self.inner.replay_range(from, to).await
    }

    async fn save_snapshot(
        &self,
        sequence_id: SequenceId,
        snapshot: &EngineStateSnapshot,
    ) -> std::result::Result<(), crate::EngineError> {
        self.inner.save_snapshot(sequence_id, snapshot).await
    }

    async fn load_latest_snapshot(
        &self,
    ) -> std::result::Result<Option<(SequenceId, EngineStateSnapshot)>, crate::EngineError> {
        self.inner.load_latest_snapshot().await
    }

    async fn latest_sequence_id(&self) -> std::result::Result<SequenceId, crate::EngineError> {
        self.inner.latest_sequence_id().await
    }
}

#[async_trait::async_trait]
impl EventJournal for RecordingJournal {
    async fn append(
        &self,
        event: &crate::SequencedEvent<EngineEvent>,
    ) -> std::result::Result<(), crate::EngineError> {
        self.state
            .lock()
            .expect("journal lock")
            .events
            .push(event.clone());
        Ok(())
    }

    async fn append_batch(
        &self,
        events: &[crate::SequencedEvent<EngineEvent>],
    ) -> std::result::Result<(), crate::EngineError> {
        self.state
            .lock()
            .expect("journal lock")
            .events
            .extend_from_slice(events);
        Ok(())
    }

    async fn persist_commit(
        &self,
        events: &[crate::SequencedEvent<EngineEvent>],
        snapshot: Option<&EngineStateSnapshot>,
    ) -> std::result::Result<(), crate::EngineError> {
        let mut state = self.state.lock().expect("journal lock");
        state.events.extend_from_slice(events);
        if let Some(snapshot) = snapshot {
            state.snapshot = Some((snapshot.sequence_id, snapshot.clone()));
        }
        Ok(())
    }

    async fn replay_from(
        &self,
        from: SequenceId,
    ) -> std::result::Result<Vec<crate::SequencedEvent<EngineEvent>>, crate::EngineError> {
        Ok(self
            .state
            .lock()
            .expect("journal lock")
            .events
            .iter()
            .filter(|event| event.sequence_id >= from)
            .cloned()
            .collect())
    }

    async fn replay_range(
        &self,
        from: SequenceId,
        to: SequenceId,
    ) -> std::result::Result<Vec<crate::SequencedEvent<EngineEvent>>, crate::EngineError> {
        Ok(self
            .state
            .lock()
            .expect("journal lock")
            .events
            .iter()
            .filter(|event| event.sequence_id >= from && event.sequence_id <= to)
            .cloned()
            .collect())
    }

    async fn save_snapshot(
        &self,
        sequence_id: SequenceId,
        snapshot: &EngineStateSnapshot,
    ) -> std::result::Result<(), crate::EngineError> {
        self.state.lock().expect("journal lock").snapshot = Some((sequence_id, snapshot.clone()));
        Ok(())
    }

    async fn load_latest_snapshot(
        &self,
    ) -> std::result::Result<Option<(SequenceId, EngineStateSnapshot)>, crate::EngineError> {
        Ok(self.state.lock().expect("journal lock").snapshot.clone())
    }

    async fn latest_sequence_id(&self) -> std::result::Result<SequenceId, crate::EngineError> {
        Ok(self
            .state
            .lock()
            .expect("journal lock")
            .events
            .last()
            .map(|event| event.sequence_id)
            .unwrap_or(SequenceId::ZERO))
    }
}

#[derive(Clone, Copy)]
struct StaticPriceSource;

impl PriceSource for StaticPriceSource {
    fn latest_price(&self, _: &InstrumentId) -> Option<types::PriceSnapshot> {
        None
    }

    fn is_stale(&self, _: &InstrumentId, _: std::time::Duration) -> bool {
        false
    }
}

struct TestCore {
    state: EngineState,
    reject_orders: bool,
}

impl TestCore {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            state: empty_state(now),
            reject_orders: false,
        }
    }

    fn with_order(mut self, record: OrderRecord) -> Self {
        self.state.orders.insert(record.core.id.clone(), record);
        self
    }

    fn rejecting_orders(mut self) -> Self {
        self.reject_orders = true;
        self
    }
}

impl EngineCore for TestCore {
    fn state(&self) -> &EngineState {
        &self.state
    }

    fn state_mut(&mut self) -> &mut EngineState {
        &mut self.state
    }

    fn evaluate_order(
        &mut self,
        order: crate::PipelineOrder<crate::Validated>,
        now: DateTime<Utc>,
    ) -> std::result::Result<types::PipelineOrder<types::RiskChecked>, crate::EngineError> {
        if self.reject_orders {
            return Err(crate::EngineError::RiskRejected {
                reason: "test rejection".to_owned(),
            });
        }
        let approval = RiskApprovalStamp {
            approved_at: now,
            checks_performed: Vec::new(),
            warnings: Vec::new(),
        };
        Ok(order.into_risk_checked(approval))
    }

    fn on_fill(&mut self, fill: &Fill, now: DateTime<Utc>) -> Result<()> {
        let record = self
            .state
            .orders
            .get_mut(&fill.order_id)
            .ok_or_else(|| crate::EngineError::OrderNotFound(fill.order_id.to_string()))?;

        if fill.qty <= Decimal::ZERO {
            return Err(crate::EngineError::InvalidTransition(
                "fill quantity must be positive".to_owned(),
            ));
        }

        record.fills.push(fill.clone());
        record.last_updated = now;

        let filled_qty = record
            .fills
            .iter()
            .map(|seen_fill| seen_fill.qty)
            .fold(Decimal::ZERO, |acc, qty| acc + qty);

        if filled_qty > record.core.qty {
            return Err(crate::EngineError::InvalidTransition(format!(
                "order {} overfilled",
                record.core.id
            )));
        }

        record.state = if filled_qty == record.core.qty {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };

        Ok(())
    }

    fn on_reconciliation(&mut self, report: &ReconciliationReport, _: DateTime<Utc>) -> Result<()> {
        for divergence in &report.position_divergences {
            if divergence.action == ReconciliationAction::AcceptVenue {
                self.state.positions.insert(
                    (
                        AgentId("reconciler".to_owned()),
                        report.venue_id.clone(),
                        divergence.instrument.clone(),
                    ),
                    TrackedPosition {
                        agent: AgentId("reconciler".to_owned()),
                        venue: report.venue_id.clone(),
                        account: report.account_id.clone(),
                        instrument: divergence.instrument.clone(),
                        qty: divergence.venue_qty,
                        avg_entry_price: Decimal::ZERO,
                        unrealized_pnl: Decimal::ZERO,
                        realized_pnl: Decimal::ZERO,
                        total_fees_paid: Decimal::ZERO,
                        accumulated_funding: Decimal::ZERO,
                        fill_ids: Vec::new(),
                        signal_ids: Vec::new(),
                        opened_at: report.started_at,
                        last_fill_at: report.completed_at,
                    },
                );
            }
        }

        Ok(())
    }

    fn mark_to_market(&mut self, _: &dyn PriceSource, _: DateTime<Utc>) -> Result<()> {
        Ok(())
    }

    fn pause_agent(&mut self, agent: &AgentId, reason: String, now: DateTime<Utc>) -> Result<()> {
        self.state
            .agent_statuses
            .insert(agent.clone(), AgentStatus::Paused { reason, since: now });
        Ok(())
    }

    fn resume_agent(&mut self, agent: &AgentId, _: DateTime<Utc>) -> Result<()> {
        self.state
            .agent_statuses
            .insert(agent.clone(), AgentStatus::Active);
        Ok(())
    }

    fn adjust_limits(&mut self, agent: &AgentId, limits: AgentRiskLimits) -> Result<()> {
        self.state
            .risk_limits
            .per_agent
            .insert(agent.clone(), limits);
        Ok(())
    }

    fn halt_all(&mut self, _: String, _: DateTime<Utc>) -> Result<()> {
        for status in self.state.agent_statuses.values_mut() {
            *status = AgentStatus::Halted;
        }
        Ok(())
    }

    fn reset_circuit_breaker(&mut self, _: DateTime<Utc>) -> Result<()> {
        Ok(())
    }
}

#[test]
fn test_sequence_generator() {
    let mut gen = SequenceGenerator::from_zero();
    assert_eq!(gen.next_id(), SequenceId(0));
    assert_eq!(gen.next_id(), SequenceId(1));
    assert_eq!(gen.next_id(), SequenceId(2));
    assert_eq!(gen.current(), SequenceId(3));
}

#[test]
fn test_sequence_generator_resume() {
    let mut gen = SequenceGenerator::new(SequenceId(1000));
    assert_eq!(gen.next_id(), SequenceId(1000));
    assert_eq!(gen.next_id(), SequenceId(1001));
}

#[test]
fn test_risk_approval_into_stamp() {
    let approval = RiskApproval {
        approved_at: Utc::now(),
        checks_performed: vec![
            RiskCheckResult {
                check_name: "CB-all".into(),
                passed: true,
                message: None,
            },
            RiskCheckResult {
                check_name: "RG-notional".into(),
                passed: true,
                message: None,
            },
        ],
        warnings: vec![RiskViolation {
            check_name: "near_limit".into(),
            violation_type: types::ViolationType::Soft,
            current_value: "0.89".into(),
            limit: "0.90".into(),
            suggested_action: "monitor".into(),
        }],
    };
    let stamp = approval.into_stamp();
    assert_eq!(stamp.checks_performed.len(), 2);
    assert_eq!(stamp.warnings.len(), 1);
}

#[tokio::test]
async fn test_priority_commands_preempt_normal_queue() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let agent = AgentId("taleb".to_owned());
    handle
        .send(SequencerCommand::PauseAgent {
            agent: agent.clone(),
            reason: "manual pause".to_owned(),
        })
        .await
        .expect("queue normal command");

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::HaltAll {
            reason: "breaker".to_owned(),
        })
        .await
        .expect("queue high-priority halt");
    handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let final_snapshot = tokio::spawn(sequencer.run())
        .await
        .expect("sequencer join")
        .expect("sequencer result");

    shutdown_rx.await.expect("shutdown ack");

    let event_tags = journal
        .events()
        .iter()
        .map(|event| event_tag(&event.event))
        .collect::<Vec<_>>();
    assert_eq!(event_tags[0], "engine_halted");
    assert_eq!(event_tags[1], "agent_paused");
    assert!(matches!(
        final_snapshot.state.agent_statuses.get(&agent),
        Some(AgentStatus::Paused { .. })
    ));
}

#[tokio::test]
async fn test_submit_signal_returns_explicitly_unsupported() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let run_task = tokio::spawn(sequencer.run());

    let (signal_tx, signal_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::SubmitSignal {
            signal: sample_signal(now),
            respond: signal_tx,
        })
        .await
        .expect("queue signal");

    let signal_result = signal_rx.await.expect("signal response");
    assert!(matches!(
        signal_result,
        Err(crate::EngineError::UnsupportedCommand {
            command: "SubmitSignal",
        })
    ));

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let final_snapshot = run_task
        .await
        .expect("sequencer join")
        .expect("sequencer result");
    shutdown_rx.await.expect("shutdown ack");

    assert_eq!(final_snapshot.sequence_id, SequenceId::ZERO);
    assert!(journal.events().is_empty());
}

#[tokio::test]
async fn test_on_fill_updates_order_state_and_journals_fill() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let order = sample_order("ord-fill", now);
    let validated = crate::PipelineOrder::new(order.clone()).into_validated(now);
    let order_record = OrderRecord {
        core: validated.core.clone(),
        state: OrderState::Accepted,
        client_order_id: ClientOrderId("athena-1-000000-r07".to_owned()),
        fills: Vec::new(),
        created_at: now,
        last_updated: now,
    };
    let core = TestCore::new(now).with_order(order_record);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let fill = Fill {
        order_id: OrderId("ord-fill".to_owned()),
        venue_order_id: VenueOrderId("venue-1".to_owned()),
        instrument_id: InstrumentId("BTCUSDT".to_owned()),
        side: Side::Buy,
        qty: Decimal::new(4, 0),
        price: Decimal::new(50_100, 0),
        fee: Decimal::new(2, 0),
        filled_at: now,
        is_maker: true,
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::OnFill { fill })
        .await
        .expect("queue fill");
    handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let final_snapshot = tokio::spawn(sequencer.run())
        .await
        .expect("sequencer join")
        .expect("sequencer result");

    shutdown_rx.await.expect("shutdown ack");

    let updated = final_snapshot
        .state
        .orders
        .get(&OrderId("ord-fill".to_owned()))
        .expect("updated order");
    assert_eq!(updated.state, OrderState::PartiallyFilled);
    assert_eq!(updated.fills.len(), 1);

    let event_tags = journal
        .events()
        .iter()
        .map(|event| event_tag(&event.event))
        .collect::<Vec<_>>();
    assert_eq!(event_tags, vec!["order_partially_filled"]);
}

#[tokio::test]
async fn test_shutdown_drains_pending_commands_before_exit() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let agent = AgentId("satoshi".to_owned());
    handle
        .send(SequencerCommand::PauseAgent {
            agent: agent.clone(),
            reason: "operator".to_owned(),
        })
        .await
        .expect("queue pause");

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let final_snapshot = tokio::spawn(sequencer.run())
        .await
        .expect("sequencer join")
        .expect("sequencer result");

    shutdown_rx.await.expect("shutdown ack");

    assert!(matches!(
        final_snapshot.state.agent_statuses.get(&agent),
        Some(AgentStatus::Paused { .. })
    ));
    assert_eq!(
        journal
            .events()
            .iter()
            .map(|event| event_tag(&event.event))
            .collect::<Vec<_>>(),
        vec!["agent_paused"]
    );
}

#[tokio::test]
async fn test_caught_panic_stops_sequencer() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    handle
        .send(SequencerCommand::PoisonPill)
        .await
        .expect("queue poison pill");

    let run_result = tokio::spawn(sequencer.run()).await.expect("sequencer join");

    assert!(matches!(run_result, Err(crate::EngineError::Internal(_))));
    assert!(journal.events().is_empty());
}

#[tokio::test]
async fn test_command_processing_is_deterministic() {
    let first = run_deterministic_sequence().await;
    let second = run_deterministic_sequence().await;

    assert_eq!(first.final_order_state, second.final_order_state);
    assert_eq!(first.final_client_order_id, second.final_client_order_id);
    assert_eq!(first.event_tags, second.event_tags);
    assert_eq!(first.sequence_id, second.sequence_id);
    assert_eq!(first.fill_count, second.fill_count);
}

#[tokio::test]
async fn test_journal_append_failure_stops_sequencer() {
    let now = fixed_time();
    let journal = FailingJournal {
        inner: RecordingJournal::default(),
        failure_mode: JournalFailureMode::PersistCommit,
    };
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal, StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let order =
        crate::PipelineOrder::new(sample_order("ord-journal-fail", now)).into_validated(now);
    let (ack_tx, _ack_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::SubmitOrder {
            order,
            respond: ack_tx,
        })
        .await
        .expect("queue order");

    let run_result = tokio::spawn(sequencer.run()).await.expect("sequencer join");
    assert!(matches!(run_result, Err(crate::EngineError::Journal(_))));
}

#[tokio::test]
async fn test_snapshot_persistence_failure_stops_sequencer() {
    let now = fixed_time();
    let journal = FailingJournal {
        inner: RecordingJournal::default(),
        failure_mode: JournalFailureMode::PersistCommit,
    };
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal, StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    handle
        .send(SequencerCommand::PauseAgent {
            agent: AgentId("athena".to_owned()),
            reason: "snapshot fail".to_owned(),
        })
        .await
        .expect("queue pause");

    let run_result = tokio::spawn(sequencer.run()).await.expect("sequencer join");
    assert!(matches!(run_result, Err(crate::EngineError::Journal(_))));
}

#[tokio::test]
async fn test_bootstrap_restores_snapshot_and_avoids_sequence_reuse_after_rejection() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let core = TestCore::new(now).rejecting_orders();
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let order = crate::PipelineOrder::new(sample_order("ord-reject", now)).into_validated(now);
    let (ack_tx, ack_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::SubmitOrder {
            order,
            respond: ack_tx,
        })
        .await
        .expect("queue rejected order");

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let first_snapshot = tokio::spawn(sequencer.run())
        .await
        .expect("sequencer join")
        .expect("sequencer result");
    shutdown_rx.await.expect("shutdown ack");
    assert!(ack_rx.await.expect("reply channel").is_err());
    assert_eq!(first_snapshot.sequence_id, SequenceId(2));

    let fresh_core = TestCore::new(now);
    let fresh_clock = types::SimulatedClock::new(now);
    let (restored_sequencer, restored_handle) = Sequencer::new(
        fresh_core,
        journal.clone(),
        StaticPriceSource,
        fresh_clock,
        7,
    )
    .await
    .expect("restore sequencer from snapshot");

    let (snapshot_tx, snapshot_rx) = oneshot::channel();
    restored_handle
        .send(SequencerCommand::Snapshot {
            respond: snapshot_tx,
        })
        .await
        .expect("queue snapshot");
    restored_handle
        .send(SequencerCommand::PauseAgent {
            agent: AgentId("athena".to_owned()),
            reason: "post-restore".to_owned(),
        })
        .await
        .expect("queue pause");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    restored_handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let restored_final_snapshot = tokio::spawn(restored_sequencer.run())
        .await
        .expect("sequencer join")
        .expect("sequencer result");
    let restored_snapshot = snapshot_rx.await.expect("snapshot response");
    shutdown_rx.await.expect("shutdown ack");

    assert_eq!(restored_snapshot.sequence_id, SequenceId(2));
    assert_eq!(restored_final_snapshot.sequence_id, SequenceId(5));
    assert_eq!(
        journal
            .events()
            .iter()
            .map(|event| event.sequence_id.0)
            .collect::<Vec<_>>(),
        vec![2, 5]
    );
}

#[tokio::test]
async fn test_bootstrap_rejects_journal_tail_without_replay_support() {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    {
        let mut state = journal.state.lock().expect("journal lock");
        state.events.push(crate::SequencedEvent {
            sequence_id: SequenceId(1),
            timestamp: now,
            event: EngineEvent::OrderRejected {
                order_id: OrderId("ord-tail".to_owned()),
                stage: "risk".to_owned(),
                reason: "tail".to_owned(),
            },
        });
    }

    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let bootstrap_result = Sequencer::new(core, journal, StaticPriceSource, clock, 7).await;
    assert!(matches!(
        bootstrap_result,
        Err(crate::EngineError::Journal(_))
    ));
}

struct ReplayOutcome {
    final_order_state: OrderState,
    final_client_order_id: ClientOrderId,
    event_tags: Vec<String>,
    sequence_id: SequenceId,
    fill_count: usize,
}

async fn run_deterministic_sequence() -> ReplayOutcome {
    let now = fixed_time();
    let journal = RecordingJournal::default();
    let core = TestCore::new(now);
    let clock = types::SimulatedClock::new(now);
    let (sequencer, handle) = Sequencer::new(core, journal.clone(), StaticPriceSource, clock, 7)
        .await
        .expect("bootstrap sequencer");

    let run_task = tokio::spawn(sequencer.run());

    let order = crate::PipelineOrder::new(sample_order("ord-seq", now)).into_validated(now);
    let (ack_tx, ack_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::SubmitOrder {
            order,
            respond: ack_tx,
        })
        .await
        .expect("queue order");
    let ack = ack_rx
        .await
        .expect("order ack channel")
        .expect("order accepted");

    let fill = Fill {
        order_id: OrderId("ord-seq".to_owned()),
        venue_order_id: VenueOrderId("venue-seq".to_owned()),
        instrument_id: InstrumentId("BTCUSDT".to_owned()),
        side: Side::Buy,
        qty: Decimal::new(3, 0),
        price: Decimal::new(50_050, 0),
        fee: Decimal::new(1, 0),
        filled_at: now,
        is_maker: false,
    };
    handle
        .send(SequencerCommand::OnFill { fill })
        .await
        .expect("queue fill");
    handle
        .send(SequencerCommand::PauseAgent {
            agent: AgentId("athena".to_owned()),
            reason: "risk".to_owned(),
        })
        .await
        .expect("queue pause");

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    handle
        .send(SequencerCommand::Shutdown {
            respond: shutdown_tx,
        })
        .await
        .expect("queue shutdown");

    let final_snapshot = run_task
        .await
        .expect("sequencer join")
        .expect("sequencer result");
    shutdown_rx.await.expect("shutdown ack");

    let final_order = final_snapshot
        .state
        .orders
        .get(&OrderId("ord-seq".to_owned()))
        .expect("final order");

    ReplayOutcome {
        final_order_state: final_order.state,
        final_client_order_id: ack.client_order_id,
        event_tags: journal
            .events()
            .iter()
            .map(|event| event_tag(&event.event))
            .collect(),
        sequence_id: final_snapshot.sequence_id,
        fill_count: final_order.fills.len(),
    }
}

fn fixed_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0)
        .single()
        .expect("fixed timestamp")
}

fn empty_state(now: DateTime<Utc>) -> EngineState {
    EngineState {
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
            firm: types::FirmRiskLimits {
                max_gross_notional: Decimal::new(250_000, 0),
                max_net_notional: Decimal::new(250_000, 0),
                max_drawdown_pct: Decimal::new(15, 2),
                max_daily_loss: Decimal::new(5_000, 0),
                max_open_orders: 100,
            },
            per_agent: HashMap::new(),
            per_instrument: HashMap::new(),
            per_venue: HashMap::new(),
            prediction_market: types::PredictionMarketLimits {
                max_total_notional: Decimal::new(10_000, 0),
                max_per_market_notional: Decimal::new(2_500, 0),
                max_pct_of_nav: Decimal::new(5, 2),
                max_unresolved_markets: 10,
            },
        },
        agent_statuses: HashMap::from([(AgentId("athena".to_owned()), AgentStatus::Active)]),
        idempotency_cache: HashMap::new(),
        last_sequence_id: SequenceId::ZERO,
        last_updated: now,
    }
}

fn sample_order(order_id: &str, now: DateTime<Utc>) -> OrderCore {
    OrderCore {
        id: OrderId(order_id.to_owned()),
        agent_id: AgentId("athena".to_owned()),
        instrument_id: InstrumentId("BTCUSDT".to_owned()),
        venue_id: VenueId("bybit".to_owned()),
        side: Side::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::GTC,
        qty: Decimal::new(10, 0),
        price: Some(Decimal::new(50_000, 0)),
        trigger_price: None,
        stop_loss: Some(Decimal::new(49_000, 0)),
        take_profit: Some(Decimal::new(52_000, 0)),
        post_only: false,
        reduce_only: false,
        dry_run: true,
        exec_hint: ExecHint::default(),
        created_at: now,
    }
}

fn sample_signal(now: DateTime<Utc>) -> Signal {
    Signal {
        id: SignalId("sig-unsupported".to_owned()),
        agent: AgentId("athena".to_owned()),
        instrument: InstrumentId("BTCUSDT".to_owned()),
        direction: Side::Buy,
        conviction: Decimal::new(75, 2),
        sizing_hint: None,
        arb_type: "momentum".to_owned(),
        stop_loss: Decimal::new(49_000, 0),
        take_profit: Some(Decimal::new(52_000, 0)),
        thesis: "unsupported path".to_owned(),
        urgency: Urgency::Normal,
        metadata: serde_json::json!({}),
        created_at: now,
    }
}

fn event_tag(event: &EngineEvent) -> String {
    match event {
        EngineEvent::OrderReceived { .. } => "order_received",
        EngineEvent::OrderValidated { .. } => "order_validated",
        EngineEvent::OrderRiskChecked { .. } => "order_risk_checked",
        EngineEvent::OrderRouted { .. } => "order_routed",
        EngineEvent::OrderSubmitted { .. } => "order_submitted",
        EngineEvent::OrderFilled { .. } => "order_filled",
        EngineEvent::OrderPartiallyFilled { .. } => "order_partially_filled",
        EngineEvent::OrderCancelled { .. } => "order_cancelled",
        EngineEvent::OrderRejected { .. } => "order_rejected",
        EngineEvent::OrderAmended { .. } => "order_amended",
        EngineEvent::RiskBreached { .. } => "risk_breached",
        EngineEvent::CircuitBreakerTripped { .. } => "circuit_breaker_tripped",
        EngineEvent::CircuitBreakerReset { .. } => "circuit_breaker_reset",
        EngineEvent::PositionCorrected { .. } => "position_corrected",
        EngineEvent::ReconciliationCompleted { .. } => "reconciliation_completed",
        EngineEvent::AgentPaused { .. } => "agent_paused",
        EngineEvent::AgentResumed { .. } => "agent_resumed",
        EngineEvent::EngineStarted { .. } => "engine_started",
        EngineEvent::EngineHalted { .. } => "engine_halted",
        EngineEvent::SnapshotCreated { .. } => "snapshot_created",
        EngineEvent::VenueConnected { .. } => "venue_connected",
        EngineEvent::VenueDisconnected { .. } => "venue_disconnected",
        EngineEvent::SubsystemDegraded { .. } => "subsystem_degraded",
        EngineEvent::SubsystemRecovered { .. } => "subsystem_recovered",
    }
    .to_owned()
}

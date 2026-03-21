use anyhow::anyhow;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use crate::{
    AgentId, ClientOrderId, ClientOrderIdGenerator, EngineCore, EngineError, EngineEvent,
    EngineStateSnapshot, EventJournal, Fill, OrderAck, OrderRecord, OrderState, PriceSource,
    ReconciliationAction, ReconciliationReport, Result, RoutingAssignment, SequenceGenerator,
    SequenceId, SequencedEvent, SequencerCommand, VenueOrderId,
};

pub const HIGH_PRIORITY_CHANNEL_CAPACITY: usize = 64;
pub const COMMAND_CHANNEL_CAPACITY: usize = 1024;
/// Capacity of the bounded channel feeding `PipelineOrder<Routed>` to the venue submit task.
pub const VENUE_SUBMIT_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct SequencerHandle {
    high_priority_tx: mpsc::Sender<SequencerCommand>,
    command_tx: mpsc::Sender<SequencerCommand>,
}

impl SequencerHandle {
    pub async fn send(
        &self,
        command: SequencerCommand,
    ) -> std::result::Result<(), mpsc::error::SendError<SequencerCommand>> {
        if command.is_high_priority() {
            self.high_priority_tx.send(command).await
        } else {
            self.command_tx.send(command).await
        }
    }

    #[must_use]
    pub fn high_priority_tx(&self) -> mpsc::Sender<SequencerCommand> {
        self.high_priority_tx.clone()
    }

    #[must_use]
    pub fn command_tx(&self) -> mpsc::Sender<SequencerCommand> {
        self.command_tx.clone()
    }
}

pub struct Sequencer<C, J, P, K> {
    core: C,
    journal: J,
    prices: P,
    clock: K,
    sequence_gen: SequenceGenerator,
    snapshot_cache: EngineStateSnapshot,
    high_priority_rx: mpsc::Receiver<SequencerCommand>,
    command_rx: mpsc::Receiver<SequencerCommand>,
    client_order_id_generators: std::collections::HashMap<AgentId, ClientOrderIdGenerator>,
    restart_epoch: u16,
    /// Send side of the bounded venue-submit channel.
    ///
    /// `None` in test / backtest contexts without a live venue adapter.
    /// When `Some`, routed orders are dispatched here for async submission by `VenueSubmitTask`.
    venue_submit_tx: Option<mpsc::Sender<crate::PipelineOrder<crate::Routed>>>,
}

impl<C, J, P, K> Sequencer<C, J, P, K>
where
    C: EngineCore,
    J: EventJournal,
    P: PriceSource,
    K: crate::Clock,
{
    pub async fn new(
        mut core: C,
        journal: J,
        prices: P,
        clock: K,
        restart_epoch: u16,
    ) -> Result<(Self, SequencerHandle)> {
        let (high_priority_tx, high_priority_rx) = mpsc::channel(HIGH_PRIORITY_CHANNEL_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);

        let latest_snapshot = journal.load_latest_snapshot().await?;
        let journal_sequence = journal.latest_sequence_id().await?;

        if let Some((_, snapshot)) = &latest_snapshot {
            core.restore_snapshot(snapshot)?;
        }

        let has_unapplied_journal_tail = match latest_snapshot.as_ref() {
            Some((snapshot_sequence, _)) => journal_sequence > *snapshot_sequence,
            None => !journal.replay_from(SequenceId::ZERO).await?.is_empty(),
        };

        if has_unapplied_journal_tail {
            return Err(EngineError::Journal(
                "journal contains unapplied events beyond the latest persisted snapshot; replay bootstrap is not implemented yet"
                    .to_owned(),
            ));
        }

        let current_sequence = latest_snapshot
            .as_ref()
            .map_or(core.state().last_sequence_id, |(sequence_id, _)| {
                *sequence_id
            });
        let snapshot_cache = latest_snapshot.map_or_else(
            || core.snapshot(current_sequence, core.state().last_updated),
            |(_, snapshot)| snapshot,
        );
        let client_order_id_generators =
            restore_client_order_generators(core.state(), restart_epoch)?;

        Ok((
            Self {
                core,
                journal,
                prices,
                clock,
                sequence_gen: SequenceGenerator::new(current_sequence.next()),
                snapshot_cache,
                high_priority_rx,
                command_rx,
                client_order_id_generators,
                restart_epoch,
                venue_submit_tx: None,
            },
            SequencerHandle {
                high_priority_tx,
                command_tx,
            },
        ))
    }

    /// Wire a venue submit channel before calling `run()`.
    ///
    /// The send side is held by the Sequencer; the receive side is passed to `VenueSubmitTask`.
    /// Routed orders are dispatched here for async venue submission.
    #[must_use]
    pub fn with_venue_submit_tx(
        mut self,
        tx: mpsc::Sender<crate::PipelineOrder<crate::Routed>>,
    ) -> Self {
        self.venue_submit_tx = Some(tx);
        self
    }

    pub async fn run(mut self) -> Result<EngineStateSnapshot> {
        loop {
            let Some(command) = self.next_command().await else {
                break;
            };

            if let Some(reply) = self.process_command(command).await? {
                self.drain_pending_commands().await?;
                let final_snapshot = self.snapshot_cache.clone();
                let _ = reply.send(());
                return Ok(final_snapshot);
            }
        }

        Ok(self.snapshot_cache.clone())
    }

    async fn next_command(&mut self) -> Option<SequencerCommand> {
        if let Some(command) = self.try_next_pending_command() {
            return Some(command);
        }

        tokio::select! {
            biased;
            Some(command) = self.high_priority_rx.recv() => Some(command),
            Some(command) = self.command_rx.recv() => Some(command),
            else => None,
        }
    }

    fn try_next_pending_command(&mut self) -> Option<SequencerCommand> {
        match self.high_priority_rx.try_recv() {
            Ok(command) => return Some(command),
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {}
        }

        self.command_rx.try_recv().ok()
    }

    async fn drain_pending_commands(&mut self) -> Result<()> {
        while let Some(command) = self.try_next_pending_command() {
            let _ = self.process_command(command).await?;
        }
        Ok(())
    }

    async fn process_command(
        &mut self,
        command: SequencerCommand,
    ) -> Result<Option<oneshot::Sender<()>>> {
        let sequence_id = self.sequence_gen.next_id();
        let now = self.clock.now();

        let outcome = catch_unwind(AssertUnwindSafe(|| self.handle_command(command, now)));
        match outcome {
            Ok(Ok(outcome)) => {
                let should_commit_metadata = outcome.state_mutated || !outcome.events.is_empty();
                let committed_snapshot =
                    should_commit_metadata.then(|| self.snapshot_for_commit(sequence_id, now));
                let events = outcome
                    .events
                    .into_iter()
                    .map(|event| SequencedEvent {
                        sequence_id,
                        timestamp: now,
                        event,
                    })
                    .collect::<Vec<_>>();

                if should_commit_metadata {
                    self.journal
                        .persist_commit(&events, committed_snapshot.as_ref())
                        .await
                        .map_err(|err| {
                            EngineError::Journal(format!(
                                "failed to persist sequencer commit at {sequence_id}: {err}"
                            ))
                        })?;
                }

                if let Some(snapshot) = committed_snapshot {
                    self.update_metadata(sequence_id, now);
                    self.snapshot_cache = snapshot;
                }

                Ok(outcome.shutdown_reply)
            }
            Ok(Err(err)) => {
                error!("sequencer command failed at {sequence_id}: {err}");
                Err(err)
            }
            Err(payload) => {
                error!("sequencer panic caught at {sequence_id}: {:?}", payload);
                Err(EngineError::Internal(anyhow!(
                    "sequencer panic at {sequence_id}: {:?}",
                    payload
                )))
            }
        }
    }

    fn handle_command(
        &mut self,
        command: SequencerCommand,
        now: DateTime<Utc>,
    ) -> Result<CommandOutcome> {
        match command {
            SequencerCommand::SubmitOrder { order, respond } => {
                self.handle_submit_order(order, respond, now)
            }
            SequencerCommand::SubmitSignal { respond, .. } => {
                let _ = respond.send(Err(EngineError::UnsupportedCommand {
                    command: "SubmitSignal",
                }));
                Ok(CommandOutcome::default())
            }
            SequencerCommand::OnVenueAck {
                order_id,
                venue_order_id,
                submitted_at,
            } => self.handle_venue_ack(order_id, venue_order_id, submitted_at),
            SequencerCommand::OnVenueSubmitFailed { order_id, reason } => {
                self.handle_venue_submit_failed(order_id, reason, now)
            }
            SequencerCommand::OnVenueCancelled {
                order_id,
                reason,
                cancelled_at,
            } => self.handle_venue_cancelled(order_id, reason, cancelled_at),
            SequencerCommand::OnFill { fill } => self.handle_fill(fill, now),
            SequencerCommand::OnReconciliation { report } => {
                self.handle_reconciliation(report, now)
            }
            SequencerCommand::MarkToMarket => {
                self.core.mark_to_market(&self.prices, now)?;
                Ok(CommandOutcome::mutated(Vec::new()))
            }
            SequencerCommand::AdjustLimits {
                agent,
                limits,
                respond,
            } => match self.core.adjust_limits(&agent, limits) {
                Ok(()) => {
                    let _ = respond.send(Ok(()));
                    Ok(CommandOutcome::mutated(Vec::new()))
                }
                Err(err) => {
                    let _ = respond.send(Err(err));
                    Ok(CommandOutcome::default())
                }
            },
            SequencerCommand::PauseAgent { agent, reason } => {
                self.core.pause_agent(&agent, reason.clone(), now)?;
                Ok(CommandOutcome::mutated(vec![EngineEvent::AgentPaused {
                    agent_id: agent,
                    reason,
                }]))
            }
            SequencerCommand::ResumeAgent { agent } => {
                self.core.resume_agent(&agent, now)?;
                Ok(CommandOutcome::mutated(vec![EngineEvent::AgentResumed {
                    agent_id: agent,
                }]))
            }
            SequencerCommand::HaltAll { reason } => {
                self.core.halt_all(reason.clone(), now)?;
                Ok(CommandOutcome::mutated(vec![EngineEvent::EngineHalted {
                    reason,
                }]))
            }
            SequencerCommand::ResetCircuitBreaker { breaker_id } => {
                self.core.reset_circuit_breaker(now)?;
                Ok(CommandOutcome::mutated(vec![
                    EngineEvent::CircuitBreakerReset { breaker_id },
                ]))
            }
            SequencerCommand::Snapshot { respond } => {
                let _ = respond.send(self.snapshot_cache.clone());
                Ok(CommandOutcome::default())
            }
            SequencerCommand::Shutdown { respond } => Ok(CommandOutcome::shutdown(respond)),
            #[cfg(test)]
            SequencerCommand::PoisonPill => panic!("poison pill"),
        }
    }

    fn handle_submit_order(
        &mut self,
        order: crate::PipelineOrder<crate::Validated>,
        respond: oneshot::Sender<Result<OrderAck>>,
        now: DateTime<Utc>,
    ) -> Result<CommandOutcome> {
        if let Some(existing_ack) = self.find_existing_ack(&order.core.id) {
            info!(order_id = %order.core.id, "sequencer returned cached order acknowledgement");
            let _ = respond.send(Ok(existing_ack));
            return Ok(CommandOutcome::default());
        }

        let order_id = order.core.id.clone();
        let agent_id = order.core.agent_id.clone();
        let instrument_id = order.core.instrument_id.clone();
        let venue_id = order.core.venue_id.clone();

        match self.core.evaluate_order(order, now) {
            Ok(risk_checked) => {
                let client_order_id = self.next_client_order_id(&agent_id, now);

                // Transition: RiskChecked → Routed.
                //
                // The venue was already selected at Draft time (by SignalOrderBridge).
                // Routing stamps the ClientOrderId and routed_at timestamp.
                // Clone core before into_routed() consumes risk_checked.
                // PipelineOrder<Routed> is !Clone (by design), so we capture the
                // OrderCore here for the OrderRecord we'll write after try_send succeeds.
                let routed_core = risk_checked.core.clone();
                let routing = RoutingAssignment {
                    venue_id: venue_id.clone(),
                    client_order_id: client_order_id.clone(),
                    routed_at: now,
                };
                let routed = risk_checked.into_routed(routing);

                // Dispatch to venue submit task BEFORE mutating state.
                // If the channel is full or closed, fail-fast: no state mutation,
                // error returned to caller. Ghost orders (state written but never
                // submitted) must never happen.
                if let Some(ref submit_tx) = self.venue_submit_tx {
                    match submit_tx.try_send(routed) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_dropped)) => {
                            let reason = "venue submit channel is full — order rejected".to_owned();
                            warn!(order_id = %order_id, "{}", reason);
                            let _ = respond
                                .send(Err(EngineError::Internal(anyhow::anyhow!("{}", reason))));
                            return Ok(CommandOutcome::events(vec![EngineEvent::OrderRejected {
                                order_id,
                                stage: "route".to_owned(),
                                reason,
                            }]));
                        }
                        Err(mpsc::error::TrySendError::Closed(_dropped)) => {
                            let reason =
                                "venue submit channel is closed — order rejected".to_owned();
                            warn!(order_id = %order_id, "{}", reason);
                            let _ = respond
                                .send(Err(EngineError::Internal(anyhow::anyhow!("{}", reason))));
                            return Ok(CommandOutcome::events(vec![EngineEvent::OrderRejected {
                                order_id,
                                stage: "route".to_owned(),
                                reason,
                            }]));
                        }
                    }
                } else {
                    warn!(
                        order_id = %order_id,
                        "no venue submit task wired — order routed but not submitted"
                    );
                }

                let ack = OrderAck {
                    order_id: order_id.clone(),
                    client_order_id: client_order_id.clone(),
                    accepted_at: now,
                };
                let record = OrderRecord {
                    core: routed_core,
                    state: OrderState::Pending,
                    client_order_id: client_order_id.clone(),
                    venue_order_id: None,
                    fills: Vec::new(),
                    created_at: now,
                    last_updated: now,
                };

                {
                    let state = self.core.state_mut();
                    state.orders.insert(order_id.clone(), record);
                    state
                        .idempotency_cache
                        .insert(client_order_id.clone(), ack.clone());
                }

                info!(
                    order_id = %order_id,
                    agent_id = %agent_id,
                    venue_id = %venue_id,
                    client_order_id = %client_order_id,
                    "sequencer accepted and routed order"
                );
                let _ = respond.send(Ok(ack));

                Ok(CommandOutcome::mutated(vec![
                    EngineEvent::OrderReceived {
                        order_id: order_id.clone(),
                        agent_id,
                        instrument_id,
                        venue_id: venue_id.clone(),
                    },
                    EngineEvent::OrderValidated {
                        order_id: order_id.clone(),
                    },
                    EngineEvent::OrderRiskChecked {
                        order_id: order_id.clone(),
                        outcome: types::RiskOutcomeKind::Approved,
                    },
                    EngineEvent::OrderRouted { order_id, venue_id },
                ]))
            }
            Err(err) => {
                let reason = err.to_string();
                warn!(
                    order_id = %order_id,
                    agent_id = %agent_id,
                    venue_id = %venue_id,
                    reason = %reason,
                    "sequencer rejected order"
                );
                let _ = respond.send(Err(err));
                Ok(CommandOutcome::events(vec![EngineEvent::OrderRejected {
                    order_id,
                    stage: "risk".to_owned(),
                    reason,
                }]))
            }
        }
    }

    fn handle_venue_ack(
        &mut self,
        order_id: crate::OrderId,
        venue_order_id: VenueOrderId,
        submitted_at: DateTime<Utc>,
    ) -> Result<CommandOutcome> {
        let record = self
            .core
            .state_mut()
            .orders
            .get_mut(&order_id)
            .ok_or_else(|| EngineError::OrderNotFound(order_id.to_string()))?;

        if record.state != OrderState::Pending {
            return Err(EngineError::InvalidTransition(format!(
                "venue ack for order {} in unexpected state {}",
                order_id, record.state
            )));
        }

        record.state = OrderState::Submitted;
        record.venue_order_id = Some(venue_order_id.clone());
        record.last_updated = submitted_at;

        info!(
            order_id = %order_id,
            venue_order_id = %venue_order_id,
            "venue acknowledged order"
        );

        Ok(CommandOutcome::mutated(vec![EngineEvent::OrderSubmitted {
            order_id,
            venue_order_id,
        }]))
    }

    fn handle_venue_submit_failed(
        &mut self,
        order_id: crate::OrderId,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<CommandOutcome> {
        let record = self
            .core
            .state_mut()
            .orders
            .get_mut(&order_id)
            .ok_or_else(|| EngineError::OrderNotFound(order_id.to_string()))?;

        if record.state != OrderState::Pending {
            return Err(EngineError::InvalidTransition(format!(
                "venue submit failed for order {} in unexpected state {}",
                order_id, record.state
            )));
        }

        record.state = OrderState::Rejected;
        record.last_updated = now;

        warn!(
            order_id = %order_id,
            reason = %reason,
            "venue submission failed"
        );

        Ok(CommandOutcome::mutated(vec![EngineEvent::OrderRejected {
            order_id,
            stage: "venue_submit".to_owned(),
            reason,
        }]))
    }

    fn handle_venue_cancelled(
        &mut self,
        order_id: crate::OrderId,
        reason: String,
        cancelled_at: DateTime<Utc>,
    ) -> Result<CommandOutcome> {
        let record = self
            .core
            .state_mut()
            .orders
            .get_mut(&order_id)
            .ok_or_else(|| EngineError::OrderNotFound(order_id.to_string()))?;

        match record.state {
            OrderState::Submitted | OrderState::PartiallyFilled => {}
            _ => {
                return Err(EngineError::InvalidTransition(format!(
                    "venue cancel for order {} in unexpected state {}",
                    order_id, record.state
                )));
            }
        }

        record.state = OrderState::Cancelled;
        record.last_updated = cancelled_at;

        Ok(CommandOutcome::mutated(vec![EngineEvent::OrderCancelled {
            order_id,
            reason,
        }]))
    }

    fn handle_fill(&mut self, fill: Fill, now: DateTime<Utc>) -> Result<CommandOutcome> {
        self.core.on_fill(&fill, now)?;

        let record = self
            .core
            .state()
            .orders
            .get(&fill.order_id)
            .ok_or_else(|| EngineError::OrderNotFound(fill.order_id.to_string()))?;

        let remaining_qty = remaining_qty(record)?;
        let event = match record.state {
            OrderState::PartiallyFilled => EngineEvent::OrderPartiallyFilled {
                order_id: fill.order_id.clone(),
                filled_qty: fill.qty,
                remaining_qty,
                price: fill.price,
                fee: fill.fee,
            },
            OrderState::Filled => EngineEvent::OrderFilled {
                order_id: fill.order_id.clone(),
                filled_qty: fill.qty,
                price: fill.price,
                fee: fill.fee,
            },
            _ => {
                return Err(EngineError::InvalidTransition(format!(
                    "fill for order {} left state {}",
                    fill.order_id, record.state
                )))
            }
        };

        Ok(CommandOutcome::mutated(vec![event]))
    }

    fn handle_reconciliation(
        &mut self,
        report: ReconciliationReport,
        now: DateTime<Utc>,
    ) -> Result<CommandOutcome> {
        self.core.on_reconciliation(&report, now)?;

        let mut events = report
            .position_divergences
            .iter()
            .filter(|divergence| divergence.action == ReconciliationAction::AcceptVenue)
            .map(|divergence| EngineEvent::PositionCorrected {
                instrument_id: divergence.instrument.clone(),
                venue_id: report.venue_id.clone(),
                old_qty: divergence.engine_qty,
                new_qty: divergence.venue_qty,
            })
            .collect::<Vec<_>>();

        let divergences_found = u32::try_from(report.divergence_count()).unwrap_or(u32::MAX);

        events.push(EngineEvent::ReconciliationCompleted {
            venue_id: report.venue_id,
            divergences_found,
        });

        Ok(CommandOutcome::mutated(events))
    }

    fn find_existing_ack(&self, order_id: &crate::OrderId) -> Option<OrderAck> {
        let state = self.core.state();
        state
            .orders
            .get(order_id)
            .and_then(|record| state.idempotency_cache.get(&record.client_order_id))
            .cloned()
    }

    fn next_client_order_id(&mut self, agent: &AgentId, now: DateTime<Utc>) -> ClientOrderId {
        self.client_order_id_generators
            .entry(agent.clone())
            .or_insert_with(|| ClientOrderIdGenerator::new(agent, self.restart_epoch))
            .next(now)
    }

    fn update_metadata(&mut self, sequence_id: SequenceId, now: DateTime<Utc>) {
        let state = self.core.state_mut();
        state.last_sequence_id = sequence_id;
        state.last_updated = now;
    }

    fn snapshot_for_commit(
        &self,
        sequence_id: SequenceId,
        now: DateTime<Utc>,
    ) -> EngineStateSnapshot {
        let mut state = self.core.state().clone();
        state.last_sequence_id = sequence_id;
        state.last_updated = now;
        EngineStateSnapshot {
            state,
            sequence_id,
            snapshot_at: now,
        }
    }
}

#[derive(Default)]
pub(crate) struct CommandOutcome {
    pub(crate) events: Vec<EngineEvent>,
    pub(crate) state_mutated: bool,
    pub(crate) shutdown_reply: Option<oneshot::Sender<()>>,
}

impl CommandOutcome {
    pub(crate) fn events(events: Vec<EngineEvent>) -> Self {
        Self {
            events,
            state_mutated: false,
            shutdown_reply: None,
        }
    }

    pub(crate) fn mutated(events: Vec<EngineEvent>) -> Self {
        Self {
            events,
            state_mutated: true,
            shutdown_reply: None,
        }
    }

    pub(crate) fn shutdown(reply: oneshot::Sender<()>) -> Self {
        Self {
            events: Vec::new(),
            state_mutated: false,
            shutdown_reply: Some(reply),
        }
    }
}

pub(crate) fn remaining_qty(record: &OrderRecord) -> Result<Decimal> {
    let filled_qty = record
        .fills
        .iter()
        .map(|fill| fill.qty)
        .fold(Decimal::ZERO, |acc, qty| acc + qty);
    let remaining = record.core.qty - filled_qty;
    if remaining < Decimal::ZERO {
        return Err(EngineError::InvalidTransition(format!(
            "order {} overfilled by {}",
            record.core.id,
            remaining.abs()
        )));
    }
    Ok(remaining)
}

fn restore_client_order_generators(
    state: &crate::EngineState,
    restart_epoch: u16,
) -> Result<HashMap<AgentId, ClientOrderIdGenerator>> {
    let mut next_sequences: HashMap<AgentId, u64> = HashMap::new();

    for ack in state.idempotency_cache.values() {
        let client_order_id = &ack.client_order_id;
        let agent = client_order_id.parse_agent().ok_or_else(|| {
            crate::EngineError::Internal(anyhow!(
                "persisted client_order_id {} is malformed: missing agent prefix",
                client_order_id
            ))
        })?;
        let next_sequence = client_order_id
            .parse_sequence()
            .and_then(|sequence| sequence.checked_add(1))
            .ok_or_else(|| {
                crate::EngineError::Internal(anyhow!(
                    "persisted client_order_id {} is malformed: missing or overflowing sequence",
                    client_order_id
                ))
            })?;

        next_sequences
            .entry(agent)
            .and_modify(|current| *current = (*current).max(next_sequence))
            .or_insert(next_sequence);
    }

    Ok(next_sequences
        .into_iter()
        .map(|(agent, next_sequence)| {
            (
                agent.clone(),
                ClientOrderIdGenerator::with_start_seq(&agent, restart_epoch, next_sequence),
            )
        })
        .collect())
}

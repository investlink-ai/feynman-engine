use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use crate::{
    AgentId, ClientOrderId, ClientOrderIdGenerator, EngineCore, EngineError, EngineEvent,
    EngineStateSnapshot, EventJournal, Fill, OrderAck, OrderRecord, OrderState, PriceSource,
    ReconciliationAction, ReconciliationReport, Result, SequenceGenerator, SequenceId,
    SequencedEvent, SequencerCommand,
};

pub const HIGH_PRIORITY_CHANNEL_CAPACITY: usize = 64;
pub const COMMAND_CHANNEL_CAPACITY: usize = 1024;

const PANIC_WINDOW_SECS: i64 = 60;
const MAX_PANICS_PER_WINDOW: usize = 3;

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
    panic_timestamps: VecDeque<DateTime<Utc>>,
}

impl<C, J, P, K> Sequencer<C, J, P, K>
where
    C: EngineCore,
    J: EventJournal,
    P: PriceSource,
    K: crate::Clock,
{
    #[must_use]
    pub fn new(
        core: C,
        journal: J,
        prices: P,
        clock: K,
        restart_epoch: u16,
    ) -> (Self, SequencerHandle) {
        let (high_priority_tx, high_priority_rx) = mpsc::channel(HIGH_PRIORITY_CHANNEL_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);

        let current_sequence = core.state().last_sequence_id;
        let snapshot_cache = core.snapshot(current_sequence, core.state().last_updated);

        (
            Self {
                core,
                journal,
                prices,
                clock,
                sequence_gen: SequenceGenerator::new(current_sequence.next()),
                snapshot_cache,
                high_priority_rx,
                command_rx,
                client_order_id_generators: std::collections::HashMap::new(),
                restart_epoch,
                panic_timestamps: VecDeque::new(),
            },
            SequencerHandle {
                high_priority_tx,
                command_tx,
            },
        )
    }

    pub async fn run(mut self) -> Result<EngineStateSnapshot> {
        loop {
            let Some(command) = self.next_command().await else {
                break;
            };

            if let Some(reply) = self.process_command(command).await {
                self.drain_pending_commands().await;
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

    async fn drain_pending_commands(&mut self) {
        while let Some(command) = self.try_next_pending_command() {
            let _ = self.process_command(command).await;
        }
    }

    async fn process_command(&mut self, command: SequencerCommand) -> Option<oneshot::Sender<()>> {
        let sequence_id = self.sequence_gen.next_id();
        let now = self.clock.now();

        let outcome = catch_unwind(AssertUnwindSafe(|| self.handle_command(command, now)));
        match outcome {
            Ok(Ok(outcome)) => {
                if outcome.state_mutated {
                    self.update_metadata(sequence_id, now);
                    self.refresh_snapshot(sequence_id, now);
                }

                if !outcome.events.is_empty() {
                    let events = outcome
                        .events
                        .into_iter()
                        .map(|event| SequencedEvent {
                            sequence_id,
                            timestamp: now,
                            event,
                        })
                        .collect::<Vec<_>>();

                    if let Err(err) = self.journal.append_batch(&events).await {
                        error!("failed to append sequencer events at {sequence_id}: {err}");
                    }
                }

                outcome.shutdown_reply
            }
            Ok(Err(err)) => {
                error!("sequencer command failed at {sequence_id}: {err}");
                None
            }
            Err(payload) => {
                error!("sequencer panic caught at {sequence_id}: {:?}", payload);
                self.record_panic(now);

                if self.panic_timestamps.len() > MAX_PANICS_PER_WINDOW {
                    let halt_reason = "repeated sequencer panics".to_owned();
                    match self.core.halt_all(halt_reason.clone(), now) {
                        Ok(()) => {
                            self.update_metadata(sequence_id, now);
                            self.refresh_snapshot(sequence_id, now);

                            let event = SequencedEvent {
                                sequence_id,
                                timestamp: now,
                                event: EngineEvent::EngineHalted {
                                    reason: halt_reason,
                                },
                            };
                            if let Err(err) = self.journal.append(&event).await {
                                error!(
                                    "failed to append panic-triggered halt event at {sequence_id}: {err}"
                                );
                            }
                        }
                        Err(err) => error!("failed to halt after repeated panics: {err}"),
                    }
                }

                None
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
                let ack = OrderAck {
                    order_id: order_id.clone(),
                    client_order_id: client_order_id.clone(),
                    accepted_at: now,
                };
                let record = OrderRecord {
                    core: risk_checked.core.clone(),
                    state: OrderState::Pending,
                    client_order_id: client_order_id.clone(),
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
                    "sequencer accepted order"
                );
                let _ = respond.send(Ok(ack));

                Ok(CommandOutcome::mutated(vec![
                    EngineEvent::OrderReceived {
                        order_id: order_id.clone(),
                        agent_id,
                        instrument_id,
                        venue_id,
                    },
                    EngineEvent::OrderValidated {
                        order_id: order_id.clone(),
                    },
                    EngineEvent::OrderRiskChecked {
                        order_id,
                        outcome: types::RiskOutcomeKind::Approved,
                    },
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

    fn refresh_snapshot(&mut self, sequence_id: SequenceId, now: DateTime<Utc>) {
        self.snapshot_cache = self.core.snapshot(sequence_id, now);
    }

    fn record_panic(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::seconds(PANIC_WINDOW_SECS);
        while self
            .panic_timestamps
            .front()
            .is_some_and(|timestamp| *timestamp < cutoff)
        {
            let _ = self.panic_timestamps.pop_front();
        }
        self.panic_timestamps.push_back(now);
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

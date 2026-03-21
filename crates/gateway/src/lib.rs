//! gateway — VenueAdapter, ExecutionGateway, pipeline stages, and validation.
//!
//! Orchestrates order processing through a composable pipeline:
//! Validate -> CircuitBreaker -> RiskGate -> Route -> Submit.
//!
//! All venue operations are timeout-aware via `TimeoutPolicy`.
//! Connection health is tracked via `VenueConnectionHealth`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

pub use types::{
    ClientOrderId, ConnectionState, HeartbeatConfig, InstrumentId, MarketId, OrderId, OrderState,
    PipelineOrder, RiskChecked, TimeoutPolicy, VenueConnectionHealth, VenueId, VenueOrderId,
};

/// Typed errors for gateway operations (thiserror for libs).
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("venue {venue_id} is not connected (state: {state})")]
    VenueNotConnected { venue_id: String, state: String },

    #[error("agent allocation not found for {agent_id}")]
    AgentAllocationNotFound { agent_id: String },

    #[error("price unavailable for instrument {instrument_id}")]
    PriceUnavailable { instrument_id: String },

    #[error("price for instrument {instrument_id} is stale (max age {max_age:?})")]
    StalePrice {
        instrument_id: String,
        max_age: Duration,
    },

    #[error("no route available for instrument {instrument_id}: {reason}")]
    RoutingUnavailable {
        instrument_id: String,
        reason: String,
    },

    #[error("operation timed out after {elapsed:?}: {operation}")]
    Timeout {
        operation: String,
        elapsed: Duration,
    },

    #[error("venue rejected order: {reason}")]
    VenueRejected { reason: String },

    #[error("order not found: {0}")]
    OrderNotFound(String),

    #[error("venue does not support: {0}")]
    Unsupported(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, GatewayError>;

// ─── Signal → Order Bridge ───

/// Mapping between venue-native symbols and unified instrument identifiers.
#[derive(Debug, Clone, Default)]
pub struct SymbolMap {
    forward: HashMap<(VenueId, String), InstrumentId>,
    reverse: HashMap<InstrumentId, Vec<VenueSymbol>>,
}

impl SymbolMap {
    /// Insert a new `(venue, symbol) → instrument` mapping.
    pub fn insert(
        &mut self,
        venue_id: VenueId,
        symbol: impl Into<String>,
        instrument_id: InstrumentId,
    ) -> Result<()> {
        let symbol = symbol.into();
        let key = (venue_id.clone(), symbol.clone());

        if let Some(existing) = self.forward.get(&key) {
            if existing != &instrument_id {
                return Err(GatewayError::Validation(format!(
                    "symbol map conflict for {}:{}: already mapped to {}",
                    venue_id, symbol, existing
                )));
            }
            return Ok(());
        }

        self.forward.insert(key, instrument_id.clone());
        let reverse_entry = self.reverse.entry(instrument_id).or_default();
        if !reverse_entry
            .iter()
            .any(|candidate| candidate.venue_id == venue_id && candidate.symbol == symbol)
        {
            reverse_entry.push(VenueSymbol { venue_id, symbol });
        }

        Ok(())
    }

    /// Resolve the unified instrument for a venue-native symbol.
    #[must_use]
    pub fn instrument_for(&self, venue_id: &VenueId, symbol: &str) -> Option<&InstrumentId> {
        self.forward.get(&(venue_id.clone(), symbol.to_owned()))
    }

    /// Resolve all venue-native listings for a unified instrument.
    #[must_use]
    pub fn venues_for(&self, instrument_id: &InstrumentId) -> Option<&[VenueSymbol]> {
        self.reverse.get(instrument_id).map(Vec::as_slice)
    }
}

/// One venue-native listing for an instrument.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VenueSymbol {
    pub venue_id: VenueId,
    pub symbol: String,
}

/// Current routing inputs for one venue.
#[derive(Debug, Clone)]
pub struct VenueRouteState {
    pub connection_health: VenueConnectionHealth,
    pub available_balance: Decimal,
}

impl VenueRouteState {
    #[must_use]
    pub fn venue_id(&self) -> &VenueId {
        &self.connection_health.venue_id
    }
}

/// Deterministic routing policy for `SubmitSignal` bridge output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignalRoutingPolicy {
    /// Use the first venue listed in the symbol map after filters are applied.
    FirstListed,
    /// Prefer the eligible venue with the most available balance.
    #[default]
    HighestAvailableBalance,
}

/// Static bridge configuration for constructing draft orders from signals.
#[derive(Debug, Clone)]
pub struct SignalOrderBridgeConfig {
    pub default_order_type: types::OrderType,
    pub default_time_in_force: types::TimeInForce,
    pub dry_run: bool,
    pub routing_policy: SignalRoutingPolicy,
    pub max_price_age: Duration,
}

impl Default for SignalOrderBridgeConfig {
    fn default() -> Self {
        Self {
            default_order_type: types::OrderType::Market,
            default_time_in_force: types::TimeInForce::IOC,
            dry_run: true,
            routing_policy: SignalRoutingPolicy::HighestAvailableBalance,
            max_price_age: Duration::from_secs(30),
        }
    }
}

/// Pure signal bridge: conviction sizing, explicit routing, `PipelineOrder<Draft>` construction.
pub struct SignalOrderBridge {
    config: SignalOrderBridgeConfig,
    symbol_map: SymbolMap,
}

impl SignalOrderBridge {
    #[must_use]
    pub fn new(config: SignalOrderBridgeConfig, symbol_map: SymbolMap) -> Self {
        Self { config, symbol_map }
    }

    /// Translate one signal into zero or more draft orders.
    ///
    /// The bridge currently emits a single draft order for single-leg signals.
    /// It still returns `Vec<_>` so structure expansion can reuse the same API.
    #[must_use = "signal bridge output must be handled explicitly"]
    pub fn signal_to_orders(
        &self,
        signal: &types::Signal,
        firm_book: &types::FirmBook,
        agent_limits: Option<&types::AgentRiskLimits>,
        price_source: &dyn types::PriceSource,
        venue_states: &[VenueRouteState],
        now: DateTime<Utc>,
    ) -> Result<Vec<PipelineOrder<types::Draft>>> {
        if agent_limits.is_none() {
            warn!(
                signal_id = %signal.id,
                agent = %signal.agent,
                "signal_to_orders called without agent risk limits; routing remains unrestricted"
            );
        }

        if let Err(err) = self.validate_signal(signal) {
            info!(
                signal_id = %signal.id,
                agent = %signal.agent,
                instrument = %signal.instrument,
                error = %err,
                "signal_to_orders rejected signal during validation"
            );
            return Err(err);
        }

        let allocation = firm_book
            .agent_allocations
            .get(&signal.agent)
            .ok_or_else(|| GatewayError::AgentAllocationNotFound {
                agent_id: signal.agent.to_string(),
            })
            .inspect_err(|err| {
                info!(
                    signal_id = %signal.id,
                    agent = %signal.agent,
                    error = %err,
                    "signal_to_orders rejected signal because allocation was missing"
                );
            })?;

        if !matches!(allocation.status, types::AgentStatus::Active) {
            let err = GatewayError::Validation(format!(
                "agent {} is not active: {}",
                signal.agent, allocation.status
            ));
            info!(
                signal_id = %signal.id,
                agent = %signal.agent,
                error = %err,
                "signal_to_orders rejected inactive agent"
            );
            return Err(err);
        }

        if signal.conviction.is_zero() {
            info!(
                signal_id = %signal.id,
                agent = %signal.agent,
                "signal_to_orders produced no draft orders for zero-conviction signal"
            );
            return Ok(Vec::new());
        }

        let reference_price =
            self.reference_price_for(signal, price_source)
                .inspect_err(|err| {
                    info!(
                        signal_id = %signal.id,
                        agent = %signal.agent,
                        instrument = %signal.instrument,
                        error = %err,
                        "signal_to_orders rejected signal because reference price was unavailable"
                    );
                })?;
        if let Err(err) = self.validate_exit_targets(signal, reference_price) {
            info!(
                signal_id = %signal.id,
                agent = %signal.agent,
                instrument = %signal.instrument,
                error = %err,
                "signal_to_orders rejected signal because exits were inconsistent with direction"
            );
            return Err(err);
        }

        let qty = self
            .size_signal(signal, allocation.allocated_capital, reference_price)
            .inspect_err(|err| {
                info!(
                    signal_id = %signal.id,
                    agent = %signal.agent,
                    instrument = %signal.instrument,
                    error = %err,
                    "signal_to_orders rejected signal during sizing"
                );
            })?;
        let venue = self
            .select_venue(signal, agent_limits, venue_states)
            .inspect_err(|err| {
                info!(
                    signal_id = %signal.id,
                    agent = %signal.agent,
                    instrument = %signal.instrument,
                    error = %err,
                    "signal_to_orders rejected signal during venue selection"
                );
            })?;

        let order = types::OrderCore {
            id: Self::order_id_for_signal(&signal.id),
            basket_id: signal.basket_id.clone(),
            agent_id: signal.agent.clone(),
            instrument_id: signal.instrument.clone(),
            market_id: MarketId(venue.symbol.clone()),
            venue_id: venue.venue_id.clone(),
            side: signal.direction,
            order_type: self.config.default_order_type,
            time_in_force: self.config.default_time_in_force,
            qty,
            // Market orders still carry a reference price so downstream risk checks
            // can size and evaluate them deterministically.
            price: Some(reference_price),
            trigger_price: None,
            stop_loss: Some(signal.stop_loss),
            take_profit: signal.take_profit,
            post_only: false,
            reduce_only: false,
            dry_run: self.config.dry_run,
            exec_hint: types::ExecHint::default(),
            created_at: now,
        };

        debug!(
            signal_id = %signal.id,
            agent = %signal.agent,
            instrument = %signal.instrument,
            venue = %venue.venue_id,
            market = %venue.symbol,
            qty = %qty,
            reference_price = %reference_price,
            order_id = %order.id,
            "draft order constructed from signal"
        );
        info!(
            signal_id = %signal.id,
            agent = %signal.agent,
            draft_orders = 1_u8,
            "signal_to_orders produced draft orders"
        );

        Ok(vec![PipelineOrder::new(order)])
    }

    fn validate_signal(&self, signal: &types::Signal) -> Result<()> {
        if signal.conviction < Decimal::ZERO || signal.conviction > Decimal::ONE {
            return Err(GatewayError::Validation(format!(
                "signal conviction must be within 0.0..=1.0, got {}",
                signal.conviction
            )));
        }

        if signal.conviction.is_zero() && signal.sizing_hint.is_some() {
            return Err(GatewayError::Validation(
                "sizing_hint provided with zero conviction; intent is ambiguous".to_owned(),
            ));
        }

        if signal.stop_loss <= Decimal::ZERO {
            return Err(GatewayError::Validation(format!(
                "signal stop_loss must be positive, got {}",
                signal.stop_loss
            )));
        }

        if signal.thesis.trim().is_empty() {
            return Err(GatewayError::Validation(
                "signal thesis must be non-empty".to_owned(),
            ));
        }

        if let Some(sizing_hint) = signal.sizing_hint {
            if sizing_hint <= Decimal::ZERO {
                return Err(GatewayError::Validation(format!(
                    "signal sizing_hint must be positive, got {}",
                    sizing_hint
                )));
            }
        }

        Ok(())
    }

    fn validate_exit_targets(
        &self,
        signal: &types::Signal,
        reference_price: Decimal,
    ) -> Result<()> {
        let valid_stop_loss = match signal.direction {
            types::Side::Buy => signal.stop_loss < reference_price,
            types::Side::Sell => signal.stop_loss > reference_price,
        };

        if !valid_stop_loss {
            return Err(GatewayError::Validation(format!(
                "stop_loss {} is not on the losing side of reference price {} for {:?}",
                signal.stop_loss, reference_price, signal.direction
            )));
        }

        if let Some(take_profit) = signal.take_profit {
            let valid_take_profit = match signal.direction {
                types::Side::Buy => take_profit > reference_price,
                types::Side::Sell => take_profit < reference_price,
            };

            if !valid_take_profit {
                return Err(GatewayError::Validation(format!(
                    "take_profit {} is not on the profitable side of reference price {} for {:?}",
                    take_profit, reference_price, signal.direction
                )));
            }
        }

        Ok(())
    }

    fn reference_price_for(
        &self,
        signal: &types::Signal,
        price_source: &dyn types::PriceSource,
    ) -> Result<Decimal> {
        let snapshot = price_source
            .latest_price(&signal.instrument)
            .ok_or_else(|| GatewayError::PriceUnavailable {
                instrument_id: signal.instrument.to_string(),
            })?;

        if price_source.is_stale(&signal.instrument, self.config.max_price_age) {
            return Err(GatewayError::StalePrice {
                instrument_id: signal.instrument.to_string(),
                max_age: self.config.max_price_age,
            });
        }

        let reference_price = match signal.direction {
            types::Side::Buy => snapshot.ask,
            types::Side::Sell => snapshot.bid,
        };

        if reference_price <= Decimal::ZERO {
            return Err(GatewayError::Validation(format!(
                "reference price must be positive, got {}",
                reference_price
            )));
        }

        Ok(reference_price)
    }

    fn size_signal(
        &self,
        signal: &types::Signal,
        allocated_capital: Decimal,
        reference_price: Decimal,
    ) -> Result<Decimal> {
        let qty = if let Some(sizing_hint) = signal.sizing_hint {
            if sizing_hint > allocated_capital {
                return Err(GatewayError::Validation(format!(
                    "sizing_hint {} exceeds agent allocated_capital {}",
                    sizing_hint, allocated_capital
                )));
            }
            sizing_hint / reference_price
        } else {
            let max_loss_per_unit = (reference_price - signal.stop_loss).abs();
            if max_loss_per_unit <= Decimal::ZERO {
                return Err(GatewayError::Validation(
                    "signal stop_loss must differ from the reference price".to_owned(),
                ));
            }

            (signal.conviction * allocated_capital) / max_loss_per_unit
        };

        if qty <= Decimal::ZERO {
            return Err(GatewayError::Validation(format!(
                "signal sizing produced a non-positive quantity: {}",
                qty
            )));
        }

        Ok(qty)
    }

    fn select_venue(
        &self,
        signal: &types::Signal,
        agent_limits: Option<&types::AgentRiskLimits>,
        venue_states: &[VenueRouteState],
    ) -> Result<VenueSymbol> {
        if let Some(allowed_instruments) =
            agent_limits.and_then(|limits| limits.allowed_instruments.as_ref())
        {
            if !allowed_instruments
                .iter()
                .any(|instrument_id| instrument_id == &signal.instrument)
            {
                return Err(GatewayError::RoutingUnavailable {
                    instrument_id: signal.instrument.to_string(),
                    reason: format!(
                        "instrument {} is not allowed for agent {}",
                        signal.instrument, signal.agent
                    ),
                });
            }
        }

        let mapped_venues = self
            .symbol_map
            .venues_for(&signal.instrument)
            .ok_or_else(|| GatewayError::RoutingUnavailable {
                instrument_id: signal.instrument.to_string(),
                reason: "instrument is missing from the symbol map".to_owned(),
            })?;

        let allowed_venues = agent_limits.and_then(|limits| limits.allowed_venues.as_ref());
        let mut eligible_states = mapped_venues
            .iter()
            .filter_map(|mapping| {
                let state = venue_states
                    .iter()
                    .find(|candidate| candidate.venue_id() == &mapping.venue_id)?;

                if let Some(allowed_venues) = allowed_venues {
                    if !allowed_venues
                        .iter()
                        .any(|venue_id| venue_id == &mapping.venue_id)
                    {
                        return None;
                    }
                }

                if !state.connection_health.is_submittable() {
                    return None;
                }

                Some((mapping, state))
            })
            .collect::<Vec<_>>();

        if eligible_states.is_empty() {
            return Err(GatewayError::RoutingUnavailable {
                instrument_id: signal.instrument.to_string(),
                reason: format!(
                    "no mapped venue is simultaneously allowed and connected for {}",
                    signal.instrument
                ),
            });
        }

        let selected = match self.config.routing_policy {
            SignalRoutingPolicy::FirstListed => eligible_states.remove(0).0.clone(),
            SignalRoutingPolicy::HighestAvailableBalance => eligible_states
                .into_iter()
                .max_by(|(_, left), (_, right)| {
                    left.available_balance.cmp(&right.available_balance)
                })
                .map(|(mapping, _)| mapping.clone())
                .ok_or_else(|| GatewayError::RoutingUnavailable {
                    instrument_id: signal.instrument.to_string(),
                    reason: "no eligible venues remained after routing".to_owned(),
                })?,
        };

        Ok(selected)
    }

    fn order_id_for_signal(signal_id: &types::SignalId) -> OrderId {
        OrderId(format!("ord-{signal_id}"))
    }
}

// ─── Venue Adapter ───

/// Adapter for a single trading venue (exchange).
///
/// "Dumb adapter" — does translation only, no business logic.
/// Each adapter instance manages one venue connection.
///
/// All operations respect the configured `TimeoutPolicy`.
/// Connection health is reported via `connection_health()`.
///
/// Sealed trait — external crates cannot implement this.
#[async_trait::async_trait]
pub trait VenueAdapter: Send + Sync + sealed::Sealed {
    /// Venue identifier.
    fn venue_id(&self) -> &VenueId;

    /// Current connection health (heartbeat, latency, state).
    fn connection_health(&self) -> &VenueConnectionHealth;

    /// Timeout policy for this venue's operations.
    fn timeout_policy(&self) -> &TimeoutPolicy;

    /// Paper adapters simulate fills locally and should still receive dry-run orders.
    #[must_use]
    fn simulates_fills_locally(&self) -> bool {
        false
    }

    /// Submit an order to the venue.
    /// Returns venue-assigned order ID on success.
    ///
    /// Caller must check `connection_health().is_submittable()` first.
    /// If connection is degraded, this returns `VenueNotConnected`.
    async fn submit_order(&self, submission: OrderSubmission) -> Result<VenueOrderAck>;

    /// Cancel an order by venue order ID.
    async fn cancel_order(&self, venue_order_id: &VenueOrderId) -> Result<()>;

    /// Amend an order (if venue supports it).
    async fn amend_order(
        &self,
        venue_order_id: &VenueOrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<VenueOrderAck>;

    /// Query current positions on this venue.
    async fn query_positions(&self) -> Result<Vec<VenuePosition>>;

    /// Query open orders on this venue.
    async fn query_open_orders(&self) -> Result<Vec<VenueOpenOrder>>;

    /// Query account balance on this venue.
    async fn query_balance(&self) -> Result<VenueBalance>;

    /// Subscribe to fill stream. Returns a bounded receiver.
    /// The adapter pushes fills as they arrive from the venue.
    async fn subscribe_fills(&self) -> Result<tokio::sync::mpsc::Receiver<VenueFill>>;

    /// Venue capabilities (what order types, TIF, amendments are supported).
    fn capabilities(&self) -> &VenueCapabilities;

    /// Start the connection (WebSocket, REST polling, etc).
    async fn connect(&mut self) -> Result<()>;

    /// Graceful disconnect.
    async fn disconnect(&mut self) -> Result<()>;
}

/// Sealed trait pattern — prevents external implementation of VenueAdapter.
mod sealed {
    pub trait Sealed {}
}

// ─── Paper fill simulation ───

/// Simulated fill enriched with fill-quality diagnostics.
#[derive(Debug, Clone)]
#[must_use]
pub struct SimulatedFill {
    pub fill: types::Fill,
    pub remaining_qty: Decimal,
    pub slippage_bps: Decimal,
    pub market_impact_bps: Decimal,
}

impl SimulatedFill {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.remaining_qty.is_zero()
    }
}

/// Result of simulating an order against the latest orderbook snapshot.
#[derive(Debug, Clone)]
pub enum FillSimulationOutcome {
    Resting,
    Executed(SimulatedFill),
}

/// Paper-mode fill simulation against live orderbook depth.
pub trait FillSimulator: Send + Sync {
    fn simulate(
        &self,
        order: &types::OrderCore,
        venue_order_id: &VenueOrderId,
        book: &types::OrderbookSnapshot,
        fee_model: &dyn types::FeeModel,
    ) -> Result<FillSimulationOutcome>;
}

/// Simple depth-walking simulator for paper mode.
#[derive(Debug, Default)]
pub struct LiveOrderbookFillSimulator;

impl FillSimulator for LiveOrderbookFillSimulator {
    fn simulate(
        &self,
        order: &types::OrderCore,
        venue_order_id: &VenueOrderId,
        book: &types::OrderbookSnapshot,
        fee_model: &dyn types::FeeModel,
    ) -> Result<FillSimulationOutcome> {
        if order.qty <= Decimal::ZERO {
            return Err(GatewayError::Validation(
                "order quantity must be positive".to_owned(),
            ));
        }

        if order.venue_id != book.venue_id {
            return Err(GatewayError::Validation(format!(
                "order venue {} does not match orderbook venue {}",
                order.venue_id, book.venue_id
            )));
        }

        if order.instrument_id != book.instrument_id {
            return Err(GatewayError::Validation(format!(
                "order instrument {} does not match orderbook instrument {}",
                order.instrument_id, book.instrument_id
            )));
        }

        book.validate()
            .map_err(|err| GatewayError::Validation(err.to_string()))?;

        let price_limit =
            match order.order_type {
                types::OrderType::Market => None,
                types::OrderType::Limit => Some(order.price.ok_or_else(|| {
                    GatewayError::Validation("limit order missing price".to_owned())
                })?),
                _ => {
                    return Err(GatewayError::Unsupported(format!(
                        "paper adapter only simulates market and limit orders, got {}",
                        order.order_type
                    )));
                }
            };

        let (levels, top_of_book) = match order.side {
            types::Side::Buy => (
                book.asks.as_slice(),
                book.best_ask().map(|level| level.price).ok_or_else(|| {
                    GatewayError::PriceUnavailable {
                        instrument_id: order.instrument_id.to_string(),
                    }
                })?,
            ),
            types::Side::Sell => (
                book.bids.as_slice(),
                book.best_bid().map(|level| level.price).ok_or_else(|| {
                    GatewayError::PriceUnavailable {
                        instrument_id: order.instrument_id.to_string(),
                    }
                })?,
            ),
        };

        let mut remaining_qty = order.qty;
        let mut executed_qty = Decimal::ZERO;
        let mut executed_notional = Decimal::ZERO;
        let mut worst_price: Option<Decimal> = None;

        for level in levels {
            if !is_level_fillable(order.side, price_limit, level.price) {
                break;
            }

            let level_fill_qty = if remaining_qty < level.qty {
                remaining_qty
            } else {
                level.qty
            };
            executed_qty += level_fill_qty;
            executed_notional += level_fill_qty * level.price;
            remaining_qty -= level_fill_qty;
            worst_price = Some(level.price);

            if remaining_qty <= Decimal::ZERO {
                remaining_qty = Decimal::ZERO;
                break;
            }
        }

        if executed_qty.is_zero() {
            return Ok(FillSimulationOutcome::Resting);
        }

        let average_price = executed_notional / executed_qty;
        let mut fill = types::Fill {
            order_id: order.id.clone(),
            venue_order_id: venue_order_id.clone(),
            instrument_id: order.instrument_id.clone(),
            side: order.side,
            qty: executed_qty,
            price: average_price,
            fee: Decimal::ZERO,
            filled_at: book.received_at,
            is_maker: false,
        };
        let fee = fee_model
            .calculate(&fill)
            .map_err(|err| GatewayError::Validation(err.to_string()))?;
        fill.fee = fee.net;

        Ok(FillSimulationOutcome::Executed(SimulatedFill {
            fill,
            remaining_qty,
            slippage_bps: basis_points_delta(average_price, top_of_book),
            market_impact_bps: basis_points_delta(worst_price.unwrap_or(top_of_book), top_of_book),
        }))
    }
}

fn is_level_fillable(
    side: types::Side,
    price_limit: Option<Decimal>,
    level_price: Decimal,
) -> bool {
    match (side, price_limit) {
        (_, None) => true,
        (types::Side::Buy, Some(limit)) => level_price <= limit,
        (types::Side::Sell, Some(limit)) => level_price >= limit,
    }
}

fn basis_points_delta(actual: Decimal, reference: Decimal) -> Decimal {
    if reference.is_zero() {
        return Decimal::ZERO;
    }

    ((actual - reference).abs() / reference) * Decimal::new(10_000, 0)
}

/// Explicit paper-adapter configuration.
#[derive(Debug, Clone)]
pub struct PaperAdapterConfig {
    pub venue_id: VenueId,
    pub timeout_policy: TimeoutPolicy,
    pub fill_channel_capacity: usize,
}

impl PaperAdapterConfig {
    pub const DEFAULT_FILL_CHANNEL_CAPACITY: usize = 64;

    #[must_use]
    pub fn new(venue_id: VenueId) -> Self {
        Self {
            venue_id,
            timeout_policy: TimeoutPolicy::default(),
            fill_channel_capacity: Self::DEFAULT_FILL_CHANNEL_CAPACITY,
        }
    }
}

#[derive(Debug, Default)]
struct PaperAdapterState {
    orderbooks: HashMap<MarketId, types::OrderbookSnapshot>,
    open_orders: HashMap<VenueOrderId, PaperOpenOrder>,
    idempotency_cache: HashMap<ClientOrderId, VenueOrderAck>,
}

#[derive(Debug, Clone)]
struct PaperOpenOrder {
    submission: OrderSubmission,
    venue_order_id: VenueOrderId,
    accepted_at: DateTime<Utc>,
    filled_qty: Decimal,
}

impl PaperOpenOrder {
    #[must_use]
    fn as_open_order(&self) -> VenueOpenOrder {
        let state = if self.filled_qty.is_zero() {
            OrderState::Accepted
        } else {
            OrderState::PartiallyFilled
        };

        VenueOpenOrder {
            venue_order_id: self.venue_order_id.clone(),
            client_order_id: Some(self.submission.client_order_id.clone()),
            instrument_id: self.submission.instrument_id.clone(),
            side: self.submission.side,
            qty: self.submission.qty,
            filled_qty: self.filled_qty,
            price: self.submission.price,
            state,
            created_at: self.accepted_at,
        }
    }
}

/// Adapter that consumes live market-data snapshots and simulates fills locally.
pub struct PaperAdapter {
    config: PaperAdapterConfig,
    connection_health: VenueConnectionHealth,
    capabilities: VenueCapabilities,
    fill_simulator: Arc<dyn FillSimulator>,
    fee_model: Arc<dyn types::FeeModel>,
    state: tokio::sync::Mutex<PaperAdapterState>,
    fill_subscribers: tokio::sync::Mutex<Vec<tokio::sync::mpsc::Sender<VenueFill>>>,
}

impl PaperAdapter {
    #[must_use]
    pub fn new(
        config: PaperAdapterConfig,
        fill_simulator: Arc<dyn FillSimulator>,
        fee_model: Arc<dyn types::FeeModel>,
    ) -> Self {
        let venue_id = config.venue_id.clone();
        Self {
            config,
            connection_health: VenueConnectionHealth::new(venue_id.clone()),
            capabilities: VenueCapabilities {
                venue_id,
                supports_market_orders: true,
                supports_limit_orders: true,
                supports_stop_market: false,
                supports_stop_limit: false,
                supports_trailing_stop: false,
                supports_oco: false,
                supports_amendment: false,
                supports_reduce_only: true,
                supports_post_only: true,
                max_batch_size: Some(1),
            },
            fill_simulator,
            fee_model,
            state: tokio::sync::Mutex::new(PaperAdapterState::default()),
            fill_subscribers: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Replace the latest live orderbook snapshot for a market.
    pub async fn replace_orderbook(&self, snapshot: types::OrderbookSnapshot) -> Result<()> {
        if snapshot.venue_id != self.config.venue_id {
            return Err(GatewayError::Validation(format!(
                "paper adapter for {} cannot ingest snapshot for {}",
                self.config.venue_id, snapshot.venue_id
            )));
        }

        snapshot
            .validate()
            .map_err(|err| GatewayError::Validation(err.to_string()))?;

        let mut state = self.state.lock().await;
        state
            .orderbooks
            .insert(snapshot.market_id.clone(), snapshot);
        Ok(())
    }

    async fn broadcast_fill(&self, fill: VenueFill) {
        let subscribers = {
            let subscribers = self.fill_subscribers.lock().await;
            subscribers.clone()
        };

        for subscriber in subscribers {
            let _ = subscriber.send(fill.clone()).await;
        }

        let mut subscribers = self.fill_subscribers.lock().await;
        subscribers.retain(|sender| !sender.is_closed());
    }

    fn validate_submission(&self, submission: &OrderSubmission) -> Result<()> {
        if submission.venue_id != self.config.venue_id {
            return Err(GatewayError::Validation(format!(
                "paper adapter {} cannot accept order for venue {}",
                self.config.venue_id, submission.venue_id
            )));
        }

        match submission.order_type {
            types::OrderType::Market | types::OrderType::Limit => {}
            _ => {
                return Err(GatewayError::Unsupported(format!(
                    "paper adapter does not support order type {}",
                    submission.order_type
                )));
            }
        }

        match submission.time_in_force {
            types::TimeInForce::GTC | types::TimeInForce::IOC | types::TimeInForce::FOK => {}
            types::TimeInForce::GTD { .. } => {
                return Err(GatewayError::Unsupported(
                    "paper adapter does not support GTD expiry handling yet".to_owned(),
                ));
            }
        }

        if submission.order_type == types::OrderType::Limit && submission.price.is_none() {
            return Err(GatewayError::Validation(
                "limit order missing price".to_owned(),
            ));
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl VenueAdapter for PaperAdapter {
    fn venue_id(&self) -> &VenueId {
        &self.config.venue_id
    }

    fn connection_health(&self) -> &VenueConnectionHealth {
        &self.connection_health
    }

    fn timeout_policy(&self) -> &TimeoutPolicy {
        &self.config.timeout_policy
    }

    fn simulates_fills_locally(&self) -> bool {
        true
    }

    async fn submit_order(&self, submission: OrderSubmission) -> Result<VenueOrderAck> {
        if !self.connection_health.is_submittable() {
            return Err(GatewayError::VenueNotConnected {
                venue_id: self.config.venue_id.to_string(),
                state: format!("{:?}", self.connection_health.state),
            });
        }

        self.validate_submission(&submission)?;

        {
            let state = self.state.lock().await;
            if let Some(existing) = state.idempotency_cache.get(&submission.client_order_id) {
                return Ok(existing.clone());
            }
        }

        let book = {
            let state = self.state.lock().await;
            state
                .orderbooks
                .get(&submission.market_id)
                .cloned()
                .ok_or_else(|| GatewayError::PriceUnavailable {
                    instrument_id: submission.instrument_id.to_string(),
                })?
        };

        let accepted_at = book.received_at;
        let venue_order_id = VenueOrderId(format!("paper-{}", submission.client_order_id));
        let client_order_id = submission.client_order_id.clone();
        let ack = VenueOrderAck {
            venue_order_id: venue_order_id.clone(),
            client_order_id: client_order_id.clone(),
            accepted_at,
        };
        let order_core = types::OrderCore {
            id: submission.order_id.clone(),
            basket_id: None,
            agent_id: submission.agent_id.clone(),
            instrument_id: submission.instrument_id.clone(),
            market_id: submission.market_id.clone(),
            venue_id: submission.venue_id.clone(),
            side: submission.side,
            order_type: submission.order_type,
            time_in_force: submission.time_in_force,
            qty: submission.qty,
            price: submission.price,
            trigger_price: submission.trigger_price,
            stop_loss: submission.stop_loss,
            take_profit: submission.take_profit,
            post_only: submission.post_only,
            reduce_only: submission.reduce_only,
            dry_run: submission.dry_run,
            exec_hint: submission.exec_hint.clone(),
            created_at: accepted_at,
        };

        let simulation = self.fill_simulator.simulate(
            &order_core,
            &venue_order_id,
            &book,
            self.fee_model.as_ref(),
        )?;

        if submission.post_only && matches!(simulation, FillSimulationOutcome::Executed(_)) {
            return Err(GatewayError::VenueRejected {
                reason: "post-only order would take liquidity on paper adapter".to_owned(),
            });
        }

        let fill_to_broadcast = match simulation {
            FillSimulationOutcome::Resting => {
                let mut state = self.state.lock().await;
                state
                    .idempotency_cache
                    .insert(client_order_id.clone(), ack.clone());
                state.open_orders.insert(
                    venue_order_id.clone(),
                    PaperOpenOrder {
                        submission,
                        venue_order_id,
                        accepted_at,
                        filled_qty: Decimal::ZERO,
                    },
                );
                None
            }
            FillSimulationOutcome::Executed(simulated_fill) => {
                if !simulated_fill.is_complete() {
                    match submission.time_in_force {
                        types::TimeInForce::GTC
                            if submission.order_type == types::OrderType::Limit =>
                        {
                            let mut state = self.state.lock().await;
                            state
                                .idempotency_cache
                                .insert(client_order_id.clone(), ack.clone());
                            state.open_orders.insert(
                                venue_order_id.clone(),
                                PaperOpenOrder {
                                    submission,
                                    venue_order_id: venue_order_id.clone(),
                                    accepted_at,
                                    filled_qty: simulated_fill.fill.qty,
                                },
                            );
                        }
                        _ => {
                            return Err(GatewayError::VenueRejected {
                                reason: format!(
                                    "paper adapter only allows residual quantity on resting GTC limit orders; {} {} left {} unfilled",
                                    submission.order_type,
                                    submission.time_in_force,
                                    simulated_fill.remaining_qty
                                ),
                            });
                        }
                    }
                } else {
                    let mut state = self.state.lock().await;
                    state
                        .idempotency_cache
                        .insert(client_order_id.clone(), ack.clone());
                }

                Some(VenueFill {
                    venue_order_id,
                    client_order_id,
                    instrument_id: simulated_fill.fill.instrument_id.clone(),
                    side: simulated_fill.fill.side,
                    qty: simulated_fill.fill.qty,
                    price: simulated_fill.fill.price,
                    fee: simulated_fill.fill.fee,
                    is_maker: simulated_fill.fill.is_maker,
                    filled_at: simulated_fill.fill.filled_at,
                })
            }
        };

        if let Some(fill) = fill_to_broadcast {
            self.broadcast_fill(fill).await;
        }

        Ok(ack)
    }

    async fn cancel_order(&self, venue_order_id: &VenueOrderId) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.open_orders.remove(venue_order_id).is_some() {
            return Ok(());
        }

        Err(GatewayError::OrderNotFound(venue_order_id.to_string()))
    }

    async fn amend_order(
        &self,
        _venue_order_id: &VenueOrderId,
        _new_price: Option<Decimal>,
        _new_qty: Option<Decimal>,
    ) -> Result<VenueOrderAck> {
        Err(GatewayError::Unsupported(
            "paper adapter does not support amendments yet".to_owned(),
        ))
    }

    async fn query_positions(&self) -> Result<Vec<VenuePosition>> {
        Err(GatewayError::Unsupported(
            "paper adapter does not own paper positions; query the sequencer state instead"
                .to_owned(),
        ))
    }

    async fn query_open_orders(&self) -> Result<Vec<VenueOpenOrder>> {
        let state = self.state.lock().await;
        Ok(state
            .open_orders
            .values()
            .map(PaperOpenOrder::as_open_order)
            .collect())
    }

    async fn query_balance(&self) -> Result<VenueBalance> {
        Err(GatewayError::Unsupported(
            "paper adapter does not model balances yet".to_owned(),
        ))
    }

    async fn subscribe_fills(&self) -> Result<tokio::sync::mpsc::Receiver<VenueFill>> {
        let (tx, rx) = tokio::sync::mpsc::channel(self.config.fill_channel_capacity);
        let mut subscribers = self.fill_subscribers.lock().await;
        subscribers.push(tx);
        Ok(rx)
    }

    fn capabilities(&self) -> &VenueCapabilities {
        &self.capabilities
    }

    async fn connect(&mut self) -> Result<()> {
        let now = Utc::now();
        self.connection_health.state = ConnectionState::Connected;
        self.connection_health.last_message_at = Some(now);
        self.connection_health.last_heartbeat_at = Some(now);
        self.connection_health.connected_since = Some(now);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.connection_health.state = ConnectionState::Disconnected;
        self.connection_health.connected_since = None;
        Ok(())
    }
}

impl sealed::Sealed for PaperAdapter {}

// ─── Execution Gateway ───

/// Main entry point for order submission. Orchestrates the full pipeline.
///
/// Accepts `PipelineOrder<Routed>` — the type system guarantees the order
/// has passed validation, risk checks, and routing before venue submission.
///
/// Idempotent on `ClientOrderId` — duplicate submissions return cached acknowledgment.
/// Checks connection health before routing to a venue adapter.
#[async_trait::async_trait]
pub trait ExecutionGateway: Send + Sync {
    /// Submit a routed order through the venue adapter.
    ///
    /// The pipeline type ensures this order has passed:
    /// 1. Structural validation (Draft → Validated)
    /// 2. Circuit breaker + risk gate (Validated → RiskChecked)
    /// 3. Venue routing (RiskChecked → Routed)
    async fn submit(&self, order: PipelineOrder<types::Routed>) -> Result<OrderAck>;

    /// Cancel an open order.
    async fn cancel(&self, order_id: &OrderId) -> Result<()>;

    /// Amend an open order (if venue supports it).
    async fn amend(
        &self,
        order_id: &OrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()>;

    /// Emergency: cancel all open orders across all venues.
    async fn cancel_all(&self) -> Result<u32>;

    /// Get connection health for all venues.
    fn venue_health(&self) -> Vec<VenueConnectionHealth>;
}

// ─── Router ───

/// Routes a risk-checked order to a venue, producing the `RiskChecked → Routed` transition.
///
/// Responsible for:
/// 1. Selecting the target venue (may differ from the requested venue).
/// 2. Generating a `ClientOrderId` for idempotent submission.
/// 3. Checking venue connection health before committing.
///
/// The router does NOT submit the order — it only produces the routing assignment.
/// Submission happens via `ExecutionGateway::submit()`.
pub trait Router: Send + Sync {
    /// Route a risk-checked order, producing a `PipelineOrder<Routed>`.
    ///
    /// Returns `GatewayError::VenueNotConnected` if the selected venue is unavailable.
    fn route(
        &self,
        order: PipelineOrder<types::RiskChecked>,
        now: DateTime<Utc>,
    ) -> Result<PipelineOrder<types::Routed>>;
}

// ─── Pipeline Stage ───

/// Composable processing stage. Stages are executed in order;
/// first rejection stops the pipeline.
#[async_trait::async_trait]
pub trait PipelineStage: Send + Sync {
    /// Name of this stage (for logging/tracing).
    fn name(&self) -> &str;

    /// Process order through this stage.
    async fn process(&self, order: OrderSubmission) -> Result<OrderSubmission>;
}

// ─── Order Validator ───

/// Fast, stateless validation before order enters the pipeline.
pub trait OrderValidator: Send + Sync {
    fn validate(
        &self,
        order: &OrderSubmission,
        caps: &VenueCapabilities,
    ) -> std::result::Result<(), Vec<ValidationError>>;
}

// ─── Types ───

/// Order submission request (sent to venue adapter).
///
/// This is the flattened, venue-ready form extracted from `PipelineOrder<Routed>`.
/// The adapter receives this — it doesn't know about pipeline stages.
#[derive(Debug, Clone)]
pub struct OrderSubmission {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub agent_id: types::AgentId,
    pub instrument_id: InstrumentId,
    pub market_id: MarketId,
    pub venue_id: VenueId,
    pub side: types::Side,
    pub order_type: types::OrderType,
    pub time_in_force: types::TimeInForce,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    pub trigger_price: Option<Decimal>,
    pub stop_loss: Option<Decimal>,
    pub take_profit: Option<Decimal>,
    pub post_only: bool,
    pub reduce_only: bool,
    pub dry_run: bool,
    pub exec_hint: types::ExecHint,
}

impl OrderSubmission {
    /// Create an `OrderSubmission` from a routed pipeline order.
    #[must_use]
    pub fn from_routed(order: &PipelineOrder<types::Routed>) -> Self {
        let core = order.core();
        let routing = order.routing();
        Self {
            order_id: core.id.clone(),
            client_order_id: routing.client_order_id.clone(),
            agent_id: core.agent_id.clone(),
            instrument_id: core.instrument_id.clone(),
            market_id: core.market_id.clone(),
            venue_id: routing.venue_id.clone(),
            side: core.side,
            order_type: core.order_type,
            time_in_force: core.time_in_force,
            qty: core.qty,
            price: core.price,
            trigger_price: core.trigger_price,
            stop_loss: core.stop_loss,
            take_profit: core.take_profit,
            post_only: core.post_only,
            reduce_only: core.reduce_only,
            dry_run: core.dry_run,
            exec_hint: core.exec_hint.clone(),
        }
    }
}

/// Acknowledgment from venue after order submission.
#[derive(Debug, Clone)]
#[must_use]
pub struct VenueOrderAck {
    pub venue_order_id: VenueOrderId,
    pub client_order_id: ClientOrderId,
    pub accepted_at: DateTime<Utc>,
}

/// Fill received from venue.
#[derive(Debug, Clone)]
pub struct VenueFill {
    pub venue_order_id: VenueOrderId,
    pub client_order_id: ClientOrderId,
    pub instrument_id: InstrumentId,
    pub side: types::Side,
    pub qty: Decimal,
    pub price: Decimal,
    pub fee: Decimal,
    pub is_maker: bool,
    pub filled_at: DateTime<Utc>,
}

/// Position as reported by venue.
#[derive(Debug, Clone)]
pub struct VenuePosition {
    pub instrument_id: InstrumentId,
    pub qty: Decimal,
    pub avg_entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub margin_used: Decimal,
}

/// Open order as reported by venue.
#[derive(Debug, Clone)]
pub struct VenueOpenOrder {
    pub venue_order_id: VenueOrderId,
    pub client_order_id: Option<ClientOrderId>,
    pub instrument_id: InstrumentId,
    pub side: types::Side,
    pub qty: Decimal,
    pub filled_qty: Decimal,
    pub price: Option<Decimal>,
    pub state: OrderState,
    pub created_at: DateTime<Utc>,
}

/// Account balance as reported by venue.
#[derive(Debug, Clone)]
pub struct VenueBalance {
    pub total_equity: Decimal,
    pub available_balance: Decimal,
    pub margin_used: Decimal,
    pub unrealized_pnl: Decimal,
    pub as_of: DateTime<Utc>,
}

/// Gateway-level order acknowledgment.
#[derive(Debug, Clone)]
#[must_use]
pub struct OrderAck {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub venue_order_id: VenueOrderId,
    pub accepted_at: DateTime<Utc>,
}

/// What a venue supports.
#[derive(Debug, Clone)]
pub struct VenueCapabilities {
    pub venue_id: VenueId,
    pub supports_market_orders: bool,
    pub supports_limit_orders: bool,
    pub supports_stop_market: bool,
    pub supports_stop_limit: bool,
    pub supports_trailing_stop: bool,
    pub supports_oco: bool,
    pub supports_amendment: bool,
    pub supports_reduce_only: bool,
    pub supports_post_only: bool,
    pub max_batch_size: Option<u32>,
}

// ─── Validation errors ───

#[derive(Debug, Clone)]
pub enum ValidationError {
    InvalidQty {
        reason: String,
    },
    InvalidPrice {
        reason: String,
    },
    UnsupportedOrderType {
        requested: types::OrderType,
        venue: VenueId,
    },
    UnsupportedTIF {
        requested: types::TimeInForce,
        venue: VenueId,
    },
    MissingRequiredField {
        field: String,
    },
    IncompatibleConstraints {
        reason: String,
    },
    PrecisionExceeded {
        field: String,
        max_decimals: u32,
        actual: u32,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQty { reason } => write!(f, "Invalid quantity: {reason}"),
            Self::InvalidPrice { reason } => write!(f, "Invalid price: {reason}"),
            Self::UnsupportedOrderType { requested, venue } => {
                write!(f, "Order type {requested} not supported on {venue}")
            }
            Self::UnsupportedTIF { requested, venue } => {
                write!(f, "TIF {requested} not supported on {venue}")
            }
            Self::MissingRequiredField { field } => write!(f, "Missing required field: {field}"),
            Self::IncompatibleConstraints { reason } => {
                write!(f, "Incompatible constraints: {reason}")
            }
            Self::PrecisionExceeded {
                field,
                max_decimals,
                actual,
            } => {
                write!(
                    f,
                    "Field {field} precision exceeded: max {max_decimals}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for ValidationError {}

// ─── Venue Submit Task ───

/// Capacity of the bounded venue submit channel (matches engine-core).
pub use engine_core::VENUE_SUBMIT_CHANNEL_CAPACITY;

/// Async task that receives `PipelineOrder<Routed>` from the Sequencer and submits
/// them to venue adapters.
///
/// Ownership model:
/// - The Sequencer holds the **send** side of the channel.
/// - `VenueSubmitTask` holds the **receive** side.
/// - Venue acks (success or failure) are sent back via `SequencerHandle`.
///
/// Safety invariants:
/// - `dry_run` orders are short-circuited: a simulated `VenueOrderId` is returned
///   immediately without calling the adapter.
/// - If no adapter is registered for the order's `venue_id`, `OnVenueSubmitFailed`
///   is sent — never a silent drop.
/// - All channels are bounded; the submit receive channel capacity matches
///   `VENUE_SUBMIT_CHANNEL_CAPACITY`.
pub struct VenueSubmitTask {
    submit_rx: tokio::sync::mpsc::Receiver<types::PipelineOrder<types::Routed>>,
    /// Live adapter instances keyed by venue ID.
    adapters: std::collections::HashMap<VenueId, std::sync::Arc<dyn VenueAdapter>>,
    sequencer_handle: engine_core::SequencerHandle,
    clock: std::sync::Arc<dyn types::Clock>,
}

impl VenueSubmitTask {
    /// Create a new `VenueSubmitTask`.
    ///
    /// `submit_rx` is the receive side of the bounded channel created alongside the Sequencer
    /// (use `tokio::sync::mpsc::channel(engine_core::VENUE_SUBMIT_CHANNEL_CAPACITY)`).
    #[must_use]
    pub fn new(
        submit_rx: tokio::sync::mpsc::Receiver<types::PipelineOrder<types::Routed>>,
        adapters: std::collections::HashMap<VenueId, std::sync::Arc<dyn VenueAdapter>>,
        sequencer_handle: engine_core::SequencerHandle,
        clock: std::sync::Arc<dyn types::Clock>,
    ) -> Self {
        Self {
            submit_rx,
            adapters,
            sequencer_handle,
            clock,
        }
    }

    /// Run the venue submit loop.
    ///
    /// Exits when the send side of the submit channel is dropped (sequencer shut down).
    pub async fn run(mut self) {
        use engine_core::SequencerCommand;
        use tracing::{error, info, warn};

        while let Some(order) = self.submit_rx.recv().await {
            let order_id = order.core.id.clone();
            let venue_id = order.core.venue_id.clone();
            let now = self.clock.now();

            // Gate 2 — adapter lookup: fail fast if no adapter registered for this venue.
            let adapter = match self.adapters.get(&venue_id) {
                Some(a) => std::sync::Arc::clone(a),
                None => {
                    let reason = format!("no adapter registered for venue {venue_id}");
                    warn!(order_id = %order_id, reason = %reason, "venue submission failed");
                    let cmd = SequencerCommand::OnVenueSubmitFailed { order_id, reason };
                    if let Err(err) = self.sequencer_handle.send(cmd).await {
                        error!(error = %err, "failed to send OnVenueSubmitFailed to sequencer");
                    }
                    continue;
                }
            };

            // Gate 1 — dry_run: simulate ack without calling real adapters.
            if order.core.dry_run && !adapter.simulates_fills_locally() {
                let simulated_id =
                    types::VenueOrderId(format!("sim-{}", order.routing().client_order_id));
                info!(
                    order_id = %order_id,
                    venue_order_id = %simulated_id,
                    "dry_run — simulating venue ack"
                );
                let cmd = SequencerCommand::OnVenueAck {
                    order_id,
                    venue_order_id: simulated_id,
                    submitted_at: now,
                };
                if let Err(err) = self.sequencer_handle.send(cmd).await {
                    error!(error = %err, "failed to send dry-run OnVenueAck to sequencer");
                }
                continue;
            }

            // Dispatch on OrderCore.kind when multi-leg support lands (#29).
            // For now all orders are single-leg and follow the same path.
            let submission = OrderSubmission::from_routed(&order);

            match adapter.submit_order(submission).await {
                Ok(ack) => {
                    info!(
                        order_id = %order_id,
                        venue_order_id = %ack.venue_order_id,
                        "venue accepted order"
                    );
                    let cmd = SequencerCommand::OnVenueAck {
                        order_id,
                        venue_order_id: ack.venue_order_id,
                        submitted_at: ack.accepted_at,
                    };
                    if let Err(err) = self.sequencer_handle.send(cmd).await {
                        error!(error = %err, "failed to send OnVenueAck to sequencer");
                    }
                }
                Err(err) => {
                    let reason = err.to_string();
                    warn!(order_id = %order_id, reason = %reason, "venue rejected order");
                    let cmd = SequencerCommand::OnVenueSubmitFailed { order_id, reason };
                    if let Err(err) = self.sequencer_handle.send(cmd).await {
                        error!(error = %err, "failed to send OnVenueSubmitFailed to sequencer");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::time::{timeout, Duration as TokioDuration};

    struct StaticPriceSource {
        prices: HashMap<InstrumentId, types::PriceSnapshot>,
        stale: bool,
    }

    impl types::PriceSource for StaticPriceSource {
        fn latest_price(&self, instrument: &InstrumentId) -> Option<types::PriceSnapshot> {
            self.prices.get(instrument).cloned()
        }

        fn is_stale(&self, _instrument: &InstrumentId, _max_age: Duration) -> bool {
            self.stale
        }
    }

    fn connected_health(venue_id: &str, now: DateTime<Utc>) -> VenueConnectionHealth {
        VenueConnectionHealth {
            venue_id: VenueId(venue_id.to_owned()),
            state: ConnectionState::Connected,
            last_message_at: Some(now),
            last_heartbeat_at: Some(now),
            heartbeat_latency_ms: Some(25),
            reconnect_count: 0,
            connected_since: Some(now),
        }
    }

    fn route_state(
        venue_id: &str,
        available_balance: Decimal,
        now: DateTime<Utc>,
    ) -> VenueRouteState {
        VenueRouteState {
            connection_health: connected_health(venue_id, now),
            available_balance,
        }
    }

    fn sample_signal(now: DateTime<Utc>) -> types::Signal {
        types::Signal {
            id: types::SignalId("sig-1".into()),
            basket_id: Some(types::BasketId("basket-1".into())),
            agent: types::AgentId("athena".into()),
            instrument: types::InstrumentId("BTC".into()),
            direction: types::Side::Buy,
            conviction: Decimal::new(50, 2),
            sizing_hint: None,
            arb_type: "directional".into(),
            stop_loss: Decimal::new(9900, 0),
            take_profit: Some(Decimal::new(10250, 0)),
            thesis: "follow breakout".into(),
            urgency: types::Urgency::High,
            metadata: serde_json::json!({}),
            created_at: now,
        }
    }

    fn sample_firm_book(now: DateTime<Utc>) -> types::FirmBook {
        types::FirmBook {
            nav: Decimal::new(100_000, 0),
            gross_notional: Decimal::ZERO,
            net_notional: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            daily_pnl: Decimal::ZERO,
            hourly_pnl: Decimal::ZERO,
            current_drawdown_pct: Decimal::ZERO,
            allocated_capital: Decimal::new(40_000, 0),
            cash_available: Decimal::new(60_000, 0),
            total_fees_paid: Decimal::ZERO,
            agent_allocations: HashMap::from([(
                types::AgentId("athena".into()),
                types::AgentAllocation {
                    allocated_capital: Decimal::new(20_000, 0),
                    used_capital: Decimal::ZERO,
                    free_capital: Decimal::new(20_000, 0),
                    realized_pnl: Decimal::ZERO,
                    unrealized_pnl: Decimal::ZERO,
                    current_drawdown: Decimal::ZERO,
                    max_drawdown_limit: Decimal::new(15, 2),
                    status: types::AgentStatus::Active,
                },
            )]),
            instruments: Vec::new(),
            prediction_exposure: types::PredictionExposureSummary {
                total_notional: Decimal::ZERO,
                pct_of_nav: Decimal::ZERO,
                unresolved_markets: 0,
            },
            as_of: now,
        }
    }

    fn sample_limits() -> types::AgentRiskLimits {
        types::AgentRiskLimits {
            allocated_capital: Decimal::new(20_000, 0),
            max_position_notional: Decimal::new(50_000, 0),
            max_gross_notional: Decimal::new(75_000, 0),
            max_drawdown_pct: Decimal::new(15, 2),
            max_daily_loss: Decimal::new(5_000, 0),
            max_open_orders: 20,
            allowed_instruments: None,
            allowed_venues: Some(vec![VenueId("bybit".into()), VenueId("binance".into())]),
        }
    }

    fn sample_prices(now: DateTime<Utc>) -> StaticPriceSource {
        StaticPriceSource {
            prices: HashMap::from([(
                InstrumentId("BTC".into()),
                types::PriceSnapshot {
                    instrument: InstrumentId("BTC".into()),
                    bid: Decimal::new(9995, 0),
                    ask: Decimal::new(10005, 0),
                    mid: Decimal::new(10000, 0),
                    last: Decimal::new(10001, 0),
                    observed_at: now,
                    received_at: now,
                },
            )]),
            stale: false,
        }
    }

    fn sample_symbol_map() -> SymbolMap {
        let mut symbol_map = SymbolMap::default();
        symbol_map
            .insert(
                VenueId("bybit".into()),
                "BTCUSDT",
                InstrumentId("BTC".into()),
            )
            .unwrap();
        symbol_map
            .insert(
                VenueId("binance".into()),
                "BTCUSDT",
                InstrumentId("BTC".into()),
            )
            .unwrap();
        symbol_map
    }

    #[derive(Debug, Clone)]
    struct StaticFeeModel {
        schedule: types::FeeSchedule,
    }

    impl StaticFeeModel {
        fn zero_fee(venue_id: &str) -> Self {
            Self {
                schedule: types::FeeSchedule {
                    venue_id: VenueId(venue_id.to_owned()),
                    tier: "paper".to_owned(),
                    maker_rate: Decimal::ZERO,
                    taker_rate: Decimal::ZERO,
                    gas_model: types::GasModel::None,
                    instrument_overrides: Vec::new(),
                },
            }
        }
    }

    impl types::FeeModel for StaticFeeModel {
        fn estimate(
            &self,
            _order: &types::OrderCore,
        ) -> std::result::Result<types::FeeEstimate, types::FeeError> {
            Ok(types::FeeEstimate {
                as_maker: Decimal::ZERO,
                as_taker: Decimal::ZERO,
                worst_case: Decimal::ZERO,
                gas_estimate: Decimal::ZERO,
            })
        }

        fn calculate(
            &self,
            _fill: &types::Fill,
        ) -> std::result::Result<types::Fee, types::FeeError> {
            Ok(types::Fee {
                trading_fee: Decimal::ZERO,
                gas_fee: Decimal::ZERO,
                rebate: Decimal::ZERO,
                funding_fee: Decimal::ZERO,
                net: Decimal::ZERO,
            })
        }

        fn schedule(
            &self,
            venue_id: &VenueId,
        ) -> std::result::Result<&types::FeeSchedule, types::FeeError> {
            if venue_id == &self.schedule.venue_id {
                return Ok(&self.schedule);
            }

            Err(types::FeeError::NoSchedule(venue_id.clone()))
        }

        fn update_schedule(&mut self, venue_id: VenueId, schedule: types::FeeSchedule) {
            self.schedule = types::FeeSchedule {
                venue_id,
                ..schedule
            };
        }
    }

    fn sample_orderbook(now: DateTime<Utc>) -> types::OrderbookSnapshot {
        types::OrderbookSnapshot {
            venue_id: VenueId("paper".into()),
            instrument_id: InstrumentId("BTC".into()),
            market_id: MarketId("BTCUSDT".into()),
            bids: vec![
                types::PriceLevel {
                    price: Decimal::new(9_990, 0),
                    qty: Decimal::new(2, 0),
                },
                types::PriceLevel {
                    price: Decimal::new(9_980, 0),
                    qty: Decimal::new(3, 0),
                },
            ],
            asks: vec![
                types::PriceLevel {
                    price: Decimal::new(10_000, 0),
                    qty: Decimal::new(1, 0),
                },
                types::PriceLevel {
                    price: Decimal::new(10_010, 0),
                    qty: Decimal::new(3, 0),
                },
            ],
            observed_at: now,
            received_at: now,
        }
    }

    fn sample_order_core(
        now: DateTime<Utc>,
        order_type: types::OrderType,
        side: types::Side,
        qty: Decimal,
        price: Option<Decimal>,
    ) -> types::OrderCore {
        types::OrderCore {
            id: OrderId("ord-paper-1".into()),
            basket_id: None,
            agent_id: types::AgentId("athena".into()),
            instrument_id: InstrumentId("BTC".into()),
            market_id: MarketId("BTCUSDT".into()),
            venue_id: VenueId("paper".into()),
            side,
            order_type,
            time_in_force: types::TimeInForce::GTC,
            qty,
            price,
            trigger_price: None,
            stop_loss: None,
            take_profit: None,
            post_only: false,
            reduce_only: false,
            dry_run: true,
            exec_hint: types::ExecHint::default(),
            created_at: now,
        }
    }

    fn sample_submission(
        client_order_id: &str,
        order_type: types::OrderType,
        side: types::Side,
        qty: Decimal,
        price: Option<Decimal>,
        time_in_force: types::TimeInForce,
    ) -> OrderSubmission {
        OrderSubmission {
            order_id: OrderId(format!("ord-{client_order_id}")),
            client_order_id: ClientOrderId(client_order_id.to_owned()),
            agent_id: types::AgentId("athena".into()),
            instrument_id: InstrumentId("BTC".into()),
            market_id: MarketId("BTCUSDT".into()),
            venue_id: VenueId("paper".into()),
            side,
            order_type,
            time_in_force,
            qty,
            price,
            trigger_price: None,
            stop_loss: None,
            take_profit: None,
            post_only: false,
            reduce_only: false,
            dry_run: true,
            exec_hint: types::ExecHint::default(),
        }
    }

    #[test]
    fn signal_bridge_sizes_from_conviction_and_stop_distance() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let signal = sample_signal(now);
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let orders = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[
                    route_state("bybit", Decimal::new(30_000, 0), now),
                    route_state("binance", Decimal::new(15_000, 0), now),
                ],
                now,
            )
            .unwrap();

        assert_eq!(orders.len(), 1);
        let order = orders.first().unwrap().core();
        let expected_qty = Decimal::new(10_000, 0) / Decimal::new(105, 0);
        assert_eq!(order.venue_id, VenueId("bybit".into()));
        assert_eq!(order.market_id, MarketId("BTCUSDT".into()));
        assert_eq!(order.basket_id, signal.basket_id);
        assert_eq!(order.price, Some(Decimal::new(10005, 0)));
        assert_eq!(order.qty, expected_qty);
    }

    #[test]
    fn signal_bridge_uses_sizing_hint_and_routes_to_funded_allowed_venue() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.sizing_hint = Some(Decimal::new(12_000, 0));
        let firm_book = sample_firm_book(now);
        let mut limits = sample_limits();
        limits.allowed_venues = Some(vec![VenueId("binance".into())]);
        let prices = sample_prices(now);

        let orders = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[
                    route_state("bybit", Decimal::new(50_000, 0), now),
                    route_state("binance", Decimal::new(20_000, 0), now),
                ],
                now,
            )
            .unwrap();

        let order = orders.first().unwrap().core();
        let expected_qty = Decimal::new(12_000, 0) / Decimal::new(10_005, 0);
        assert_eq!(order.venue_id, VenueId("binance".into()));
        assert_eq!(order.market_id, MarketId("BTCUSDT".into()));
        assert_eq!(order.qty, expected_qty);
    }

    #[test]
    fn signal_bridge_returns_empty_for_zero_conviction_without_hint() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.conviction = Decimal::ZERO;
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let orders = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap();

        assert!(orders.is_empty());
    }

    #[test]
    fn signal_bridge_rejects_invalid_conviction() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.conviction = Decimal::new(101, 2);
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[test]
    fn signal_bridge_rejects_zero_conviction_with_sizing_hint() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.conviction = Decimal::ZERO;
        signal.sizing_hint = Some(Decimal::new(1_000, 0));
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[test]
    fn signal_bridge_rejects_sizing_hint_above_allocation() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.sizing_hint = Some(Decimal::new(25_000, 0));
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[test]
    fn signal_bridge_rejects_instrument_outside_agent_scope() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let signal = sample_signal(now);
        let firm_book = sample_firm_book(now);
        let mut limits = sample_limits();
        limits.allowed_instruments = Some(vec![InstrumentId("ETH".into())]);
        let prices = sample_prices(now);

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::RoutingUnavailable { .. }));
    }

    #[test]
    fn signal_bridge_rejects_take_profit_on_wrong_side_of_entry() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.take_profit = Some(Decimal::new(9_900, 0));
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[test]
    fn signal_bridge_rejects_stop_loss_on_wrong_side_of_entry() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let mut signal = sample_signal(now);
        signal.stop_loss = Decimal::new(10_100, 0);
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[test]
    fn signal_bridge_rejects_stale_prices() {
        let now = Utc::now();
        let bridge =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let signal = sample_signal(now);
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let mut prices = sample_prices(now);
        prices.stale = true;

        let err = bridge
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap_err();

        assert!(matches!(err, GatewayError::StalePrice { .. }));
    }

    #[test]
    fn signal_bridge_order_ids_are_deterministic_for_signal_replays() {
        let now = Utc::now();
        let bridge_a =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let bridge_b =
            SignalOrderBridge::new(SignalOrderBridgeConfig::default(), sample_symbol_map());
        let signal = sample_signal(now);
        let firm_book = sample_firm_book(now);
        let limits = sample_limits();
        let prices = sample_prices(now);

        let order_a = bridge_a
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap()
            .pop()
            .unwrap();
        let order_b = bridge_b
            .signal_to_orders(
                &signal,
                &firm_book,
                Some(&limits),
                &prices,
                &[route_state("bybit", Decimal::new(30_000, 0), now)],
                now,
            )
            .unwrap()
            .pop()
            .unwrap();

        assert_eq!(order_a.core().id, OrderId("ord-sig-1".into()));
        assert_eq!(order_a.core().id, order_b.core().id);
    }

    #[test]
    fn fill_simulator_walks_orderbook_depth_for_market_buy() {
        let now = Utc::now();
        let simulator = LiveOrderbookFillSimulator;
        let fee_model = StaticFeeModel::zero_fee("paper");
        let book = sample_orderbook(now);
        let order = sample_order_core(
            now,
            types::OrderType::Market,
            types::Side::Buy,
            Decimal::new(2, 0),
            None,
        );

        let outcome = simulator
            .simulate(
                &order,
                &VenueOrderId("paper-athena-1".into()),
                &book,
                &fee_model,
            )
            .unwrap();

        let simulated_fill = match outcome {
            FillSimulationOutcome::Executed(fill) => fill,
            FillSimulationOutcome::Resting => panic!("market order should execute"),
        };
        assert_eq!(simulated_fill.fill.qty, Decimal::new(2, 0));
        assert_eq!(simulated_fill.fill.price, Decimal::new(10_005, 0));
        assert_eq!(simulated_fill.slippage_bps, Decimal::new(5, 0));
        assert_eq!(simulated_fill.market_impact_bps, Decimal::new(10, 0));
        assert!(simulated_fill.is_complete());
    }

    #[test]
    fn fill_simulator_returns_resting_for_non_marketable_limit() {
        let now = Utc::now();
        let simulator = LiveOrderbookFillSimulator;
        let fee_model = StaticFeeModel::zero_fee("paper");
        let book = sample_orderbook(now);
        let order = sample_order_core(
            now,
            types::OrderType::Limit,
            types::Side::Buy,
            Decimal::new(1, 0),
            Some(Decimal::new(9_995, 0)),
        );

        let outcome = simulator
            .simulate(
                &order,
                &VenueOrderId("paper-athena-2".into()),
                &book,
                &fee_model,
            )
            .unwrap();

        assert!(matches!(outcome, FillSimulationOutcome::Resting));
    }

    #[tokio::test]
    async fn paper_adapter_emits_simulated_fill_for_dry_run_submission() {
        let now = Utc::now();
        let fee_model: Arc<dyn types::FeeModel> = Arc::new(StaticFeeModel::zero_fee("paper"));
        let simulator: Arc<dyn FillSimulator> = Arc::new(LiveOrderbookFillSimulator);
        let mut adapter = PaperAdapter::new(
            PaperAdapterConfig::new(VenueId("paper".into())),
            simulator,
            fee_model,
        );
        adapter.connect().await.unwrap();
        adapter
            .replace_orderbook(sample_orderbook(now))
            .await
            .unwrap();

        let mut fills = adapter.subscribe_fills().await.unwrap();
        let submission = sample_submission(
            "athena-1",
            types::OrderType::Market,
            types::Side::Buy,
            Decimal::new(2, 0),
            None,
            types::TimeInForce::IOC,
        );

        let ack = adapter.submit_order(submission).await.unwrap();
        let fill = timeout(TokioDuration::from_secs(1), fills.recv())
            .await
            .expect("fill should arrive")
            .expect("fill channel should stay open");

        assert_eq!(ack.venue_order_id, fill.venue_order_id);
        assert_eq!(fill.qty, Decimal::new(2, 0));
        assert_eq!(fill.price, Decimal::new(10_005, 0));
        assert!(adapter.query_open_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn paper_adapter_tracks_resting_limit_orders() {
        let now = Utc::now();
        let fee_model: Arc<dyn types::FeeModel> = Arc::new(StaticFeeModel::zero_fee("paper"));
        let simulator: Arc<dyn FillSimulator> = Arc::new(LiveOrderbookFillSimulator);
        let mut adapter = PaperAdapter::new(
            PaperAdapterConfig::new(VenueId("paper".into())),
            simulator,
            fee_model,
        );
        adapter.connect().await.unwrap();
        adapter
            .replace_orderbook(sample_orderbook(now))
            .await
            .unwrap();

        let submission = sample_submission(
            "athena-2",
            types::OrderType::Limit,
            types::Side::Buy,
            Decimal::new(1, 0),
            Some(Decimal::new(9_995, 0)),
            types::TimeInForce::GTC,
        );

        let ack = adapter.submit_order(submission).await.unwrap();
        let open_orders = adapter.query_open_orders().await.unwrap();

        assert_eq!(open_orders.len(), 1);
        assert_eq!(open_orders[0].venue_order_id, ack.venue_order_id);
        assert_eq!(open_orders[0].state, OrderState::Accepted);
    }
}

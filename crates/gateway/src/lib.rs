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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
}

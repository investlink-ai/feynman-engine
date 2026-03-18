//! gateway — VenueAdapter, ExecutionGateway, pipeline stages, and validation.
//!
//! Orchestrates order processing through a composable pipeline:
//! Validate -> CircuitBreaker -> RiskGate -> Route -> Submit.
//!
//! All venue operations are timeout-aware via `TimeoutPolicy`.
//! Connection health is tracked via `VenueConnectionHealth`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::fmt;
use std::time::Duration;

pub use types::{
    ClientOrderId, ConnectionState, HeartbeatConfig, InstrumentId, OrderId, OrderState,
    PipelineOrder, RiskChecked, TimeoutPolicy, VenueConnectionHealth, VenueId, VenueOrderId,
};

/// Typed errors for gateway operations (thiserror for libs).
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("venue {venue_id} is not connected (state: {state})")]
    VenueNotConnected { venue_id: String, state: String },

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

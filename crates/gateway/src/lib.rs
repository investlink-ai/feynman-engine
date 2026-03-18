//! gateway — ExecutionGateway, PipelineStage, and OrderValidator traits.
//!
//! Orchestrates order processing through a composable pipeline:
//! Validate → CircuitBreaker → RiskGate → Emulate → Route → Submit.
//!
//! - ExecutionGateway: main entry point for order submission (idempotent on OrderId)
//! - PipelineStage: composable processing stages
//! - OrderValidator: fast, stateless pre-validation before risk gate

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::fmt;

pub use types::{OrderId, VenueId};

/// Placeholder types that will be replaced by actual types from types crate.
#[derive(Debug, Clone)]
pub struct CanonicalOrder;
#[derive(Debug, Clone)]
pub struct Fill;
#[derive(Debug, Clone)]
pub struct VenueCapabilities;
#[derive(Debug, Clone)]
pub struct RiskApproval;

/// Result type for gateway operations.
pub type Result<T> = std::result::Result<T, anyhow::Error>;

/// ─── Execution Gateway ───
///
/// Main entry point for order submission. Orchestrates the full pipeline.
/// Idempotent on `OrderId` — duplicate submissions return cached acknowledgment.
#[async_trait::async_trait]
pub trait ExecutionGateway: Send + Sync {
    /// Submit order through the full pipeline:
    /// Validate → CircuitBreaker → RiskGate → Emulate (if needed) → Router → Adapter
    ///
    /// Idempotent on order.id — if the same OrderId is submitted multiple times
    /// (e.g., on retry or restart), only one execution occurs; subsequent calls
    /// return the cached `OrderAck`.
    async fn submit(&self, order: CanonicalOrder) -> Result<OrderAck>;

    /// Cancel an open order.
    async fn cancel(&self, order_id: &OrderId) -> Result<()>;

    /// Amend an open order (if venue supports it).
    /// Returns error if venue doesn't support amendments.
    async fn amend(
        &self,
        order_id: &OrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()>;

    /// Emergency: cancel all open orders across all venues.
    /// Used during risk breach or manual halt.
    async fn cancel_all(&self) -> Result<u32>;

    /// Get order by internal OrderId.
    /// Returns None if order not found.
    async fn get_order(&self, order_id: &OrderId) -> Result<Option<OrderRecord>>;

    /// Get all open orders across all venues.
    async fn open_orders(&self) -> Result<Vec<OrderRecord>>;

    /// Receive fill stream. Called once at startup to subscribe to fills.
    /// (Implementation will use appropriate async channel from tokio)
    fn fill_stream(&self) -> std::sync::mpsc::Receiver<Fill>;
}

/// Acknowledgment returned after order submission.
#[derive(Debug, Clone)]
pub struct OrderAck {
    /// Internal order ID
    pub order_id: OrderId,
    /// Current state of the order
    pub state: OrderState,
    /// Risk gate approval (if passed)
    pub risk_approval: Option<RiskApproval>,
    /// Which execution strategy was selected
    pub strategy_selected: String,
    /// When order was accepted
    pub accepted_at: DateTime<Utc>,
}

/// Current state of an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderState {
    /// Accepted, waiting for submission
    Pending,
    /// Submitted to venue
    Submitted { venue_order_id: String, submitted_at: DateTime<Utc> },
    /// Partially filled
    PartiallyFilled { filled_qty: Decimal, remaining_qty: Decimal },
    /// Fully filled
    Filled { filled_qty: Decimal },
    /// Cancelled (user or engine)
    Cancelled { reason: String, cancelled_at: DateTime<Utc> },
    /// Rejected by risk gate or circuit breaker
    Rejected { reason: String, rejected_at: DateTime<Utc> },
}

/// Full order record with history.
#[derive(Debug, Clone)]
pub struct OrderRecord {
    /// The original order
    pub order: CanonicalOrder,
    /// Current state
    pub state: OrderState,
    /// All fills executed against this order
    pub fills: Vec<Fill>,
    /// Audit trail of events
    pub events: Vec<Event>,
    /// When order was created
    pub created_at: DateTime<Utc>,
    /// When order was last modified
    pub last_updated: DateTime<Utc>,
}

/// Audit event for order lifecycle.
#[derive(Debug, Clone)]
pub struct Event {
    /// Event description
    pub message: String,
    /// When event occurred
    pub timestamp: DateTime<Utc>,
}

/// ─── Pipeline Stage ───
///
/// Composable processing stage. Stages are executed in order;
/// first rejection stops the pipeline.
///
/// Default pipeline: [Validator, CircuitBreaker, RiskGate, Emulator, Router]
/// Additional stages (e.g., fee optimizer, order dedup) can be inserted.
#[async_trait::async_trait]
pub trait PipelineStage: Send + Sync {
    /// Name of this stage (for logging/debugging)
    fn name(&self) -> &str;

    /// Process order through this stage.
    /// Returns:
    /// - `Ok(order)` — pass to next stage (may be modified)
    /// - `Err(reason)` — reject and stop pipeline
    async fn process(&self, order: CanonicalOrder) -> Result<CanonicalOrder>;
}

/// ─── Order Validator ───
///
/// Fast, stateless validation before order enters the pipeline.
/// Rejects orders that are structurally invalid regardless of current state.
/// Runs before risk gate.
pub trait OrderValidator: Send + Sync {
    /// Validate order structure and venue capability compatibility.
    /// Returns `Ok(())` if valid, or `Err(errors)` with list of violations.
    fn validate(
        &self,
        order: &CanonicalOrder,
        caps: &VenueCapabilities,
    ) -> std::result::Result<(), Vec<ValidationError>>;
}

/// Validation error returned by OrderValidator.
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// Quantity is zero, negative, or below venue minimum lot size
    InvalidQty { reason: String },
    /// Price is negative, outside tick size, or invalid
    InvalidPrice { reason: String },
    /// Venue doesn't support this order type
    UnsupportedOrderType { requested: String, venue: VenueId },
    /// Venue doesn't support this time-in-force
    UnsupportedTIF {
        requested: String,
        venue: VenueId,
    },
    /// Required field is missing
    MissingRequiredField { field: String },
    /// Incompatible constraint combination (e.g., post_only + market)
    IncompatibleConstraints { reason: String },
    /// Precision exceeds venue's decimal places
    PrecisionExceeded {
        field: String,
        max_decimals: u32,
        actual: u32,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::InvalidQty { reason } => write!(f, "Invalid quantity: {}", reason),
            ValidationError::InvalidPrice { reason } => write!(f, "Invalid price: {}", reason),
            ValidationError::UnsupportedOrderType { requested, venue } => {
                write!(f, "Order type {} not supported on {}", requested, venue.0)
            }
            ValidationError::UnsupportedTIF { requested, venue } => {
                write!(f, "TIF {} not supported on {}", requested, venue.0)
            }
            ValidationError::MissingRequiredField { field } => {
                write!(f, "Missing required field: {}", field)
            }
            ValidationError::IncompatibleConstraints { reason } => {
                write!(f, "Incompatible constraints: {}", reason)
            }
            ValidationError::PrecisionExceeded {
                field,
                max_decimals,
                actual,
            } => {
                write!(
                    f,
                    "Field {} precision exceeded: max {}, got {}",
                    field, max_decimals, actual
                )
            }
        }
    }
}

impl std::error::Error for ValidationError {}

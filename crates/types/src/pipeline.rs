//! Type-state pipeline — compile-time enforcement of order processing stages.
//!
//! An order must pass through stages in order: `Draft → Validated → RiskChecked → Routed`.
//! Each transition consumes the previous stage, making it impossible to submit
//! an un-risk-checked order to a venue.
//!
//! The types crate defines the structure and transitions. The actual logic
//! (validation, risk checking, routing) lives in the respective crates.

use std::marker::PhantomData;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{
    AgentId, ClientOrderId, InstrumentId, OrderId, RiskCheckResult, RiskViolation, Side, VenueId,
    VenueOrderId,
};

// ─── Order classification ───

/// Order type determines execution semantics at the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderType {
    /// Execute immediately at best available price.
    Market,
    /// Execute at specified price or better.
    Limit,
    /// Becomes a market order when trigger price is reached.
    StopMarket,
    /// Becomes a limit order when trigger price is reached.
    StopLimit,
    /// Trailing stop with dynamic trigger.
    TrailingStop,
}

impl std::fmt::Display for OrderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Market => write!(f, "market"),
            Self::Limit => write!(f, "limit"),
            Self::StopMarket => write!(f, "stop_market"),
            Self::StopLimit => write!(f, "stop_limit"),
            Self::TrailingStop => write!(f, "trailing_stop"),
        }
    }
}

/// Time-in-force determines how long an order remains active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good 'til cancelled (persists until filled or manually cancelled).
    GTC,
    /// Immediate-or-cancel (fill what you can, cancel the rest).
    IOC,
    /// Fill-or-kill (fill entirely or cancel entirely).
    FOK,
    /// Good 'til date (expires at the specified time).
    GTD { expire_at: DateTime<Utc> },
}

impl std::fmt::Display for TimeInForce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GTC => write!(f, "GTC"),
            Self::IOC => write!(f, "IOC"),
            Self::FOK => write!(f, "FOK"),
            Self::GTD { expire_at } => write!(f, "GTD({})", expire_at),
        }
    }
}

/// Execution hints — controls how the engine handles order submission.
///
/// These flags are opt-in. `ExecHint::default()` is the safe, conservative default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecHint {
    /// Allow the engine to emulate unsupported order types.
    /// When `false` (default), submission fails if the venue doesn't natively support the order type.
    /// When `true`, the engine may emulate (e.g., synthetic stop via polling).
    pub allow_emulation: bool,
}

// ─── Stage markers (sealed) ───

/// Prevents external crates from implementing `PipelineStage`.
mod sealed {
    pub trait Stage {}
    impl Stage for super::Draft {}
    impl Stage for super::Validated {}
    impl Stage for super::RiskChecked {}
    impl Stage for super::Routed {}
}

/// Trait bound for generic code that accepts any pipeline stage.
pub trait PipelineStage: sealed::Stage {}

/// Order has been constructed but not yet validated.
#[derive(Debug, Clone)]
pub struct Draft;
/// Order has passed structural validation (fields, precision, venue support).
#[derive(Debug, Clone)]
pub struct Validated {
    validated_at: DateTime<Utc>,
}
/// Order has passed circuit breaker and risk gate checks.
#[derive(Debug, Clone)]
pub struct RiskChecked {
    validated_at: DateTime<Utc>,
    approval: RiskApprovalStamp,
}
/// Order has been assigned a venue route and is ready for submission.
#[derive(Debug, Clone)]
pub struct Routed {
    validated_at: DateTime<Utc>,
    approval: RiskApprovalStamp,
    routing: RoutingAssignment,
}

impl PipelineStage for Draft {}
impl PipelineStage for Validated {}
impl PipelineStage for RiskChecked {}
impl PipelineStage for Routed {}

// ─── Core order data (always present) ───

/// Immutable core fields present at every pipeline stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderCore {
    pub id: OrderId,
    pub agent_id: AgentId,
    pub instrument_id: InstrumentId,
    pub venue_id: VenueId,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    /// Trigger price for stop orders. Required when `order_type` is `StopMarket` or `StopLimit`.
    pub trigger_price: Option<Decimal>,
    pub stop_loss: Option<Decimal>,
    pub take_profit: Option<Decimal>,
    /// Post-only flag: reject if order would take liquidity.
    pub post_only: bool,
    /// Reduce-only flag: order can only reduce an existing position.
    pub reduce_only: bool,
    pub dry_run: bool,
    pub exec_hint: ExecHint,
    pub created_at: DateTime<Utc>,
}

// ─── Pipeline order ───

/// An order at a specific stage in the processing pipeline.
///
/// The stage parameter `S` carries data accumulated during processing.
/// Transitions consume `self` and produce the next stage, making it
/// impossible to skip stages at compile time.
///
/// Not `Clone` — transitions are consuming. Extract `core` if you need
/// to store order data before consuming.
#[derive(Debug)]
pub struct PipelineOrder<S: PipelineStage> {
    /// Core order fields (immutable across stages).
    pub core: OrderCore,
    stage: S,
    _marker: PhantomData<S>,
}

// ─── Stage-specific data ───

/// Proof that risk checks passed. Carried forward through subsequent stages.
///
/// Uses typed `RiskCheckResult` and `RiskViolation` instead of raw strings
/// for structured analysis, monitoring, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskApprovalStamp {
    pub approved_at: DateTime<Utc>,
    pub checks_performed: Vec<RiskCheckResult>,
    pub warnings: Vec<RiskViolation>,
}

/// Venue assignment and client order ID for submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingAssignment {
    pub venue_id: VenueId,
    pub client_order_id: ClientOrderId,
    pub routed_at: DateTime<Utc>,
}

// ─── Constructors & transitions ───

impl PipelineOrder<Draft> {
    /// Create a new draft order. This is the only entry point into the pipeline.
    #[must_use]
    pub fn new(core: OrderCore) -> Self {
        Self {
            core,
            stage: Draft,
            _marker: PhantomData,
        }
    }

    /// Transition: Draft → Validated.
    ///
    /// Called after structural validation passes (fields, precision, venue support).
    /// The validation logic itself lives in `gateway::OrderValidator`.
    #[must_use]
    pub fn into_validated(self, now: DateTime<Utc>) -> PipelineOrder<Validated> {
        PipelineOrder {
            core: self.core,
            stage: Validated { validated_at: now },
            _marker: PhantomData,
        }
    }
}

impl PipelineOrder<Validated> {
    /// When this order was validated.
    #[must_use]
    pub fn validated_at(&self) -> DateTime<Utc> {
        self.stage.validated_at
    }

    /// Transition: Validated → RiskChecked.
    ///
    /// Called after circuit breaker and risk gate both approve.
    /// The risk logic itself lives in `risk::CircuitBreaker` and `risk::RiskGate`.
    #[must_use]
    pub fn into_risk_checked(self, approval: RiskApprovalStamp) -> PipelineOrder<RiskChecked> {
        PipelineOrder {
            core: self.core,
            stage: RiskChecked {
                validated_at: self.stage.validated_at,
                approval,
            },
            _marker: PhantomData,
        }
    }
}

impl PipelineOrder<RiskChecked> {
    /// When this order was validated.
    #[must_use]
    pub fn validated_at(&self) -> DateTime<Utc> {
        self.stage.validated_at
    }

    /// Access the risk approval stamp.
    #[must_use]
    pub fn risk_approval(&self) -> &RiskApprovalStamp {
        &self.stage.approval
    }

    /// Transition: RiskChecked → Routed.
    ///
    /// Called after venue selection confirms availability and client order ID is generated.
    #[must_use]
    pub fn into_routed(self, routing: RoutingAssignment) -> PipelineOrder<Routed> {
        PipelineOrder {
            core: self.core,
            stage: Routed {
                validated_at: self.stage.validated_at,
                approval: self.stage.approval,
                routing,
            },
            _marker: PhantomData,
        }
    }
}

impl PipelineOrder<Routed> {
    /// When this order was validated.
    #[must_use]
    pub fn validated_at(&self) -> DateTime<Utc> {
        self.stage.validated_at
    }

    /// Access the risk approval stamp.
    #[must_use]
    pub fn risk_approval(&self) -> &RiskApprovalStamp {
        &self.stage.approval
    }

    /// Access routing assignment (venue, client_order_id).
    #[must_use]
    pub fn routing(&self) -> &RoutingAssignment {
        &self.stage.routing
    }

    /// Venue this order is routed to.
    #[must_use]
    pub fn venue_id(&self) -> &VenueId {
        &self.stage.routing.venue_id
    }

    /// Client order ID for venue submission.
    #[must_use]
    pub fn client_order_id(&self) -> &ClientOrderId {
        &self.stage.routing.client_order_id
    }
}

/// Shared accessors available at all stages.
impl<S: PipelineStage> PipelineOrder<S> {
    /// Borrow the immutable core order data.
    #[must_use]
    pub fn core(&self) -> &OrderCore {
        &self.core
    }
}

// ─── Order state (post-submission lifecycle) ───

/// Runtime state of an order after it enters the engine.
///
/// Unlike the pipeline stages (compile-time), the post-submission lifecycle is
/// runtime because transitions depend on external venue responses.
///
/// Exhaustive match required — no `_` wildcard on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderState {
    /// Accepted by engine, pending risk check.
    Pending,
    /// Sent to venue, awaiting acknowledgment.
    Submitted,
    /// Venue acknowledged receipt.
    Accepted,
    /// Partially filled (some quantity executed).
    PartiallyFilled,
    /// Fully filled.
    Filled,
    /// Cancelled (by engine, agent, or venue).
    Cancelled,
    /// Rejected by venue or risk gate.
    Rejected,
    /// Expired (TIF expired on venue).
    Expired,
}

impl OrderState {
    /// Is this a terminal state (no further transitions possible)?
    #[must_use]
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Pending => false,
            Self::Submitted => false,
            Self::Accepted => false,
            Self::PartiallyFilled => false,
            Self::Filled => true,
            Self::Cancelled => true,
            Self::Rejected => true,
            Self::Expired => true,
        }
    }

    /// Is the order active on a venue (may receive fills)?
    #[must_use]
    pub fn is_active(self) -> bool {
        match self {
            Self::Pending => false,
            Self::Submitted => true,
            Self::Accepted => true,
            Self::PartiallyFilled => true,
            Self::Filled => false,
            Self::Cancelled => false,
            Self::Rejected => false,
            Self::Expired => false,
        }
    }
}

impl std::fmt::Display for OrderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Submitted => write!(f, "submitted"),
            Self::Accepted => write!(f, "accepted"),
            Self::PartiallyFilled => write!(f, "partially_filled"),
            Self::Filled => write!(f, "filled"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Rejected => write!(f, "rejected"),
            Self::Expired => write!(f, "expired"),
        }
    }
}

// ─── Fill (unified across crates) ───

/// A fill received from a venue, enriched with engine-side metadata.
///
/// Defined here (not in gateway) because multiple crates reference fills:
/// engine-core, risk, observability, event sourcing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub venue_order_id: VenueOrderId,
    pub instrument_id: InstrumentId,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    pub fee: Decimal,
    pub filled_at: DateTime<Utc>,
    pub is_maker: bool,
}

// ─── Live Order (post-submission) ───

/// An order that has been submitted to a venue.
///
/// Bridges the compile-time type-state pipeline to the runtime venue lifecycle.
/// Created by consuming a `PipelineOrder<Routed>` — the type system guarantees
/// the order passed validation, risk checks, and routing before reaching this point.
///
/// State transitions are validated at runtime via `VenueState::try_transition()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveOrder {
    pub core: OrderCore,
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<VenueOrderId>,
    pub venue_state: VenueState,
    pub filled_qty: Decimal,
    pub remaining_qty: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub fills: Vec<Fill>,
    pub submitted_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}

impl LiveOrder {
    /// Create a `LiveOrder` from a routed pipeline order at submission time.
    #[must_use]
    pub fn from_routed(order: PipelineOrder<Routed>, now: DateTime<Utc>) -> Self {
        let qty = order.core.qty;
        let client_order_id = order.client_order_id().clone();
        Self {
            core: order.core,
            client_order_id,
            venue_order_id: None,
            venue_state: VenueState::Submitted,
            filled_qty: Decimal::ZERO,
            remaining_qty: qty,
            avg_fill_price: None,
            fills: Vec::new(),
            submitted_at: now,
            last_updated: now,
        }
    }

    /// Venue acknowledged the order.
    pub fn on_accepted(
        &mut self,
        venue_order_id: VenueOrderId,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), VenueStateError> {
        self.venue_state = self.venue_state.try_transition(VenueState::Accepted)?;
        self.venue_order_id = Some(venue_order_id);
        self.last_updated = now;
        Ok(())
    }

    /// Process a fill from the venue.
    pub fn on_fill(
        &mut self,
        fill: Fill,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), VenueStateError> {
        // Update average fill price
        let total_filled_notional =
            self.avg_fill_price.unwrap_or(Decimal::ZERO) * self.filled_qty + fill.price * fill.qty;
        self.filled_qty += fill.qty;
        self.remaining_qty -= fill.qty;

        if self.filled_qty > Decimal::ZERO {
            self.avg_fill_price = Some(total_filled_notional / self.filled_qty);
        }

        let next_state = if self.remaining_qty <= Decimal::ZERO {
            VenueState::Filled
        } else {
            VenueState::PartiallyFilled
        };

        self.venue_state = self.venue_state.try_transition(next_state)?;
        self.fills.push(fill);
        self.last_updated = now;
        Ok(())
    }

    /// Order was cancelled.
    pub fn on_cancel(&mut self, now: DateTime<Utc>) -> std::result::Result<(), VenueStateError> {
        self.venue_state = self.venue_state.try_transition(VenueState::Cancelled)?;
        self.last_updated = now;
        Ok(())
    }

    /// Order was rejected by venue.
    pub fn on_reject(&mut self, now: DateTime<Utc>) -> std::result::Result<(), VenueStateError> {
        self.venue_state = self.venue_state.try_transition(VenueState::Rejected)?;
        self.last_updated = now;
        Ok(())
    }

    /// Order expired on venue.
    pub fn on_expire(&mut self, now: DateTime<Utc>) -> std::result::Result<(), VenueStateError> {
        self.venue_state = self.venue_state.try_transition(VenueState::Expired)?;
        self.last_updated = now;
        Ok(())
    }
}

// ─── Venue State FSM ───

/// Runtime state machine for post-submission order lifecycle.
///
/// Transitions are validated — invalid transitions return `VenueStateError`.
/// Exhaustive match required — no `_` wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VenueState {
    /// Sent to venue, awaiting acknowledgment.
    Submitted,
    /// Venue acknowledged receipt.
    Accepted,
    /// Partially filled.
    PartiallyFilled,
    /// Fully filled (terminal).
    Filled,
    /// Cancelled (terminal).
    Cancelled,
    /// Rejected by venue (terminal).
    Rejected,
    /// Expired on venue (terminal).
    Expired,
}

impl VenueState {
    /// Is this a terminal state?
    #[must_use]
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Submitted => false,
            Self::Accepted => false,
            Self::PartiallyFilled => false,
            Self::Filled => true,
            Self::Cancelled => true,
            Self::Rejected => true,
            Self::Expired => true,
        }
    }

    /// Validate and execute a state transition.
    ///
    /// Returns the new state if the transition is valid, or an error describing
    /// why the transition is illegal.
    pub fn try_transition(self, to: Self) -> std::result::Result<Self, VenueStateError> {
        let valid = match (self, to) {
            // From Submitted
            (Self::Submitted, Self::Accepted) => true,
            (Self::Submitted, Self::Rejected) => true,
            (Self::Submitted, Self::Cancelled) => true,
            // Some venues fill without explicit accept
            (Self::Submitted, Self::PartiallyFilled) => true,
            (Self::Submitted, Self::Filled) => true,
            // From Accepted
            (Self::Accepted, Self::PartiallyFilled) => true,
            (Self::Accepted, Self::Filled) => true,
            (Self::Accepted, Self::Cancelled) => true,
            (Self::Accepted, Self::Expired) => true,
            // From PartiallyFilled
            (Self::PartiallyFilled, Self::PartiallyFilled) => true, // more fills
            (Self::PartiallyFilled, Self::Filled) => true,
            (Self::PartiallyFilled, Self::Cancelled) => true,
            // Terminal → anything is invalid
            (Self::Filled, _) => false,
            (Self::Cancelled, _) => false,
            (Self::Rejected, _) => false,
            (Self::Expired, _) => false,
            // All other transitions are invalid
            _ => false,
        };

        if valid {
            Ok(to)
        } else {
            Err(VenueStateError::InvalidTransition { from: self, to })
        }
    }
}

impl std::fmt::Display for VenueState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Submitted => write!(f, "submitted"),
            Self::Accepted => write!(f, "accepted"),
            Self::PartiallyFilled => write!(f, "partially_filled"),
            Self::Filled => write!(f, "filled"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Rejected => write!(f, "rejected"),
            Self::Expired => write!(f, "expired"),
        }
    }
}

/// Error for invalid venue state transitions.
#[derive(Debug, Clone, thiserror::Error)]
pub enum VenueStateError {
    #[error("invalid venue state transition: {from} → {to}")]
    InvalidTransition { from: VenueState, to: VenueState },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_core() -> OrderCore {
        OrderCore {
            id: OrderId("ord-1".into()),
            agent_id: AgentId("satoshi".into()),
            instrument_id: InstrumentId("BTCUSDT".into()),
            venue_id: VenueId("bybit".into()),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            qty: Decimal::new(1, 0),
            price: Some(Decimal::new(65000, 0)),
            trigger_price: None,
            stop_loss: Some(Decimal::new(60000, 0)),
            take_profit: Some(Decimal::new(70000, 0)),
            post_only: false,
            reduce_only: false,
            dry_run: true,
            exec_hint: ExecHint::default(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_pipeline_transitions_compile() {
        let now = Utc::now();
        let draft = PipelineOrder::new(sample_core());

        // Draft → Validated
        let validated = draft.into_validated(now);
        assert_eq!(validated.validated_at(), now);

        // Validated → RiskChecked
        let approval = RiskApprovalStamp {
            approved_at: now,
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
            warnings: vec![],
        };
        let risk_checked = validated.into_risk_checked(approval);
        assert_eq!(risk_checked.risk_approval().checks_performed.len(), 2);

        // RiskChecked → Routed
        let routing = RoutingAssignment {
            venue_id: VenueId("bybit".into()),
            client_order_id: ClientOrderId("satoshi-123-000000-r00".into()),
            routed_at: now,
        };
        let routed = risk_checked.into_routed(routing);
        assert_eq!(routed.venue_id(), &VenueId("bybit".into()));
        assert_eq!(routed.core().side, Side::Buy);
    }

    #[test]
    fn test_order_state_terminal() {
        assert!(!OrderState::Pending.is_terminal());
        assert!(!OrderState::Submitted.is_terminal());
        assert!(!OrderState::Accepted.is_terminal());
        assert!(!OrderState::PartiallyFilled.is_terminal());
        assert!(OrderState::Filled.is_terminal());
        assert!(OrderState::Cancelled.is_terminal());
        assert!(OrderState::Rejected.is_terminal());
        assert!(OrderState::Expired.is_terminal());
    }

    #[test]
    fn test_order_state_active() {
        assert!(!OrderState::Pending.is_active());
        assert!(OrderState::Submitted.is_active());
        assert!(OrderState::Accepted.is_active());
        assert!(OrderState::PartiallyFilled.is_active());
        assert!(!OrderState::Filled.is_active());
    }

    #[test]
    fn test_order_state_display() {
        assert_eq!(
            format!("{}", OrderState::PartiallyFilled),
            "partially_filled"
        );
        assert_eq!(format!("{}", OrderState::Filled), "filled");
    }

    #[test]
    fn test_fill_serde_round_trip() {
        let fill = Fill {
            order_id: OrderId("ord-1".into()),
            venue_order_id: VenueOrderId("v-1".into()),
            instrument_id: InstrumentId("BTCUSDT".into()),
            side: Side::Buy,
            qty: Decimal::new(1, 0),
            price: Decimal::new(65000, 0),
            fee: Decimal::new(13, 2), // 0.13
            filled_at: Utc::now(),
            is_maker: true,
        };
        let json = serde_json::to_string(&fill).unwrap();
        let deser: Fill = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.order_id, fill.order_id);
        assert_eq!(deser.fee, Decimal::new(13, 2));
    }

    #[test]
    fn test_venue_state_valid_transitions() {
        // Submitted → Accepted → PartiallyFilled → Filled
        let s = VenueState::Submitted;
        let s = s.try_transition(VenueState::Accepted).unwrap();
        let s = s.try_transition(VenueState::PartiallyFilled).unwrap();
        let s = s.try_transition(VenueState::Filled).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn test_venue_state_invalid_transition() {
        // Cannot go from Filled (terminal) to Accepted
        let s = VenueState::Filled;
        assert!(s.try_transition(VenueState::Accepted).is_err());
    }

    #[test]
    fn test_venue_state_submit_to_reject() {
        let s = VenueState::Submitted;
        let s = s.try_transition(VenueState::Rejected).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn test_venue_state_partial_to_cancel() {
        // Can cancel a partially filled order
        let s = VenueState::PartiallyFilled;
        let s = s.try_transition(VenueState::Cancelled).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn test_live_order_lifecycle() {
        let now = Utc::now();
        let core = sample_core();
        let draft = PipelineOrder::new(core);
        let validated = draft.into_validated(now);
        let approval = RiskApprovalStamp {
            approved_at: now,
            checks_performed: vec![RiskCheckResult {
                check_name: "CB-all".into(),
                passed: true,
                message: None,
            }],
            warnings: vec![],
        };
        let risk_checked = validated.into_risk_checked(approval);
        let routing = RoutingAssignment {
            venue_id: VenueId("bybit".into()),
            client_order_id: ClientOrderId("satoshi-123-000000-r00".into()),
            routed_at: now,
        };
        let routed = risk_checked.into_routed(routing);

        // Create LiveOrder
        let mut live = LiveOrder::from_routed(routed, now);
        assert_eq!(live.venue_state, VenueState::Submitted);
        assert_eq!(live.filled_qty, Decimal::ZERO);

        // Accept
        live.on_accepted(VenueOrderId("v-ord-1".into()), now)
            .unwrap();
        assert_eq!(live.venue_state, VenueState::Accepted);

        // Partial fill
        let fill = Fill {
            order_id: OrderId("ord-1".into()),
            venue_order_id: VenueOrderId("v-ord-1".into()),
            instrument_id: InstrumentId("BTCUSDT".into()),
            side: Side::Buy,
            qty: Decimal::new(5, 1), // 0.5
            price: Decimal::new(65000, 0),
            fee: Decimal::new(13, 2),
            filled_at: now,
            is_maker: true,
        };
        live.on_fill(fill, now).unwrap();
        assert_eq!(live.venue_state, VenueState::PartiallyFilled);
        assert_eq!(live.filled_qty, Decimal::new(5, 1));

        // Final fill
        let fill2 = Fill {
            order_id: OrderId("ord-1".into()),
            venue_order_id: VenueOrderId("v-ord-1".into()),
            instrument_id: InstrumentId("BTCUSDT".into()),
            side: Side::Buy,
            qty: Decimal::new(5, 1), // 0.5
            price: Decimal::new(65100, 0),
            fee: Decimal::new(13, 2),
            filled_at: now,
            is_maker: false,
        };
        live.on_fill(fill2, now).unwrap();
        assert_eq!(live.venue_state, VenueState::Filled);
        assert!(live.venue_state.is_terminal());
        assert_eq!(live.fills.len(), 2);
    }
}

//! risk — CircuitBreaker (L0) and RiskGate (L1) for the Feynman execution engine.
//!
//! Path-aware checks: universal (all order paths) + signal-specific (`SubmitSignal` only).
//! CircuitBreaker provides hardcoded, compiled-in safety checks (CB-1 through CB-11).
//! AgentRiskManager provides deterministic Layer 1 evaluation.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

pub use types::{
    AgentAllocation, AgentId, AgentRiskLimits, Clock, ConnectionState, Fill, FirmBook,
    FirmRiskLimits, InstrumentId, InstrumentRiskLimits, OrderCore, PipelineOrder,
    PredictionMarketLimits, PriceSource, RiskCheckResult, RiskLimits, RiskOutcome, RiskViolation,
    Side, SimulatedClock, Validated, VenueConnectionHealth, VenueId, VenueRiskLimits,
    ViolationType, WallClock,
};

pub mod circuit_breaker;

/// Typed errors for risk operations (thiserror for libs).
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error("circuit breaker tripped: [{breaker}] {reason}")]
    CircuitBreakerTripped { breaker: String, reason: String },

    #[error("risk gate rejected: {0}")]
    Rejected(String),

    #[error("price data stale for {instrument}: cannot evaluate risk")]
    StalePriceData { instrument: String },

    #[error("agent not found: {0}")]
    AgentNotFound(String),
}

pub type Result<T> = std::result::Result<T, RiskError>;

// ─── Circuit Breaker (Layer 0) ───

/// Sealed trait — external crates cannot add new circuit breakers.
pub(crate) mod sealed {
    pub trait Sealed {}
}

/// Engine execution mode — determines which circuit breaker checks apply (CB-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    Live,
    Paper,
    Backtest,
}

/// The kind of operation being evaluated by the circuit breaker bank.
///
/// `Cancel` operations bypass all circuit breakers — they never increase exposure.
/// `NewOrder` operations are subject to all checks, including latched halts.
/// `ReduceOnly` is a convenience alias recognised by the HaltAll early-returns:
/// the latch still fires for new directional orders, but `ReduceOnly` is allowed
/// through CB-3/4/5 so resting positions can be flattened during a halt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    /// A new order that may increase exposure.
    NewOrder,
    /// A cancel request — never increases exposure, always allowed through.
    Cancel,
    /// A reduce-only order — allowed through latched HaltAll/GrossNotional checks.
    ReduceOnly,
}

/// Thresholds for all 11 compiled-in circuit breakers.
///
/// Not hot-reloadable — changed only via code deploy and review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// CB-1: Max notional for a single order (USD).
    pub max_single_order_notional: Decimal,
    /// CB-2: Max gross notional per instrument across the firm (USD).
    pub max_instrument_position_notional: Decimal,
    /// CB-3: Max firm gross notional across all instruments (USD).
    pub max_firm_gross_notional: Decimal,
    /// CB-4: Max daily loss as a fraction of NAV (e.g. `0.05` = 5%).
    pub max_daily_loss_pct: Decimal,
    /// CB-5: Max hourly loss as a fraction of NAV (e.g. `0.02` = 2%).
    pub max_hourly_loss_pct: Decimal,
    /// CB-6: Max firm-wide new orders in the last 60 seconds.
    pub max_firm_orders_per_min: u32,
    /// CB-7: Max per-agent new orders in the last 60 seconds.
    pub max_agent_orders_per_min: u32,
    /// CB-8: Max venue error rate (errors / total) over the last 5 minutes.
    pub max_venue_error_rate: Decimal,
    /// CB-9: Max seconds since last venue heartbeat before rejecting orders to that venue.
    pub max_venue_disconnect_secs: u64,
    /// CB-11: Max Sequencer command queue depth before rejecting new orders.
    pub max_sequencer_queue_depth: usize,
}

/// Venue error counts over the last 5-minute window for CB-8.
///
/// Populated by the Sequencer from venue adapter callbacks.
#[derive(Debug, Clone, Default)]
pub struct VenueErrorStats {
    pub errors_last_5min: u32,
    pub total_last_5min: u32,
}

/// Inputs to every circuit breaker check call.
///
/// The Sequencer constructs this from live engine state before calling
/// `CircuitBreakerBank::check_all`.
pub struct CircuitBreakerContext<'a> {
    /// The order being evaluated. `None` for latch-state pre-checks and cancels.
    pub order: Option<&'a PipelineOrder<Validated>>,
    pub firm_book: &'a FirmBook,
    /// Current connection health per venue, keyed by `VenueId`.
    pub venue_health: &'a HashMap<VenueId, VenueConnectionHealth>,
    /// Error stats per venue for CB-8, keyed by `VenueId`.
    pub venue_error_stats: &'a HashMap<VenueId, VenueErrorStats>,
    /// Firm-wide orders submitted in the last 60 seconds for CB-6.
    pub firm_orders_last_60s: u32,
    /// Per-agent orders submitted in the last 60 seconds for CB-7.
    pub agent_orders_last_60s: &'a HashMap<AgentId, u32>,
    /// Current Sequencer command queue depth for CB-11.
    pub sequencer_queue_depth: usize,
    /// Current engine execution mode for CB-10.
    pub execution_mode: ExecutionMode,
    /// Kind of operation being evaluated — determines which circuit breakers apply.
    pub operation_kind: OperationKind,
    pub config: &'a CircuitBreakerConfig,
    pub clock: &'a dyn Clock,
}

/// Result of a single circuit breaker check.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum CircuitBreakerResult {
    Pass,
    /// Order rejected by this breaker — fills and cancels still processed.
    Reject {
        breaker: &'static str,
        reason: String,
    },
    /// Full trading halt — new orders blocked until manually reset.
    /// Cancels and `ReduceOnly` orders are allowed through so resting positions
    /// can be flattened; `NewOrder` operations are hard-blocked.
    HaltAll {
        breaker: &'static str,
        reason: String,
    },
}

/// Reason a stateful circuit breaker was manually reset.
#[derive(Debug, Clone)]
pub enum ResetReason {
    ManualOperator { operator: String },
    CioApproval { approver: String },
}

/// Hardcoded, compiled-in safety checks. Not configurable at runtime.
/// Sealed — external crates cannot add new circuit breakers.
///
/// All 11 breakers are registered in `CircuitBreakerBank`. The bank is
/// the only public entry point; individual breakers are crate-internal.
pub trait CircuitBreaker: Send + Sync + sealed::Sealed {
    /// Identifier for this breaker (e.g. `"CB-4"`).
    fn id(&self) -> &'static str;

    /// Evaluate the breaker against the current context. Always re-evaluates;
    /// does not cache or latch state — the bank handles latching for CB-3/4/5.
    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult;

    /// Reset a latched stateful breaker. No-op for stateless breakers.
    fn reset(&mut self, reason: ResetReason);

    /// Whether this breaker is currently latched (stateful CBs only).
    fn is_tripped(&self) -> bool;
}

// ─── Risk Gate (Layer 1) ───

/// Stateful risk evaluation. Knows current firm book. Configurable limits (hot-reloadable).
///
/// Accepts `&PipelineOrder<Validated>` — the type system guarantees the order
/// has passed structural validation and circuit breaker checks.
///
/// All time-dependent checks receive `now` from the Sequencer's Clock.
/// Mark-to-market checks use `PriceSource` for current prices.
///
/// Fast (<1ms per evaluation).
pub trait RiskGate: Send + Sync {
    /// Evaluate order against all risk limits.
    /// `now` from `Clock::now()`, `prices` for mark-to-market checks.
    fn evaluate(
        &self,
        order: &PipelineOrder<Validated>,
        prices: &dyn PriceSource,
        now: DateTime<Utc>,
    ) -> std::result::Result<RiskApproval, Vec<RiskViolation>>;

    /// Current firm book snapshot (positions, allocations, P&L).
    fn firm_book(&self) -> &FirmBook;

    /// Hot-reload risk limits (called by Taleb L2 or Feynman L3).
    fn update_limits(&mut self, limits: RiskLimits);

    /// Update internal state when a fill is processed.
    fn on_fill(&mut self, fill: &Fill, now: DateTime<Utc>);

    /// Update internal state during reconciliation.
    fn on_position_corrected(
        &mut self,
        instrument: &InstrumentId,
        new_qty: Decimal,
        now: DateTime<Utc>,
    );

    /// Get current limits (for display/monitoring).
    fn current_limits(&self) -> &RiskLimits;
}

/// Approval from RiskGate after successful evaluation.
#[derive(Debug, Clone)]
#[must_use]
pub struct RiskApproval {
    pub approved_at: DateTime<Utc>,
    pub warnings: Vec<RiskViolation>,
    pub checks_performed: Vec<RiskCheckResult>,
}

/// Order ingress path determines which risk checks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvaluationPath {
    SubmitSignal,
    SubmitOrder,
    SubmitBatch,
}

/// Layer 1 risk manager for path-aware deterministic checks.
#[derive(Debug, Clone)]
pub struct AgentRiskManager {
    nav: Decimal,
    agent_limits: HashMap<AgentId, AgentRiskLimits>,
    instrument_limits: HashMap<InstrumentId, InstrumentRiskLimits>,
    venue_limits: HashMap<VenueId, VenueRiskLimits>,
}

#[derive(Debug, Clone)]
struct ResizeCandidate {
    new_qty: Decimal,
    reason: String,
    warning: RiskViolation,
}

/// Project (net_notional, gross_notional) after applying a signed notional delta.
///
/// Correctly handles paired long/short book entries: a sell against a long reduces
/// gross rather than inflating it; a buy against a short covers rather than adds.
/// Used by CB-2 and the L1 risk gate.
pub(crate) fn project_net_and_gross(
    current_net: Decimal,
    current_gross: Decimal,
    signed_delta: Decimal,
) -> (Decimal, Decimal) {
    let two = dec!(2);
    let mut long = (current_gross + current_net) / two;
    let mut short = (current_gross - current_net) / two;

    if signed_delta >= Decimal::ZERO {
        let reduction = signed_delta.min(short);
        short -= reduction;
        long += signed_delta - reduction;
    } else {
        let sell_qty = signed_delta.abs();
        let reduction = sell_qty.min(long);
        long -= reduction;
        short += sell_qty - reduction;
    }

    (long - short, long + short)
}

impl AgentRiskManager {
    fn position_nav_cap() -> Decimal {
        dec!(0.05)
    }

    fn account_risk_nav_cap() -> Decimal {
        dec!(0.01)
    }

    fn drawdown_halt_pct() -> Decimal {
        dec!(-0.15)
    }

    fn cash_reserve_min_pct() -> Decimal {
        dec!(0.20)
    }

    fn min_risk_reward_ratio() -> Decimal {
        dec!(2)
    }

    #[must_use]
    pub fn new(
        nav: Decimal,
        agent_limits: HashMap<AgentId, AgentRiskLimits>,
        instrument_limits: HashMap<InstrumentId, InstrumentRiskLimits>,
        venue_limits: HashMap<VenueId, VenueRiskLimits>,
    ) -> Self {
        Self {
            nav,
            agent_limits,
            instrument_limits,
            venue_limits,
        }
    }

    #[must_use = "risk evaluation must be handled explicitly"]
    pub fn evaluate(
        &self,
        order: &PipelineOrder<Validated>,
        firm_book: &FirmBook,
        path: EvaluationPath,
    ) -> RiskOutcome {
        let core = order.core();
        let price = match core.price {
            Some(price) if price > Decimal::ZERO => price,
            Some(price) => {
                return Self::rejected(vec![Self::violation(
                    "order_price_positive",
                    price,
                    Decimal::ZERO,
                    "reject",
                    "order price must be positive for risk evaluation",
                )]);
            }
            None => {
                return Self::rejected(vec![Self::string_violation(
                    "order_price_required",
                    "missing",
                    "present",
                    "reject",
                    "order price is required for deterministic risk evaluation",
                )]);
            }
        };

        if core.qty <= Decimal::ZERO {
            return Self::rejected(vec![Self::violation(
                "order_qty_positive",
                core.qty,
                Decimal::ZERO,
                "reject",
                "order quantity must be positive",
            )]);
        }

        let effective_nav = match self.effective_nav(firm_book) {
            Ok(nav) => nav,
            Err(violation) => return Self::rejected(vec![violation]),
        };

        let agent_limits = match self.agent_limits.get(&core.agent_id) {
            Some(limits) => limits,
            None => {
                return Self::rejected(vec![Self::string_violation(
                    "agent_limits_present",
                    core.agent_id.to_string(),
                    "configured agent limits",
                    "reject",
                    "agent risk limits are not configured",
                )]);
            }
        };

        let agent_allocation = match firm_book.agent_allocations.get(&core.agent_id) {
            Some(allocation) => allocation,
            None => {
                return Self::rejected(vec![Self::string_violation(
                    "agent_allocation_present",
                    core.agent_id.to_string(),
                    "active agent allocation",
                    "reject",
                    "agent allocation is not present in the firm book",
                )]);
            }
        };

        let instrument_limits = match self.instrument_limits.get(&core.instrument_id) {
            Some(limits) => limits,
            None => {
                return Self::rejected(vec![Self::string_violation(
                    "instrument_limits_present",
                    core.instrument_id.to_string(),
                    "configured instrument limits",
                    "reject",
                    "instrument risk limits are not configured",
                )]);
            }
        };

        let venue_limits = match self.venue_limits.get(&core.venue_id) {
            Some(limits) => limits,
            None => {
                return Self::rejected(vec![Self::string_violation(
                    "venue_limits_present",
                    core.venue_id.to_string(),
                    "configured venue limits",
                    "reject",
                    "venue risk limits are not configured",
                )]);
            }
        };

        let order_notional = core.qty * price;
        let instrument_exposure = firm_book
            .instruments
            .iter()
            .find(|exposure| exposure.instrument == core.instrument_id);
        let agent_instrument_exposure = instrument_exposure.and_then(|exposure| {
            exposure
                .by_agent
                .iter()
                .find(|agent_exposure| agent_exposure.agent == core.agent_id)
        });
        let venue_instrument_exposure = instrument_exposure.and_then(|exposure| {
            exposure
                .by_venue
                .iter()
                .find(|venue_exposure| venue_exposure.venue == core.venue_id)
        });
        let current_instrument_net_qty =
            instrument_exposure.map_or(Decimal::ZERO, |exposure| exposure.net_qty);
        let current_instrument_gross_qty =
            instrument_exposure.map_or(Decimal::ZERO, |exposure| exposure.gross_qty);
        let current_instrument_net_notional =
            instrument_exposure.map_or(Decimal::ZERO, |exposure| exposure.net_notional_usd);
        let current_instrument_gross_notional =
            instrument_exposure.map_or(Decimal::ZERO, |exposure| exposure.gross_notional_usd);
        let current_agent_instrument_net_notional =
            agent_instrument_exposure.map_or(Decimal::ZERO, |agent_exposure| {
                Self::signed_value_from_net(
                    agent_exposure.net_qty,
                    agent_exposure.notional_usd.abs(),
                )
            });
        let current_agent_instrument_gross_notional = agent_instrument_exposure
            .map_or(Decimal::ZERO, |agent_exposure| {
                agent_exposure.notional_usd.abs()
            });
        let current_venue_instrument_net_notional =
            venue_instrument_exposure.map_or(Decimal::ZERO, |venue_exposure| {
                Self::signed_value_from_net(
                    venue_exposure.net_qty,
                    venue_exposure.notional_usd.abs(),
                )
            });
        let current_venue_instrument_gross_notional = venue_instrument_exposure
            .map_or(Decimal::ZERO, |venue_exposure| {
                venue_exposure.notional_usd.abs()
            });
        let signed_order_qty = Self::signed_delta(core.side, core.qty);
        let signed_order_notional = Self::signed_delta(core.side, order_notional);
        let (projected_instrument_net_qty, projected_instrument_gross_qty) =
            Self::project_net_and_gross(
                current_instrument_net_qty,
                current_instrument_gross_qty,
                signed_order_qty,
            );
        let (_, projected_instrument_gross_notional) = Self::project_net_and_gross(
            current_instrument_net_notional,
            current_instrument_gross_notional,
            signed_order_notional,
        );
        let (_, projected_agent_instrument_gross_notional) = Self::project_net_and_gross(
            current_agent_instrument_net_notional,
            current_agent_instrument_gross_notional,
            signed_order_notional,
        );
        let projected_agent_gross_notional = agent_allocation.used_capital
            - current_agent_instrument_gross_notional
            + projected_agent_instrument_gross_notional;
        let projected_agent_free_capital =
            agent_allocation.allocated_capital - projected_agent_gross_notional;
        let current_venue_notional = firm_book
            .instruments
            .iter()
            .flat_map(|exposure| exposure.by_venue.iter())
            .filter(|venue_exposure| venue_exposure.venue == core.venue_id)
            .fold(Decimal::ZERO, |acc, venue_exposure| {
                acc + venue_exposure.notional_usd.abs()
            });
        let current_venue_positions = firm_book
            .instruments
            .iter()
            .filter(|exposure| {
                exposure
                    .by_venue
                    .iter()
                    .any(|venue_exposure| venue_exposure.venue == core.venue_id)
            })
            .count() as u32;
        let (_, projected_venue_instrument_gross_notional) = Self::project_net_and_gross(
            current_venue_instrument_net_notional,
            current_venue_instrument_gross_notional,
            signed_order_notional,
        );
        let projected_venue_notional = current_venue_notional
            - current_venue_instrument_gross_notional
            + projected_venue_instrument_gross_notional;
        let projected_venue_positions =
            current_venue_positions + u32::from(venue_instrument_exposure.is_none());

        let mut hard_violations = Vec::new();
        let mut resize_candidates = Vec::new();

        self.check_agent_permissions(core, agent_limits, &mut hard_violations);
        self.check_agent_budget(
            projected_agent_gross_notional,
            projected_agent_free_capital,
            projected_agent_instrument_gross_notional,
            agent_limits,
            &mut hard_violations,
        );
        self.check_position_limit(order_notional, price, effective_nav, &mut resize_candidates);
        self.check_account_risk(
            core,
            price,
            effective_nav,
            &mut resize_candidates,
            &mut hard_violations,
        );
        self.check_leverage(
            projected_agent_gross_notional,
            projected_agent_free_capital,
            instrument_limits,
            &mut hard_violations,
        );
        self.check_drawdown(firm_book, &mut hard_violations);
        self.check_cash_reserve(
            signed_order_notional,
            effective_nav,
            firm_book,
            &mut hard_violations,
        );
        self.check_instrument_limits(
            effective_nav,
            instrument_limits,
            projected_instrument_net_qty,
            projected_instrument_gross_qty,
            projected_instrument_gross_notional,
            &mut hard_violations,
        );
        Self::check_venue_limits(
            projected_venue_notional,
            projected_venue_positions,
            effective_nav,
            venue_limits,
            &mut hard_violations,
        );

        if path == EvaluationPath::SubmitSignal {
            self.check_signal_requirements(core, price, &mut hard_violations);
        }

        if !hard_violations.is_empty() {
            return Self::rejected(hard_violations);
        }

        if let Some(candidate) = Self::tightest_resize(resize_candidates) {
            return RiskOutcome::Resized {
                new_qty: candidate.new_qty,
                reason: candidate.reason,
                warnings: vec![candidate.warning],
            };
        }

        RiskOutcome::Approved {
            warnings: Vec::new(),
        }
    }

    fn effective_nav(&self, firm_book: &FirmBook) -> std::result::Result<Decimal, RiskViolation> {
        if self.nav <= Decimal::ZERO {
            return Err(Self::violation(
                "manager_nav_positive",
                self.nav,
                Decimal::ZERO,
                "reject",
                "configured risk-manager NAV must be positive",
            ));
        }
        if firm_book.nav <= Decimal::ZERO {
            return Err(Self::violation(
                "firm_nav_positive",
                firm_book.nav,
                Decimal::ZERO,
                "reject",
                "firm-book NAV must be positive",
            ));
        }

        Ok(self.nav.min(firm_book.nav))
    }

    fn check_agent_permissions(
        &self,
        core: &OrderCore,
        agent_limits: &AgentRiskLimits,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        if let Some(allowed_instruments) = &agent_limits.allowed_instruments {
            if !allowed_instruments.contains(&core.instrument_id) {
                hard_violations.push(Self::string_violation(
                    "agent_allowed_instruments",
                    core.instrument_id.to_string(),
                    "instrument in agent whitelist",
                    "reject",
                    "agent is not permitted to trade this instrument",
                ));
            }
        }

        if let Some(allowed_venues) = &agent_limits.allowed_venues {
            if !allowed_venues.contains(&core.venue_id) {
                hard_violations.push(Self::string_violation(
                    "agent_allowed_venues",
                    core.venue_id.to_string(),
                    "venue in agent whitelist",
                    "reject",
                    "agent is not permitted to trade this venue",
                ));
            }
        }
    }

    fn check_agent_budget(
        &self,
        projected_agent_gross_notional: Decimal,
        projected_agent_free_capital: Decimal,
        projected_agent_instrument_gross_notional: Decimal,
        agent_limits: &AgentRiskLimits,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        if projected_agent_free_capital < Decimal::ZERO {
            hard_violations.push(Self::violation(
                "agent_free_capital",
                projected_agent_free_capital,
                Decimal::ZERO,
                "reject",
                "order exceeds the agent's free capital",
            ));
        }

        if projected_agent_instrument_gross_notional > agent_limits.max_position_notional {
            hard_violations.push(Self::violation(
                "agent_max_position_notional",
                projected_agent_instrument_gross_notional,
                agent_limits.max_position_notional,
                "reject",
                "order exceeds the agent's max single-position notional",
            ));
        }

        if projected_agent_gross_notional > agent_limits.max_gross_notional {
            hard_violations.push(Self::violation(
                "agent_max_gross_notional",
                projected_agent_gross_notional,
                agent_limits.max_gross_notional,
                "reject",
                "projected agent gross notional exceeds the configured cap",
            ));
        }
    }

    fn check_position_limit(
        &self,
        order_notional: Decimal,
        price: Decimal,
        nav: Decimal,
        resize_candidates: &mut Vec<ResizeCandidate>,
    ) {
        let max_position_notional = nav * Self::position_nav_cap();
        if order_notional > max_position_notional {
            resize_candidates.push(ResizeCandidate {
                new_qty: max_position_notional / price,
                reason: "position notional exceeds 5% NAV; resize to the 5% cap".into(),
                warning: Self::soft_violation(
                    "position_notional_pct",
                    order_notional,
                    max_position_notional,
                    "resize",
                    "order exceeds the 5% NAV single-position cap",
                ),
            });
        }
    }

    fn check_account_risk(
        &self,
        core: &OrderCore,
        price: Decimal,
        nav: Decimal,
        resize_candidates: &mut Vec<ResizeCandidate>,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        let risk_per_unit = core
            .stop_loss
            .map_or(price, |stop_loss| (price - stop_loss).abs());
        let max_loss_cap = nav * Self::account_risk_nav_cap();

        if risk_per_unit <= Decimal::ZERO {
            hard_violations.push(Self::violation(
                "account_risk_distance_positive",
                risk_per_unit,
                Decimal::ZERO,
                "reject",
                "entry price and stop loss must define a positive loss distance",
            ));
            return;
        }

        let projected_max_loss = core.qty * risk_per_unit;
        if projected_max_loss > max_loss_cap {
            resize_candidates.push(ResizeCandidate {
                new_qty: max_loss_cap / risk_per_unit,
                reason: "projected account risk exceeds 1% NAV; resize to the max-loss cap".into(),
                warning: Self::soft_violation(
                    "account_risk_pct",
                    projected_max_loss,
                    max_loss_cap,
                    "resize",
                    "projected max loss exceeds the 1% NAV cap",
                ),
            });
        }
    }

    fn check_leverage(
        &self,
        projected_agent_gross_notional: Decimal,
        projected_agent_free_capital: Decimal,
        instrument_limits: &InstrumentRiskLimits,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        if instrument_limits.max_leverage <= Decimal::ZERO {
            hard_violations.push(Self::violation(
                "instrument_max_leverage_positive",
                instrument_limits.max_leverage,
                Decimal::ZERO,
                "reject",
                "instrument max leverage must be positive",
            ));
            return;
        }

        if projected_agent_free_capital <= Decimal::ZERO {
            hard_violations.push(Self::violation(
                "agent_free_capital_positive",
                projected_agent_free_capital,
                Decimal::ZERO,
                "reject",
                "agent free capital must be positive to evaluate leverage",
            ));
            return;
        }

        let projected_leverage = projected_agent_gross_notional / projected_agent_free_capital;
        if projected_leverage > instrument_limits.max_leverage {
            hard_violations.push(Self::violation(
                "instrument_max_leverage",
                projected_leverage,
                instrument_limits.max_leverage,
                "reject",
                "projected leverage exceeds the instrument leverage cap",
            ));
        }
    }

    fn check_drawdown(&self, firm_book: &FirmBook, hard_violations: &mut Vec<RiskViolation>) {
        if firm_book.current_drawdown_pct < Self::drawdown_halt_pct() {
            hard_violations.push(Self::violation(
                "firm_drawdown_pct",
                firm_book.current_drawdown_pct,
                Self::drawdown_halt_pct(),
                "halt_all",
                "firm drawdown breached the 15% halt threshold",
            ));
        }
    }

    fn check_cash_reserve(
        &self,
        signed_order_notional: Decimal,
        nav: Decimal,
        firm_book: &FirmBook,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        let projected_cash_available = firm_book.cash_available - signed_order_notional;
        let projected_cash_pct = projected_cash_available / nav;

        if projected_cash_pct < Self::cash_reserve_min_pct() {
            hard_violations.push(Self::violation(
                "cash_reserve_pct",
                projected_cash_pct,
                Self::cash_reserve_min_pct(),
                "reject",
                "projected cash reserve falls below the 20% NAV minimum",
            ));
        }
    }

    fn check_instrument_limits(
        &self,
        nav: Decimal,
        instrument_limits: &InstrumentRiskLimits,
        projected_net_qty: Decimal,
        projected_gross_qty: Decimal,
        projected_gross_notional: Decimal,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        let projected_concentration_pct = projected_gross_notional / nav;

        if projected_net_qty.abs() > instrument_limits.max_net_qty {
            hard_violations.push(Self::violation(
                "instrument_max_net_qty",
                projected_net_qty.abs(),
                instrument_limits.max_net_qty,
                "reject",
                "projected instrument net quantity exceeds the configured cap",
            ));
        }

        if projected_gross_qty > instrument_limits.max_gross_qty {
            hard_violations.push(Self::violation(
                "instrument_max_gross_qty",
                projected_gross_qty,
                instrument_limits.max_gross_qty,
                "reject",
                "projected instrument gross quantity exceeds the configured cap",
            ));
        }

        if projected_concentration_pct > instrument_limits.max_concentration_pct {
            hard_violations.push(Self::violation(
                "instrument_max_concentration_pct",
                projected_concentration_pct,
                instrument_limits.max_concentration_pct,
                "reject",
                "projected instrument exposure exceeds the concentration cap",
            ));
        }
    }

    fn check_venue_limits(
        projected_venue_notional: Decimal,
        projected_positions: u32,
        nav: Decimal,
        venue_limits: &VenueRiskLimits,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        let projected_pct_of_nav = projected_venue_notional / nav;

        if projected_venue_notional > venue_limits.max_notional {
            hard_violations.push(Self::violation(
                "venue_max_notional",
                projected_venue_notional,
                venue_limits.max_notional,
                "reject",
                "projected venue notional exceeds the configured cap",
            ));
        }

        if projected_pct_of_nav > venue_limits.max_pct_of_nav {
            hard_violations.push(Self::violation(
                "venue_max_pct_of_nav",
                projected_pct_of_nav,
                venue_limits.max_pct_of_nav,
                "reject",
                "projected venue concentration exceeds the NAV cap",
            ));
        }

        if projected_positions > venue_limits.max_positions {
            hard_violations.push(Self::violation(
                "venue_max_positions",
                Decimal::from(projected_positions),
                Decimal::from(venue_limits.max_positions),
                "reject",
                "projected venue position count exceeds the configured cap",
            ));
        }
    }

    fn check_signal_requirements(
        &self,
        core: &OrderCore,
        price: Decimal,
        hard_violations: &mut Vec<RiskViolation>,
    ) {
        let stop_loss = match core.stop_loss {
            Some(stop_loss) => stop_loss,
            None => {
                hard_violations.push(Self::string_violation(
                    "signal_stop_loss_required",
                    "missing",
                    "present",
                    "reject",
                    "SubmitSignal orders must define a stop loss",
                ));
                return;
            }
        };

        if let Some(take_profit) = core.take_profit {
            let risk_distance = (price - stop_loss).abs();
            if risk_distance <= Decimal::ZERO {
                hard_violations.push(Self::violation(
                    "signal_risk_distance_positive",
                    risk_distance,
                    Decimal::ZERO,
                    "reject",
                    "risk/reward evaluation requires a positive stop distance",
                ));
                return;
            }

            let reward_distance = (take_profit - price).abs();
            let risk_reward_ratio = reward_distance / risk_distance;
            if risk_reward_ratio < Self::min_risk_reward_ratio() {
                hard_violations.push(Self::violation(
                    "signal_risk_reward_ratio",
                    risk_reward_ratio,
                    Self::min_risk_reward_ratio(),
                    "reject",
                    "signal risk/reward ratio is below 2:1",
                ));
            }
        }
    }

    fn tightest_resize(candidates: Vec<ResizeCandidate>) -> Option<ResizeCandidate> {
        candidates
            .into_iter()
            .min_by(|left, right| left.new_qty.cmp(&right.new_qty))
    }

    fn signed_delta(side: Side, qty: Decimal) -> Decimal {
        match side {
            Side::Buy => qty,
            Side::Sell => -qty,
        }
    }

    fn signed_value_from_net(net_qty: Decimal, absolute_value: Decimal) -> Decimal {
        if net_qty >= Decimal::ZERO {
            absolute_value
        } else {
            -absolute_value
        }
    }

    fn project_net_and_gross(
        current_net: Decimal,
        current_gross: Decimal,
        signed_delta: Decimal,
    ) -> (Decimal, Decimal) {
        project_net_and_gross(current_net, current_gross, signed_delta)
    }

    fn rejected(violations: Vec<RiskViolation>) -> RiskOutcome {
        RiskOutcome::Rejected { violations }
    }

    fn violation(
        check_name: &'static str,
        current_value: Decimal,
        limit: Decimal,
        suggested_action: &'static str,
        reason: &'static str,
    ) -> RiskViolation {
        RiskViolation {
            check_name: check_name.into(),
            violation_type: ViolationType::Hard,
            current_value: current_value.to_string(),
            limit: limit.to_string(),
            suggested_action: format!("{suggested_action}: {reason}"),
        }
    }

    fn soft_violation(
        check_name: &'static str,
        current_value: Decimal,
        limit: Decimal,
        suggested_action: &'static str,
        reason: &'static str,
    ) -> RiskViolation {
        RiskViolation {
            check_name: check_name.into(),
            violation_type: ViolationType::Soft,
            current_value: current_value.to_string(),
            limit: limit.to_string(),
            suggested_action: format!("{suggested_action}: {reason}"),
        }
    }

    fn string_violation(
        check_name: &'static str,
        current_value: impl Into<String>,
        limit: impl Into<String>,
        suggested_action: &'static str,
        reason: &'static str,
    ) -> RiskViolation {
        RiskViolation {
            check_name: check_name.into(),
            violation_type: ViolationType::Hard,
            current_value: current_value.into(),
            limit: limit.into(),
            suggested_action: format!("{suggested_action}: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::{AgentRiskManager, EvaluationPath, RiskOutcome};
    use types::{
        AgentAllocation, AgentId, AgentRiskLimits, AgentStatus, ExecHint, FirmBook,
        InstrumentExposure, InstrumentId, InstrumentRiskLimits, OrderCore, OrderId, OrderType,
        PipelineOrder, PredictionExposureSummary, Side, TimeInForce, Validated, VenueExposure,
        VenueId, VenueRiskLimits,
    };

    #[test]
    fn test_evaluate_position_exceeding_nav_resized() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(60), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Resized { new_qty, .. } => assert_eq!(new_qty, dec!(50)),
            other => panic!("expected resized outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_account_risk_exceeding_cap_resized() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(30), Some(dec!(50)), Some(dec!(150)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Resized { new_qty, .. } => assert_eq!(new_qty, dec!(20)),
            other => panic!("expected resized outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_leverage_limit_rejected() {
        let mut manager = test_manager();
        manager.instrument_limits.insert(
            InstrumentId("BTC-USD".into()),
            InstrumentRiskLimits {
                instrument: InstrumentId("BTC-USD".into()),
                max_net_qty: dec!(1_000),
                max_gross_qty: dec!(1_000),
                max_concentration_pct: dec!(0.50),
                max_leverage: dec!(1.4),
            },
        );
        let mut book = test_firm_book();
        book.agent_allocations.insert(
            AgentId("satoshi".into()),
            AgentAllocation {
                allocated_capital: dec!(12_000),
                used_capital: dec!(5_000),
                free_capital: dec!(7_000),
                realized_pnl: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
                current_drawdown: Decimal::ZERO,
                max_drawdown_limit: dec!(-0.15),
                status: AgentStatus::Active,
            },
        );
        let order = test_order(dec!(50), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "instrument_max_leverage"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_drawdown_breach_rejected() {
        let manager = test_manager();
        let mut book = test_firm_book();
        book.current_drawdown_pct = dec!(-0.16);
        let order = test_order(dec!(5), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "firm_drawdown_pct"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_cash_reserve_breach_rejected() {
        let mut book = test_firm_book();
        book.cash_available = dec!(21_000);
        let manager = test_manager();
        let order = test_order(dec!(20), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "cash_reserve_pct"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_signal_without_stop_loss_rejected() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(5), None, Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitSignal) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "signal_stop_loss_required"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_signal_bad_risk_reward_rejected() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(5), Some(dec!(90)), Some(dec!(115)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitSignal) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "signal_risk_reward_ratio"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_submit_order_without_stop_loss_approved() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(5), None, None);

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Approved { warnings } => assert!(warnings.is_empty()),
            other => panic!("expected approved outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_submit_batch_without_stop_loss_approved() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(5), None, None);

        match manager.evaluate(&order, &book, EvaluationPath::SubmitBatch) {
            RiskOutcome::Approved { warnings } => assert!(warnings.is_empty()),
            other => panic!("expected approved outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_agent_budget_isolated_rejected() {
        let manager = test_manager();
        let mut book = test_firm_book();
        book.agent_allocations.insert(
            AgentId("satoshi".into()),
            AgentAllocation {
                allocated_capital: dec!(10_000),
                used_capital: dec!(9_750),
                free_capital: dec!(250),
                realized_pnl: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
                current_drawdown: Decimal::ZERO,
                max_drawdown_limit: dec!(-0.15),
                status: AgentStatus::Active,
            },
        );
        let order = test_order(dec!(5), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "agent_free_capital"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_reducing_sell_frees_capital_and_cash() {
        let manager = test_manager();
        let mut book = test_firm_book();
        book.cash_available = dec!(20_500);
        book.agent_allocations.insert(
            AgentId("satoshi".into()),
            AgentAllocation {
                allocated_capital: dec!(3_000),
                used_capital: dec!(2_500),
                free_capital: dec!(500),
                realized_pnl: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
                current_drawdown: Decimal::ZERO,
                max_drawdown_limit: dec!(-0.15),
                status: AgentStatus::Active,
            },
        );
        let order = test_order_with_side(Side::Sell, dec!(10), Some(dec!(105)), None);

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Approved { warnings } => assert!(warnings.is_empty()),
            other => panic!("expected approved outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_leverage_is_agent_scoped() {
        let manager = test_manager();
        let mut book = test_firm_book();
        book.gross_notional = dec!(100_000);
        let order = test_order(dec!(5), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Approved { warnings } => assert!(warnings.is_empty()),
            other => panic!("expected approved outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_instrument_concentration_rejected() {
        let mut manager = test_manager();
        manager.instrument_limits.insert(
            InstrumentId("BTC-USD".into()),
            InstrumentRiskLimits {
                instrument: InstrumentId("BTC-USD".into()),
                max_net_qty: dec!(1_000),
                max_gross_qty: dec!(1_000),
                max_concentration_pct: dec!(0.05),
                max_leverage: dec!(3),
            },
        );
        let mut book = test_firm_book();
        book.instruments[0].gross_notional_usd = dec!(4_500);
        book.instruments[0].net_notional_usd = dec!(4_500);
        let order = test_order(dec!(10), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "instrument_max_concentration_pct"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_venue_limit_rejected() {
        let manager = test_manager();
        let mut book = test_firm_book();
        book.instruments[0].by_venue[0].notional_usd = dec!(9_500);
        let order = test_order(dec!(10), Some(dec!(95)), Some(dec!(110)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitOrder) {
            RiskOutcome::Rejected { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.check_name == "venue_max_notional"));
            }
            other => panic!("expected rejected outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_all_checks_pass_approved() {
        let manager = test_manager();
        let book = test_firm_book();
        let order = test_order(dec!(5), Some(dec!(95)), Some(dec!(120)));

        match manager.evaluate(&order, &book, EvaluationPath::SubmitSignal) {
            RiskOutcome::Approved { warnings } => assert!(warnings.is_empty()),
            other => panic!("expected approved outcome, got {other:?}"),
        }
    }

    fn test_manager() -> AgentRiskManager {
        let agent_id = AgentId("satoshi".into());
        let instrument_id = InstrumentId("BTC-USD".into());
        let venue_id = VenueId("bybit".into());

        AgentRiskManager::new(
            dec!(100_000),
            HashMap::from([(
                agent_id.clone(),
                AgentRiskLimits {
                    allocated_capital: dec!(10_000),
                    max_position_notional: dec!(10_000),
                    max_gross_notional: dec!(10_000),
                    max_drawdown_pct: dec!(0.15),
                    max_daily_loss: dec!(2_000),
                    max_open_orders: 10,
                    allowed_instruments: Some(vec![instrument_id.clone()]),
                    allowed_venues: Some(vec![venue_id.clone()]),
                },
            )]),
            HashMap::from([(
                instrument_id.clone(),
                InstrumentRiskLimits {
                    instrument: instrument_id,
                    max_net_qty: dec!(1_000),
                    max_gross_qty: dec!(1_000),
                    max_concentration_pct: dec!(0.10),
                    max_leverage: dec!(10),
                },
            )]),
            HashMap::from([(
                venue_id.clone(),
                VenueRiskLimits {
                    venue: venue_id,
                    max_notional: dec!(10_000),
                    max_positions: 5,
                    max_pct_of_nav: dec!(0.10),
                },
            )]),
        )
    }

    fn test_firm_book() -> FirmBook {
        FirmBook {
            nav: dec!(100_000),
            gross_notional: dec!(2_500),
            net_notional: dec!(2_500),
            realized_pnl: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            daily_pnl: Decimal::ZERO,
            hourly_pnl: Decimal::ZERO,
            current_drawdown_pct: Decimal::ZERO,
            allocated_capital: dec!(10_000),
            cash_available: dec!(50_000),
            total_fees_paid: Decimal::ZERO,
            agent_allocations: HashMap::from([(
                AgentId("satoshi".into()),
                AgentAllocation {
                    allocated_capital: dec!(10_000),
                    used_capital: dec!(2_500),
                    free_capital: dec!(7_500),
                    realized_pnl: Decimal::ZERO,
                    unrealized_pnl: Decimal::ZERO,
                    current_drawdown: Decimal::ZERO,
                    max_drawdown_limit: dec!(-0.15),
                    status: AgentStatus::Active,
                },
            )]),
            instruments: vec![InstrumentExposure {
                instrument: InstrumentId("BTC-USD".into()),
                net_qty: dec!(25),
                gross_qty: dec!(25),
                net_notional_usd: dec!(2_500),
                gross_notional_usd: dec!(2_500),
                by_venue: vec![VenueExposure {
                    venue: VenueId("bybit".into()),
                    account: types::AccountId("acct-1".into()),
                    net_qty: dec!(25),
                    notional_usd: dec!(2_500),
                }],
                by_agent: vec![types::AgentExposure {
                    agent: AgentId("satoshi".into()),
                    net_qty: dec!(25),
                    notional_usd: dec!(2_500),
                    pnl: Decimal::ZERO,
                }],
            }],
            prediction_exposure: PredictionExposureSummary {
                total_notional: Decimal::ZERO,
                pct_of_nav: Decimal::ZERO,
                unresolved_markets: 0,
            },
            as_of: Utc::now(),
        }
    }

    fn test_order(
        qty: Decimal,
        stop_loss: Option<Decimal>,
        take_profit: Option<Decimal>,
    ) -> PipelineOrder<Validated> {
        test_order_with_side(Side::Buy, qty, stop_loss, take_profit)
    }

    fn test_order_with_side(
        side: Side,
        qty: Decimal,
        stop_loss: Option<Decimal>,
        take_profit: Option<Decimal>,
    ) -> PipelineOrder<Validated> {
        PipelineOrder::new(OrderCore {
            id: OrderId("ord-1".into()),
            basket_id: None,
            agent_id: AgentId("satoshi".into()),
            instrument_id: InstrumentId("BTC-USD".into()),
            market_id: types::MarketId("BTC-USD".into()),
            venue_id: VenueId("bybit".into()),
            side,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            qty,
            price: Some(dec!(100)),
            trigger_price: None,
            stop_loss,
            take_profit,
            post_only: false,
            reduce_only: false,
            dry_run: true,
            exec_hint: ExecHint::default(),
            created_at: Utc::now(),
        })
        .into_validated(Utc::now())
    }
}

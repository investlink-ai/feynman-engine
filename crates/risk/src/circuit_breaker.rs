//! Circuit Breaker implementations — CB-1 through CB-11 (Layer 0).
//!
//! All 11 breakers are compiled-in and immutable. They run synchronously in the
//! Sequencer hot path before the L1 risk gate. Sub-microsecond, zero I/O.
//!
//! **Hierarchy:**
//! - CB-4 / CB-5 (loss-based) → `HaltAll`
//! - CB-3 → `Reject` with latch (manual reset required)
//! - All others → `Reject` per-order (auto-reset, re-evaluated each call)
//!
//! Entry point: [`CircuitBreakerBank::check_all`].

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::{sealed, CircuitBreaker, CircuitBreakerContext, CircuitBreakerResult, ResetReason};

use tracing::{error, warn};

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn reject(breaker: &'static str, reason: impl Into<String>) -> CircuitBreakerResult {
    CircuitBreakerResult::Reject {
        breaker,
        reason: reason.into(),
    }
}

fn halt(breaker: &'static str, reason: impl Into<String>) -> CircuitBreakerResult {
    CircuitBreakerResult::HaltAll {
        breaker,
        reason: reason.into(),
    }
}

// ─── CB-1: Max single order notional ─────────────────────────────────────────

/// Rejects any single order whose notional exceeds the compiled-in cap.
pub(crate) struct Cb1MaxSingleOrderNotional;

impl sealed::Sealed for Cb1MaxSingleOrderNotional {}

impl CircuitBreaker for Cb1MaxSingleOrderNotional {
    fn id(&self) -> &'static str {
        "CB-1"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        let order = match ctx.order {
            Some(o) => o,
            None => return CircuitBreakerResult::Pass,
        };
        let core = order.core();
        let price = match core.price {
            Some(p) if p > Decimal::ZERO => p,
            _ => return CircuitBreakerResult::Pass, // price validation is L1's job
        };
        let notional = core.qty * price;
        if notional > ctx.config.max_single_order_notional {
            return reject(
                "CB-1",
                format!(
                    "order notional {notional} exceeds max single order notional {}",
                    ctx.config.max_single_order_notional
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {} // stateless
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-2: Max position per instrument ───────────────────────────────────────

/// Rejects orders that would push gross instrument exposure past the cap.
///
/// Uses a signed notional delta (positive for buys, negative for sells) so that
/// risk-reducing trades — a sell against a long, a buy to cover a short — are
/// not incorrectly rejected when the book is near the limit.
pub(crate) struct Cb2MaxInstrumentPosition;

impl sealed::Sealed for Cb2MaxInstrumentPosition {}

impl CircuitBreaker for Cb2MaxInstrumentPosition {
    fn id(&self) -> &'static str {
        "CB-2"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        let order = match ctx.order {
            Some(o) => o,
            None => return CircuitBreakerResult::Pass,
        };
        let core = order.core();
        let price = match core.price {
            Some(p) if p > Decimal::ZERO => p,
            _ => return CircuitBreakerResult::Pass,
        };
        let order_notional = core.qty * price;
        // Signed delta: positive for buys (increases long / covers short),
        // negative for sells (reduces long / increases short).
        let signed_delta = match core.side {
            types::Side::Buy => order_notional,
            types::Side::Sell => -order_notional,
        };
        let (current_net, current_gross) = ctx
            .firm_book
            .instruments
            .iter()
            .find(|e| e.instrument == core.instrument_id)
            .map_or((Decimal::ZERO, Decimal::ZERO), |e| {
                (e.net_notional_usd, e.gross_notional_usd)
            });
        let (_projected_net, projected_gross) =
            crate::project_net_and_gross(current_net, current_gross, signed_delta);
        if projected_gross > ctx.config.max_instrument_position_notional {
            return reject(
                "CB-2",
                format!(
                    "projected instrument gross exposure {projected_gross} exceeds max {}",
                    ctx.config.max_instrument_position_notional
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-3: Max firm gross notional ───────────────────────────────────────────

/// Rejects new orders when firm gross notional exceeds the cap.
/// Latches on trip — manual reset required.
pub(crate) struct Cb3MaxFirmGrossNotional {
    tripped: bool,
}

impl Cb3MaxFirmGrossNotional {
    fn new() -> Self {
        Self { tripped: false }
    }

    fn latch(&mut self) {
        self.tripped = true;
    }
}

impl sealed::Sealed for Cb3MaxFirmGrossNotional {}

impl CircuitBreaker for Cb3MaxFirmGrossNotional {
    fn id(&self) -> &'static str {
        "CB-3"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        if self.tripped {
            return reject(
                "CB-3",
                "firm gross notional limit latched — manual reset required",
            );
        }
        if ctx.firm_book.gross_notional > ctx.config.max_firm_gross_notional {
            return reject(
                "CB-3",
                format!(
                    "firm gross notional {} exceeds max {}",
                    ctx.firm_book.gross_notional, ctx.config.max_firm_gross_notional
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.tripped = false;
    }

    fn is_tripped(&self) -> bool {
        self.tripped
    }
}

// ─── CB-4: Firm daily loss ────────────────────────────────────────────────────

/// HaltAll when firm daily P&L breaches the loss cap. Latches — manual reset + CIO approval.
pub(crate) struct Cb4FirmDailyLoss {
    tripped: bool,
}

impl Cb4FirmDailyLoss {
    fn new() -> Self {
        Self { tripped: false }
    }

    fn latch(&mut self) {
        self.tripped = true;
    }
}

impl sealed::Sealed for Cb4FirmDailyLoss {}

impl CircuitBreaker for Cb4FirmDailyLoss {
    fn id(&self) -> &'static str {
        "CB-4"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        if self.tripped {
            return halt(
                "CB-4",
                "daily loss halt latched — manual reset + CIO approval required",
            );
        }
        if ctx.firm_book.nav <= Decimal::ZERO {
            return CircuitBreakerResult::Pass; // NAV check is L1's job
        }
        let max_loss = ctx.config.max_daily_loss_pct * ctx.firm_book.nav;
        if ctx.firm_book.daily_pnl < -max_loss {
            return halt(
                "CB-4",
                format!(
                    "daily P&L {} breached -{:.1}% NAV loss cap (limit: -{})",
                    ctx.firm_book.daily_pnl,
                    ctx.config.max_daily_loss_pct * dec!(100),
                    max_loss
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.tripped = false;
    }

    fn is_tripped(&self) -> bool {
        self.tripped
    }
}

// ─── CB-5: Firm hourly loss ───────────────────────────────────────────────────

/// HaltAll when firm hourly P&L breaches the loss cap. Latches — manual reset.
pub(crate) struct Cb5FirmHourlyLoss {
    tripped: bool,
}

impl Cb5FirmHourlyLoss {
    fn new() -> Self {
        Self { tripped: false }
    }

    fn latch(&mut self) {
        self.tripped = true;
    }
}

impl sealed::Sealed for Cb5FirmHourlyLoss {}

impl CircuitBreaker for Cb5FirmHourlyLoss {
    fn id(&self) -> &'static str {
        "CB-5"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        if self.tripped {
            return halt("CB-5", "hourly loss halt latched — manual reset required");
        }
        if ctx.firm_book.nav <= Decimal::ZERO {
            return CircuitBreakerResult::Pass;
        }
        let max_loss = ctx.config.max_hourly_loss_pct * ctx.firm_book.nav;
        if ctx.firm_book.hourly_pnl < -max_loss {
            return halt(
                "CB-5",
                format!(
                    "hourly P&L {} breached -{:.1}% NAV loss cap (limit: -{})",
                    ctx.firm_book.hourly_pnl,
                    ctx.config.max_hourly_loss_pct * dec!(100),
                    max_loss
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.tripped = false;
    }

    fn is_tripped(&self) -> bool {
        self.tripped
    }
}

// ─── CB-6: Orders per minute (firm) ──────────────────────────────────────────

/// Rejects new orders when firm-wide order rate exceeds the cap.
/// Auto-resets — re-evaluated each call against the context window.
pub(crate) struct Cb6FirmOrdersPerMinute;

impl sealed::Sealed for Cb6FirmOrdersPerMinute {}

impl CircuitBreaker for Cb6FirmOrdersPerMinute {
    fn id(&self) -> &'static str {
        "CB-6"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        if ctx.firm_orders_last_60s > ctx.config.max_firm_orders_per_min {
            return reject(
                "CB-6",
                format!(
                    "firm order rate {}/min exceeds max {}/min",
                    ctx.firm_orders_last_60s, ctx.config.max_firm_orders_per_min
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-7: Orders per minute (per-agent) ─────────────────────────────────────

/// Rejects orders from an agent when that agent's order rate exceeds the cap.
/// Auto-resets — re-evaluated each call against the context window.
pub(crate) struct Cb7AgentOrdersPerMinute;

impl sealed::Sealed for Cb7AgentOrdersPerMinute {}

impl CircuitBreaker for Cb7AgentOrdersPerMinute {
    fn id(&self) -> &'static str {
        "CB-7"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        let order = match ctx.order {
            Some(o) => o,
            None => return CircuitBreakerResult::Pass,
        };
        let agent_id = &order.core().agent_id;
        let agent_rate = ctx
            .agent_orders_last_60s
            .get(agent_id)
            .copied()
            .unwrap_or(0);
        if agent_rate > ctx.config.max_agent_orders_per_min {
            return reject(
                "CB-7",
                format!(
                    "agent {} order rate {}/min exceeds max {}/min",
                    agent_id, agent_rate, ctx.config.max_agent_orders_per_min
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-8: Venue error rate ───────────────────────────────────────────────────

/// Rejects orders to a venue when its error rate over the last 5 minutes is too high.
/// Auto-resets — re-evaluated each call against the context window.
pub(crate) struct Cb8VenueErrorRate;

impl sealed::Sealed for Cb8VenueErrorRate {}

impl CircuitBreaker for Cb8VenueErrorRate {
    fn id(&self) -> &'static str {
        "CB-8"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        let order = match ctx.order {
            Some(o) => o,
            None => return CircuitBreakerResult::Pass,
        };
        let venue_id = &order.core().venue_id;
        if let Some(stats) = ctx.venue_error_stats.get(venue_id) {
            if stats.total_last_5min > 0 {
                let error_rate =
                    Decimal::from(stats.errors_last_5min) / Decimal::from(stats.total_last_5min);
                if error_rate > ctx.config.max_venue_error_rate {
                    return reject(
                        "CB-8",
                        format!(
                            "venue {venue_id} error rate {error_rate:.2} exceeds max {}",
                            ctx.config.max_venue_error_rate
                        ),
                    );
                }
            }
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-9: Venue disconnected ─────────────────────────────────────────────────

/// Rejects orders to a venue that has not sent a heartbeat within the disconnect window.
/// Auto-resets — re-evaluated each call against the current heartbeat timestamp.
pub(crate) struct Cb9VenueDisconnected;

impl sealed::Sealed for Cb9VenueDisconnected {}

impl CircuitBreaker for Cb9VenueDisconnected {
    fn id(&self) -> &'static str {
        "CB-9"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        let order = match ctx.order {
            Some(o) => o,
            None => return CircuitBreakerResult::Pass,
        };
        let venue_id = &order.core().venue_id;
        let health = match ctx.venue_health.get(venue_id) {
            Some(h) => h,
            None => {
                return reject(
                    "CB-9",
                    format!("no health record for venue {venue_id} — cannot verify connectivity"),
                )
            }
        };
        // Fail closed: venue must be in Connected state before we even check heartbeat age.
        // Stale, Reconnecting, and Disconnected states are never submittable even if the
        // last heartbeat timestamp is recent (the state machine tracks transitions that
        // heartbeat age alone cannot capture).
        if !health.is_submittable() {
            return reject(
                "CB-9",
                format!(
                    "venue {venue_id} is not in Connected state ({:?}) — submission blocked",
                    health.state
                ),
            );
        }
        let last_hb = match health.last_heartbeat_at {
            Some(t) => t,
            None => {
                return reject(
                    "CB-9",
                    format!("venue {venue_id} has never sent a heartbeat"),
                )
            }
        };
        // Fail closed on future timestamps: if last_hb is ahead of the engine clock, the
        // heartbeat record is untrustworthy (clock skew, replay bug, or tampered state).
        // Passing silently would keep CB-9 open indefinitely while the clock catches up.
        let elapsed = ctx.clock.now() - last_hb;
        if elapsed.num_seconds() < 0 {
            return reject(
                "CB-9",
                format!(
                    "venue {venue_id} heartbeat timestamp is {:.1}s in the future — clock skew detected",
                    elapsed.num_seconds().unsigned_abs()
                ),
            );
        }
        let elapsed_secs = elapsed.num_seconds() as u64;
        if elapsed_secs > ctx.config.max_venue_disconnect_secs {
            return reject(
                "CB-9",
                format!(
                    "venue {venue_id} last heartbeat {elapsed_secs}s ago exceeds {}-second disconnect limit",
                    ctx.config.max_venue_disconnect_secs
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-10: Dry-run bypass ────────────────────────────────────────────────────

/// Hard-rejects any order where `dry_run == false` and the engine is not in live mode.
/// Prevents paper/backtest orders from reaching real venues. Never bypassable.
pub(crate) struct Cb10DryRunBypass;

impl sealed::Sealed for Cb10DryRunBypass {}

impl CircuitBreaker for Cb10DryRunBypass {
    fn id(&self) -> &'static str {
        "CB-10"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        let order = match ctx.order {
            Some(o) => o,
            None => return CircuitBreakerResult::Pass,
        };
        let core = order.core();
        if !core.dry_run && ctx.execution_mode != crate::ExecutionMode::Live {
            return reject(
                "CB-10",
                format!(
                    "order has dry_run=false but engine is in {:?} mode — live submission blocked",
                    ctx.execution_mode
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── CB-11: Sequencer queue depth ────────────────────────────────────────────

/// Rejects new orders when the Sequencer command queue is above the depth cap (backpressure).
/// Auto-resets when the queue drains below the threshold.
pub(crate) struct Cb11SequencerQueueDepth;

impl sealed::Sealed for Cb11SequencerQueueDepth {}

impl CircuitBreaker for Cb11SequencerQueueDepth {
    fn id(&self) -> &'static str {
        "CB-11"
    }

    fn check(&self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        if ctx.sequencer_queue_depth > ctx.config.max_sequencer_queue_depth {
            return reject(
                "CB-11",
                format!(
                    "sequencer queue depth {} exceeds max {}",
                    ctx.sequencer_queue_depth, ctx.config.max_sequencer_queue_depth
                ),
            );
        }
        CircuitBreakerResult::Pass
    }

    fn reset(&mut self, _reason: ResetReason) {}
    fn is_tripped(&self) -> bool {
        false
    }
}

// ─── Circuit Breaker Bank ─────────────────────────────────────────────────────

/// Runs all 11 compiled-in circuit breakers in a single call.
///
/// The bank owns all breaker state. The Sequencer owns the bank. No external crate
/// can register new breakers (sealed trait).
///
/// **Check order:**
/// 1. Latched HaltAll (CB-4, CB-5) → early return
/// 2. Latched Reject (CB-3) → early return
/// 3. CB-10: dry-run bypass (hard invariant, checked before P&L or order state)
/// 4. CB-4/CB-5: daily/hourly loss (fresh evaluation, latch on HaltAll)
/// 5. CB-3: firm gross notional (fresh evaluation, latch on Reject)
/// 6. CB-11: queue depth (backpressure, before per-order checks)
/// 7. CB-1: single order notional
/// 8. CB-2: instrument position
/// 9. CB-6: firm order rate
/// 10. CB-7: agent order rate
/// 11. CB-8: venue error rate
/// 12. CB-9: venue disconnected
pub struct CircuitBreakerBank {
    cb1: Cb1MaxSingleOrderNotional,
    cb2: Cb2MaxInstrumentPosition,
    cb3: Cb3MaxFirmGrossNotional,
    cb4: Cb4FirmDailyLoss,
    cb5: Cb5FirmHourlyLoss,
    cb6: Cb6FirmOrdersPerMinute,
    cb7: Cb7AgentOrdersPerMinute,
    cb8: Cb8VenueErrorRate,
    cb9: Cb9VenueDisconnected,
    cb10: Cb10DryRunBypass,
    cb11: Cb11SequencerQueueDepth,
}

impl CircuitBreakerBank {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cb1: Cb1MaxSingleOrderNotional,
            cb2: Cb2MaxInstrumentPosition,
            cb3: Cb3MaxFirmGrossNotional::new(),
            cb4: Cb4FirmDailyLoss::new(),
            cb5: Cb5FirmHourlyLoss::new(),
            cb6: Cb6FirmOrdersPerMinute,
            cb7: Cb7AgentOrdersPerMinute,
            cb8: Cb8VenueErrorRate,
            cb9: Cb9VenueDisconnected,
            cb10: Cb10DryRunBypass,
            cb11: Cb11SequencerQueueDepth,
        }
    }

    /// Run all 11 circuit breakers in order. Returns on the first non-Pass result.
    ///
    /// Stateful breakers (CB-3, CB-4, CB-5) are latched inside this method when
    /// their fresh check triggers.
    ///
    /// **Check order:**
    /// 1. Latched HaltAll (CB-4, CB-5) — early return; these supersede all other checks.
    /// 2. Latched Reject (CB-3) — early return.
    /// 3. CB-10: dry-run bypass — checked before any fresh P&L evaluation, but only
    ///    reached when no latch is already active (see steps 1–2).
    /// 4. CB-4 / CB-5: fresh P&L evaluation; latch on HaltAll.
    /// 5. CB-3: fresh gross notional evaluation; latch on Reject.
    ///    6–12. Per-order / rate / connectivity checks (CB-11, CB-1, CB-2, CB-6–CB-9).
    pub fn check_all(&mut self, ctx: &CircuitBreakerContext<'_>) -> CircuitBreakerResult {
        // 0. Cancels never increase exposure — they bypass all circuit breakers.
        if ctx.operation_kind == crate::OperationKind::Cancel {
            return CircuitBreakerResult::Pass;
        }

        let is_reduce_only = ctx.operation_kind == crate::OperationKind::ReduceOnly;

        // 1. Early return on latched HaltAll (CB-4 > CB-5 — daily loss takes precedence).
        //    ReduceOnly operations are allowed through so resting positions can be flattened.
        if self.cb4.is_tripped() && !is_reduce_only {
            let agent = ctx.order.map(|o| o.core().agent_id.to_string());
            warn!(
                breaker = "CB-4",
                agent_id = ?agent,
                "HALT active: order blocked by latched daily loss halt"
            );
            return halt(
                "CB-4",
                "daily loss halt latched — manual reset + CIO approval required",
            );
        }
        if self.cb5.is_tripped() && !is_reduce_only {
            let agent = ctx.order.map(|o| o.core().agent_id.to_string());
            warn!(
                breaker = "CB-5",
                agent_id = ?agent,
                "HALT active: order blocked by latched hourly loss halt"
            );
            return halt("CB-5", "hourly loss halt latched — manual reset required");
        }
        // 2. Early return on latched Reject (CB-3).
        //    ReduceOnly operations are allowed through: they reduce gross notional, which
        //    is exactly what is needed to clear the CB-3 breach condition.
        if self.cb3.is_tripped() && !is_reduce_only {
            let agent = ctx.order.map(|o| o.core().agent_id.to_string());
            warn!(
                breaker = "CB-3",
                agent_id = ?agent,
                "order blocked by latched firm gross notional limit"
            );
            return reject(
                "CB-3",
                "firm gross notional limit latched — manual reset required",
            );
        }

        // 3. CB-10: dry-run bypass (hard invariant; only reached when no latch is active)
        let r = self.cb10.check(ctx);
        if r != CircuitBreakerResult::Pass {
            return r;
        }

        // 4. CB-4: daily loss (latch on HaltAll).
        //    ReduceOnly skips the fresh evaluation — it is de-risking, not adding exposure.
        if !is_reduce_only {
            let r = self.cb4.check(ctx);
            if r != CircuitBreakerResult::Pass {
                if matches!(r, CircuitBreakerResult::HaltAll { .. }) {
                    self.cb4.latch();
                    error!(
                        breaker = "CB-4",
                        daily_pnl = %ctx.firm_book.daily_pnl,
                        nav = %ctx.firm_book.nav,
                        "HALT ALL: daily loss circuit breaker latched"
                    );
                }
                return r;
            }
        }

        // 5. CB-5: hourly loss (latch on HaltAll).
        //    ReduceOnly skips the fresh evaluation for the same reason.
        if !is_reduce_only {
            let r = self.cb5.check(ctx);
            if r != CircuitBreakerResult::Pass {
                if matches!(r, CircuitBreakerResult::HaltAll { .. }) {
                    self.cb5.latch();
                    error!(
                        breaker = "CB-5",
                        hourly_pnl = %ctx.firm_book.hourly_pnl,
                        nav = %ctx.firm_book.nav,
                        "HALT ALL: hourly loss circuit breaker latched"
                    );
                }
                return r;
            }
        }

        // 6. CB-3: firm gross notional (latch on Reject).
        //    ReduceOnly skips: a sell/cover reduces gross notional, which is the desired outcome
        //    when CB-3 is near or above its limit.
        if !is_reduce_only {
            let r = self.cb3.check(ctx);
            if r != CircuitBreakerResult::Pass {
                if matches!(r, CircuitBreakerResult::Reject { .. }) {
                    self.cb3.latch();
                    error!(
                        breaker = "CB-3",
                        gross_notional = %ctx.firm_book.gross_notional,
                        limit = %ctx.config.max_firm_gross_notional,
                        "firm gross notional circuit breaker latched — all new orders blocked"
                    );
                }
                return r;
            }
        }

        // 7. CB-11: queue depth (backpressure before per-order work)
        let r = self.cb11.check(ctx);
        if r != CircuitBreakerResult::Pass {
            warn!(
                breaker = "CB-11",
                queue_depth = ctx.sequencer_queue_depth,
                limit = ctx.config.max_sequencer_queue_depth,
                "sequencer backpressure: order rejected"
            );
            return r;
        }

        // 8–12: per-order checks (require an order in context).
        // CB-1 (per-order notional cap) is skipped for ReduceOnly: a flatten order whose
        // notional exceeds the single-order cap must not be blocked — the engine would be
        // stuck holding risk during a halt. The position-size breach that triggered the
        // flatten is already captured by CB-3/4/5 latch logic.
        if !is_reduce_only {
            let r = self.cb1.check(ctx);
            if r != CircuitBreakerResult::Pass {
                if let CircuitBreakerResult::Reject { ref reason, .. } = r {
                    warn!(
                        breaker = "CB-1",
                        reason, "order rejected by circuit breaker"
                    );
                }
                return r;
            }
        }

        let r = self.cb2.check(ctx);
        if r != CircuitBreakerResult::Pass {
            if let CircuitBreakerResult::Reject { ref reason, .. } = r {
                warn!(
                    breaker = "CB-2",
                    reason, "order rejected by circuit breaker"
                );
            }
            return r;
        }

        let r = self.cb6.check(ctx);
        if r != CircuitBreakerResult::Pass {
            if let CircuitBreakerResult::Reject { ref reason, .. } = r {
                warn!(
                    breaker = "CB-6",
                    reason, "order rejected by circuit breaker"
                );
            }
            return r;
        }

        let r = self.cb7.check(ctx);
        if r != CircuitBreakerResult::Pass {
            if let CircuitBreakerResult::Reject { ref reason, .. } = r {
                warn!(
                    breaker = "CB-7",
                    reason, "order rejected by circuit breaker"
                );
            }
            return r;
        }

        let r = self.cb8.check(ctx);
        if r != CircuitBreakerResult::Pass {
            if let CircuitBreakerResult::Reject { ref reason, .. } = r {
                warn!(
                    breaker = "CB-8",
                    reason, "order rejected by circuit breaker"
                );
            }
            return r;
        }

        let r = self.cb9.check(ctx);
        if r != CircuitBreakerResult::Pass {
            if let CircuitBreakerResult::Reject { ref reason, .. } = r {
                warn!(
                    breaker = "CB-9",
                    reason, "order rejected by circuit breaker"
                );
            }
        }
        r
    }

    /// Reset a stateful circuit breaker by ID.
    ///
    /// Only CB-3, CB-4, and CB-5 are resettable (they are the only latching breakers).
    /// Returns `Err` if the breaker ID is unrecognized or not resettable.
    pub fn reset(&mut self, breaker_id: &str, reason: ResetReason) -> crate::Result<()> {
        match breaker_id {
            "CB-3" => {
                self.cb3.reset(reason);
                Ok(())
            }
            // CB-4 (daily loss halt) requires explicit CIO approval — ManualOperator is rejected.
            "CB-4" => match reason {
                ResetReason::CioApproval { .. } => {
                    self.cb4.reset(reason);
                    Ok(())
                }
                _ => Err(crate::RiskError::Rejected(
                    "CB-4 (daily loss halt) requires CIO approval to reset — use ResetReason::CioApproval".into(),
                )),
            },
            "CB-5" => {
                self.cb5.reset(reason);
                Ok(())
            }
            other => Err(crate::RiskError::Rejected(format!(
                "circuit breaker {other} is not resettable or does not exist"
            ))),
        }
    }

    /// Whether any HaltAll breaker is currently latched.
    #[must_use]
    pub fn is_halt_active(&self) -> bool {
        self.cb4.is_tripped() || self.cb5.is_tripped()
    }

    /// Whether any stateful breaker is currently tripped.
    #[must_use]
    pub fn any_tripped(&self) -> bool {
        self.cb3.is_tripped() || self.cb4.is_tripped() || self.cb5.is_tripped()
    }
}

impl Default for CircuitBreakerBank {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::{
        CircuitBreakerConfig, CircuitBreakerContext, CircuitBreakerResult, ExecutionMode,
        ResetReason, VenueErrorStats,
    };
    use types::{
        AgentAllocation, AgentId, AgentStatus, ExecHint, FirmBook, InstrumentExposure,
        InstrumentId, OrderCore, OrderId, OrderType, PipelineOrder, PredictionExposureSummary,
        Side, SimulatedClock, TimeInForce, VenueConnectionHealth, VenueId,
    };

    // ── Fixture builders ──────────────────────────────────────────────────────

    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_single_order_notional: dec!(100_000),
            max_instrument_position_notional: dec!(500_000),
            max_firm_gross_notional: dec!(1_000_000),
            max_daily_loss_pct: dec!(0.05),
            max_hourly_loss_pct: dec!(0.02),
            max_firm_orders_per_min: 100,
            max_agent_orders_per_min: 20,
            max_venue_error_rate: dec!(0.5),
            max_venue_disconnect_secs: 30,
            max_sequencer_queue_depth: 1000,
        }
    }

    fn test_firm_book() -> FirmBook {
        FirmBook {
            nav: dec!(1_000_000),
            gross_notional: dec!(200_000),
            net_notional: dec!(100_000),
            realized_pnl: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            daily_pnl: Decimal::ZERO,
            hourly_pnl: Decimal::ZERO,
            current_drawdown_pct: Decimal::ZERO,
            allocated_capital: dec!(500_000),
            cash_available: dec!(800_000),
            total_fees_paid: Decimal::ZERO,
            agent_allocations: HashMap::from([(
                AgentId("satoshi".into()),
                AgentAllocation {
                    allocated_capital: dec!(100_000),
                    used_capital: Decimal::ZERO,
                    free_capital: dec!(100_000),
                    realized_pnl: Decimal::ZERO,
                    unrealized_pnl: Decimal::ZERO,
                    current_drawdown: Decimal::ZERO,
                    max_drawdown_limit: dec!(-0.15),
                    status: AgentStatus::Active,
                },
            )]),
            instruments: Vec::new(),
            prediction_exposure: PredictionExposureSummary {
                total_notional: Decimal::ZERO,
                pct_of_nav: Decimal::ZERO,
                unresolved_markets: 0,
            },
            as_of: Utc::now(),
        }
    }

    fn test_order(qty: Decimal, price: Decimal) -> PipelineOrder<types::Validated> {
        test_order_for_agent(AgentId("satoshi".into()), qty, price)
    }

    fn test_order_for_agent(
        agent_id: AgentId,
        qty: Decimal,
        price: Decimal,
    ) -> PipelineOrder<types::Validated> {
        PipelineOrder::new(OrderCore {
            id: OrderId("ord-1".into()),
            basket_id: None,
            agent_id,
            instrument_id: InstrumentId("BTC-USD".into()),
            market_id: types::MarketId("BTC-USD".into()),
            venue_id: VenueId("bybit".into()),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            qty,
            price: Some(price),
            trigger_price: None,
            stop_loss: Some(price - dec!(5)),
            take_profit: None,
            post_only: false,
            reduce_only: false,
            dry_run: true,
            exec_hint: ExecHint::default(),
            created_at: Utc::now(),
        })
        .into_validated(Utc::now())
    }

    fn connected_venue_health() -> VenueConnectionHealth {
        let mut h = VenueConnectionHealth::new(VenueId("bybit".into()));
        h.state = types::ConnectionState::Connected;
        h.last_heartbeat_at = Some(Utc::now());
        h
    }

    fn test_ctx<'a>(
        order: Option<&'a PipelineOrder<types::Validated>>,
        firm_book: &'a FirmBook,
        config: &'a CircuitBreakerConfig,
        clock: &'a dyn types::Clock,
        venue_health: &'a HashMap<VenueId, VenueConnectionHealth>,
        venue_error_stats: &'a HashMap<VenueId, VenueErrorStats>,
        agent_orders: &'a HashMap<AgentId, u32>,
    ) -> CircuitBreakerContext<'a> {
        CircuitBreakerContext {
            order,
            firm_book,
            venue_health,
            venue_error_stats,
            firm_orders_last_60s: 0,
            agent_orders_last_60s: agent_orders,
            sequencer_queue_depth: 0,
            execution_mode: ExecutionMode::Paper,
            operation_kind: crate::OperationKind::NewOrder,
            config,
            clock,
        }
    }

    // ── CB-1: Max single order notional ───────────────────────────────────────

    #[test]
    fn cb1_below_limit_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(99_999)); // 99_999 < 100_000
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb1MaxSingleOrderNotional;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb1_exactly_at_limit_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100_000)); // exactly at limit
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb1MaxSingleOrderNotional;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb1_above_limit_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100_001)); // just over limit
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb1MaxSingleOrderNotional;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-1",
                ..
            }
        ));
    }

    // ── CB-2: Max position per instrument ────────────────────────────────────

    #[test]
    fn cb2_no_existing_exposure_below_limit_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(4), dec!(100_000)); // 400_000 < 500_000
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb2MaxInstrumentPosition;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb2_existing_exposure_pushes_over_limit_rejects() {
        let config = test_config();
        let mut book = test_firm_book();
        // Existing 400_000 exposure on BTC-USD
        book.instruments.push(InstrumentExposure {
            instrument: InstrumentId("BTC-USD".into()),
            net_qty: dec!(4),
            gross_qty: dec!(4),
            net_notional_usd: dec!(400_000),
            gross_notional_usd: dec!(400_000),
            by_venue: Vec::new(),
            by_agent: Vec::new(),
        });
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(2), dec!(100_000)); // 200_000 would push to 600_000 > 500_000
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb2MaxInstrumentPosition;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-2",
                ..
            }
        ));
    }

    // ── CB-3: Max firm gross notional ────────────────────────────────────────

    #[test]
    fn cb3_below_limit_passes() {
        let config = test_config();
        let book = test_firm_book(); // gross_notional = 200_000 < 1_000_000
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb3MaxFirmGrossNotional::new();
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb3_above_limit_rejects() {
        let config = test_config();
        let mut book = test_firm_book();
        book.gross_notional = dec!(1_000_001); // just over 1_000_000
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb3MaxFirmGrossNotional::new();
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-3",
                ..
            }
        ));
    }

    #[test]
    fn cb3_latches_and_stays_rejected_after_reset_removes_latch() {
        let config = test_config();
        let mut book = test_firm_book();
        book.gross_notional = dec!(1_000_001);
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut bank = CircuitBreakerBank::new();

        // Trip CB-3 via bank
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let r = bank.check_all(&ctx);
        assert!(
            matches!(
                r,
                CircuitBreakerResult::Reject {
                    breaker: "CB-3",
                    ..
                }
            ),
            "expected CB-3 reject, got {r:?}"
        );
        assert!(bank.cb3.is_tripped());

        // Even with gross_notional corrected, latch holds
        book.gross_notional = dec!(100_000);
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let r = bank.check_all(&ctx);
        assert!(
            matches!(
                r,
                CircuitBreakerResult::Reject {
                    breaker: "CB-3",
                    ..
                }
            ),
            "latch should hold"
        );

        // Manual reset clears the latch
        bank.reset(
            "CB-3",
            ResetReason::ManualOperator {
                operator: "ops".into(),
            },
        )
        .unwrap();
        assert!(!bank.cb3.is_tripped());
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        assert_eq!(bank.check_all(&ctx), CircuitBreakerResult::Pass);
    }

    // ── CB-4: Firm daily loss ─────────────────────────────────────────────────

    #[test]
    fn cb4_below_daily_loss_cap_passes() {
        let config = test_config();
        let mut book = test_firm_book();
        book.daily_pnl = dec!(-49_999); // NAV=1_000_000, 5% = 50_000 → -49_999 ok
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb4FirmDailyLoss::new();
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb4_at_daily_loss_cap_passes() {
        let config = test_config();
        let mut book = test_firm_book();
        book.daily_pnl = dec!(-50_000); // exactly at limit (not strictly less than)
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb4FirmDailyLoss::new();
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb4_exceeds_daily_loss_cap_halts_all() {
        let config = test_config();
        let mut book = test_firm_book();
        book.daily_pnl = dec!(-50_001); // just over 5% of 1_000_000
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb4FirmDailyLoss::new();
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::HaltAll {
                breaker: "CB-4",
                ..
            }
        ));
    }

    #[test]
    fn cb4_haltall_latches_via_bank() {
        let config = test_config();
        let mut book = test_firm_book();
        book.daily_pnl = dec!(-50_001);
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut bank = CircuitBreakerBank::new();

        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let r = bank.check_all(&ctx);
        assert!(
            matches!(
                r,
                CircuitBreakerResult::HaltAll {
                    breaker: "CB-4",
                    ..
                }
            ),
            "got {r:?}"
        );
        assert!(bank.is_halt_active());

        // After P&L corrects, latch still holds
        book.daily_pnl = Decimal::ZERO;
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let r = bank.check_all(&ctx);
        assert!(matches!(
            r,
            CircuitBreakerResult::HaltAll {
                breaker: "CB-4",
                ..
            }
        ));

        // ManualOperator is rejected for CB-4 — CIO approval required
        assert!(bank
            .reset(
                "CB-4",
                ResetReason::ManualOperator {
                    operator: "ops".into()
                }
            )
            .is_err());
        assert!(
            bank.is_halt_active(),
            "latch must hold after rejected reset"
        );

        // CIO approval clears the latch
        bank.reset(
            "CB-4",
            ResetReason::CioApproval {
                approver: "cio".into(),
            },
        )
        .unwrap();
        assert!(!bank.is_halt_active());
    }

    // ── CB-5: Firm hourly loss ────────────────────────────────────────────────

    #[test]
    fn cb5_below_hourly_loss_cap_passes() {
        let config = test_config();
        let mut book = test_firm_book();
        book.hourly_pnl = dec!(-19_999); // 2% of 1_000_000 = 20_000 → ok
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb5FirmHourlyLoss::new();
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb5_exceeds_hourly_loss_cap_halts_all() {
        let config = test_config();
        let mut book = test_firm_book();
        book.hourly_pnl = dec!(-20_001); // just over 2% of 1_000_000
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        let cb = Cb5FirmHourlyLoss::new();
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::HaltAll {
                breaker: "CB-5",
                ..
            }
        ));
    }

    // ── CB-6: Firm order rate ─────────────────────────────────────────────────

    #[test]
    fn cb6_below_rate_limit_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        ctx.firm_orders_last_60s = 100; // exactly at limit
        let cb = Cb6FirmOrdersPerMinute;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb6_above_rate_limit_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        ctx.firm_orders_last_60s = 101;
        let cb = Cb6FirmOrdersPerMinute;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-6",
                ..
            }
        ));
    }

    // ── CB-7: Agent order rate ────────────────────────────────────────────────

    #[test]
    fn cb7_below_agent_rate_limit_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100));
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let mut agent_orders: HashMap<AgentId, u32> = HashMap::new();
        agent_orders.insert(AgentId("satoshi".into()), 20); // exactly at limit
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb7AgentOrdersPerMinute;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb7_above_agent_rate_limit_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100));
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let mut agent_orders: HashMap<AgentId, u32> = HashMap::new();
        agent_orders.insert(AgentId("satoshi".into()), 21);
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb7AgentOrdersPerMinute;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-7",
                ..
            }
        ));
    }

    // ── CB-8: Venue error rate ────────────────────────────────────────────────

    #[test]
    fn cb8_below_error_rate_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100));
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let mut stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        stats.insert(
            VenueId("bybit".into()),
            VenueErrorStats {
                errors_last_5min: 4,
                total_last_5min: 10,
            },
        ); // 40% < 50%
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb8VenueErrorRate;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb8_above_error_rate_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100));
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let mut stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        stats.insert(
            VenueId("bybit".into()),
            VenueErrorStats {
                errors_last_5min: 6,
                total_last_5min: 10,
            },
        ); // 60% > 50%
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb8VenueErrorRate;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-8",
                ..
            }
        ));
    }

    // ── CB-9: Venue disconnected ──────────────────────────────────────────────

    #[test]
    fn cb9_connected_recently_passes() {
        let config = test_config();
        let book = test_firm_book();
        let order = test_order(dec!(1), dec!(100));
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        // connected_venue_health uses Utc::now() for last_heartbeat; use WallClock so
        // elapsed time is sub-second and always within the 30-second disconnect window.
        let wall_clock = types::WallClock;
        let ctx = CircuitBreakerContext {
            order: Some(&order),
            firm_book: &book,
            venue_health: &health,
            venue_error_stats: &stats,
            firm_orders_last_60s: 0,
            agent_orders_last_60s: &agent_orders,
            sequencer_queue_depth: 0,
            execution_mode: ExecutionMode::Paper,
            operation_kind: crate::OperationKind::NewOrder,
            config: &config,
            clock: &wall_clock,
        };
        let cb = Cb9VenueDisconnected;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb9_stale_heartbeat_rejects() {
        let config = test_config();
        let book = test_firm_book();
        // Use a clock 60 seconds past the heartbeat
        let hb_time = Utc::now();
        let clock_time = hb_time + chrono::Duration::seconds(60);
        let clock = SimulatedClock::new(clock_time);
        let order = test_order(dec!(1), dec!(100));
        let mut health_record = VenueConnectionHealth::new(VenueId("bybit".into()));
        health_record.last_heartbeat_at = Some(hb_time);
        let health = HashMap::from([(VenueId("bybit".into()), health_record)]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb9VenueDisconnected;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-9",
                ..
            }
        ));
    }

    #[test]
    fn cb9_no_health_record_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = test_order(dec!(1), dec!(100));
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb9VenueDisconnected;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-9",
                ..
            }
        ));
    }

    // ── CB-10: Dry-run bypass ─────────────────────────────────────────────────

    #[test]
    fn cb10_dry_run_true_in_paper_mode_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        // dry_run = true (default in test_order)
        let order = test_order(dec!(1), dec!(100));
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb10DryRunBypass;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb10_dry_run_false_in_paper_mode_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = PipelineOrder::new(OrderCore {
            id: OrderId("ord-1".into()),
            basket_id: None,
            agent_id: AgentId("satoshi".into()),
            instrument_id: InstrumentId("BTC-USD".into()),
            market_id: types::MarketId("BTC-USD".into()),
            venue_id: VenueId("bybit".into()),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            qty: dec!(1),
            price: Some(dec!(100)),
            trigger_price: None,
            stop_loss: None,
            take_profit: None,
            post_only: false,
            reduce_only: false,
            dry_run: false, // ← the critical flag
            exec_hint: ExecHint::default(),
            created_at: Utc::now(),
        })
        .into_validated(Utc::now());
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb10DryRunBypass;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-10",
                ..
            }
        ));
    }

    #[test]
    fn cb10_dry_run_false_in_live_mode_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let order = PipelineOrder::new(OrderCore {
            id: OrderId("ord-1".into()),
            basket_id: None,
            agent_id: AgentId("satoshi".into()),
            instrument_id: InstrumentId("BTC-USD".into()),
            market_id: types::MarketId("BTC-USD".into()),
            venue_id: VenueId("bybit".into()),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            qty: dec!(1),
            price: Some(dec!(100)),
            trigger_price: None,
            stop_loss: None,
            take_profit: None,
            post_only: false,
            reduce_only: false,
            dry_run: false,
            exec_hint: ExecHint::default(),
            created_at: Utc::now(),
        })
        .into_validated(Utc::now());
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        // In LIVE mode, dry_run=false is allowed
        let ctx = CircuitBreakerContext {
            order: Some(&order),
            firm_book: &book,
            venue_health: &health,
            venue_error_stats: &stats,
            firm_orders_last_60s: 0,
            agent_orders_last_60s: &agent_orders,
            sequencer_queue_depth: 0,
            execution_mode: ExecutionMode::Live,
            operation_kind: crate::OperationKind::NewOrder,
            config: &config,
            clock: &clock,
        };
        let cb = Cb10DryRunBypass;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    // ── CB-11: Sequencer queue depth ─────────────────────────────────────────

    #[test]
    fn cb11_below_queue_depth_passes() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        ctx.sequencer_queue_depth = 1000; // exactly at limit
        let cb = Cb11SequencerQueueDepth;
        assert_eq!(cb.check(&ctx), CircuitBreakerResult::Pass);
    }

    #[test]
    fn cb11_above_queue_depth_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        ctx.sequencer_queue_depth = 1001;
        let cb = Cb11SequencerQueueDepth;
        assert!(matches!(
            cb.check(&ctx),
            CircuitBreakerResult::Reject {
                breaker: "CB-11",
                ..
            }
        ));
    }

    // ── CB-2: side-aware projection ───────────────────────────────────────────

    #[test]
    fn cb2_sell_against_long_reduces_exposure_and_passes() {
        // Book is long 400_000 on BTC-USD (close to the 500_000 cap).
        // A sell order should reduce exposure and must NOT be rejected.
        let config = test_config(); // max_instrument_position_notional = 500_000
        let mut book = test_firm_book();
        book.instruments.push(types::InstrumentExposure {
            instrument: InstrumentId("BTC-USD".into()),
            net_qty: dec!(4),
            gross_qty: dec!(4),
            net_notional_usd: dec!(400_000),
            gross_notional_usd: dec!(400_000),
            by_venue: Vec::new(),
            by_agent: Vec::new(),
        });
        let clock = SimulatedClock::at_epoch();
        // Sell 1 unit @ 100_000 — notional = 100_000, reduces gross to 300_000
        let order = PipelineOrder::new(OrderCore {
            id: OrderId("ord-sell".into()),
            basket_id: None,
            agent_id: AgentId("satoshi".into()),
            instrument_id: InstrumentId("BTC-USD".into()),
            market_id: types::MarketId("BTC-USD".into()),
            venue_id: VenueId("bybit".into()),
            side: types::Side::Sell,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            qty: dec!(1),
            price: Some(dec!(100_000)),
            trigger_price: None,
            stop_loss: None,
            take_profit: None,
            post_only: false,
            reduce_only: true,
            dry_run: true,
            exec_hint: ExecHint::default(),
            created_at: Utc::now(),
        })
        .into_validated(Utc::now());
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb2MaxInstrumentPosition;
        assert_eq!(
            cb.check(&ctx),
            CircuitBreakerResult::Pass,
            "sell against long should pass CB-2"
        );
    }

    // ── CB-9: state check ─────────────────────────────────────────────────────

    #[test]
    fn cb9_disconnected_state_rejects_even_with_recent_heartbeat() {
        let config = test_config();
        let book = test_firm_book();
        let order = test_order(dec!(1), dec!(100));
        // Heartbeat is very recent, but venue is in Disconnected state.
        let mut health_record = VenueConnectionHealth::new(VenueId("bybit".into()));
        health_record.last_heartbeat_at = Some(Utc::now());
        health_record.state = types::ConnectionState::Disconnected;
        let health = HashMap::from([(VenueId("bybit".into()), health_record)]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let wall_clock = types::WallClock;
        let ctx = CircuitBreakerContext {
            order: Some(&order),
            firm_book: &book,
            venue_health: &health,
            venue_error_stats: &stats,
            firm_orders_last_60s: 0,
            agent_orders_last_60s: &agent_orders,
            sequencer_queue_depth: 0,
            execution_mode: ExecutionMode::Paper,
            operation_kind: crate::OperationKind::NewOrder,
            config: &config,
            clock: &wall_clock,
        };
        let cb = Cb9VenueDisconnected;
        assert!(
            matches!(
                cb.check(&ctx),
                CircuitBreakerResult::Reject {
                    breaker: "CB-9",
                    ..
                }
            ),
            "disconnected venue must be rejected even with a fresh heartbeat"
        );
    }

    // ── Bank: Cancel / ReduceOnly bypass of HaltAll ───────────────────────────

    #[test]
    fn cancel_bypasses_latched_cb4_halt() {
        let config = test_config();
        let mut book = test_firm_book();
        book.daily_pnl = dec!(-50_001); // trips CB-4
        let clock = SimulatedClock::at_epoch();
        let health: HashMap<VenueId, VenueConnectionHealth> = HashMap::new();
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut bank = CircuitBreakerBank::new();

        // First call trips and latches CB-4
        let ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        assert!(matches!(
            bank.check_all(&ctx),
            CircuitBreakerResult::HaltAll {
                breaker: "CB-4",
                ..
            }
        ));
        assert!(bank.is_halt_active());

        // A Cancel operation must pass even while CB-4 is latched
        let mut cancel_ctx = test_ctx(None, &book, &config, &clock, &health, &stats, &agent_orders);
        cancel_ctx.operation_kind = crate::OperationKind::Cancel;
        assert_eq!(
            bank.check_all(&cancel_ctx),
            CircuitBreakerResult::Pass,
            "Cancel must bypass latched CB-4 halt"
        );
    }

    #[test]
    fn reduce_only_bypasses_latched_cb4_halt() {
        let config = test_config();
        let mut book = test_firm_book();
        book.daily_pnl = dec!(-50_001); // trips CB-4
                                        // Use WallClock so connected_venue_health()'s Utc::now() heartbeat is not in
                                        // the future relative to the clock used by CB-9.
        let wall_clock = types::WallClock;
        let order = test_order(dec!(1), dec!(100));
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let mut bank = CircuitBreakerBank::new();

        // Trip and latch CB-4 (order=None bypasses CB-9)
        let ctx = test_ctx(
            None,
            &book,
            &config,
            &wall_clock,
            &health,
            &stats,
            &agent_orders,
        );
        bank.check_all(&ctx);
        assert!(bank.is_halt_active());

        // A ReduceOnly operation must pass through the latched CB-4 check
        let mut reduce_ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &wall_clock,
            &health,
            &stats,
            &agent_orders,
        );
        reduce_ctx.operation_kind = crate::OperationKind::ReduceOnly;
        assert_eq!(
            bank.check_all(&reduce_ctx),
            CircuitBreakerResult::Pass,
            "ReduceOnly must bypass latched CB-4 halt"
        );
    }

    // ── CB-9: future heartbeat rejects ───────────────────────────────────────

    #[test]
    fn cb9_future_heartbeat_rejects() {
        let config = test_config();
        let book = test_firm_book();
        let order = test_order(dec!(1), dec!(100));
        // Clock is at epoch; heartbeat is 30s later — future relative to the engine clock.
        let clock = SimulatedClock::at_epoch();
        let clock_now: &dyn types::Clock = &clock;
        let mut health_record = VenueConnectionHealth::new(VenueId("bybit".into()));
        health_record.state = types::ConnectionState::Connected;
        health_record.last_heartbeat_at = Some(clock_now.now() + chrono::Duration::seconds(30));
        let health = HashMap::from([(VenueId("bybit".into()), health_record)]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &clock,
            &health,
            &stats,
            &agent_orders,
        );
        let cb = Cb9VenueDisconnected;
        assert!(
            matches!(
                cb.check(&ctx),
                CircuitBreakerResult::Reject {
                    breaker: "CB-9",
                    ..
                }
            ),
            "future heartbeat timestamp must be rejected (fail closed)"
        );
    }

    // ── CB-1 skipped for ReduceOnly ───────────────────────────────────────────

    #[test]
    fn reduce_only_skips_cb1_notional_cap() {
        // Order notional = 200_000, well above the 100_000 CB-1 cap.
        // A NewOrder is rejected; a ReduceOnly must pass CB-1 via check_all.
        let config = test_config(); // max_single_order_notional = 100_000
        let book = test_firm_book();
        // WallClock so connected_venue_health()'s Utc::now() heartbeat is not in the future.
        let wall_clock = types::WallClock;
        let order = test_order(dec!(1), dec!(200_000)); // notional = 200_000 > 100_000
        let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
        let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
        let agent_orders: HashMap<AgentId, u32> = HashMap::new();

        // Sanity: CB-1 alone rejects this order
        let ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &wall_clock,
            &health,
            &stats,
            &agent_orders,
        );
        assert!(
            matches!(
                Cb1MaxSingleOrderNotional.check(&ctx),
                CircuitBreakerResult::Reject {
                    breaker: "CB-1",
                    ..
                }
            ),
            "sanity: CB-1 must reject 200_000 notional order"
        );

        // check_all with ReduceOnly must pass (CB-1 skipped)
        let mut bank = CircuitBreakerBank::new();
        let mut reduce_ctx = test_ctx(
            Some(&order),
            &book,
            &config,
            &wall_clock,
            &health,
            &stats,
            &agent_orders,
        );
        reduce_ctx.operation_kind = crate::OperationKind::ReduceOnly;
        assert_eq!(
            bank.check_all(&reduce_ctx),
            CircuitBreakerResult::Pass,
            "ReduceOnly must skip CB-1 notional cap in check_all"
        );
    }

    // ── Bank: reset validation ────────────────────────────────────────────────

    #[test]
    fn bank_reset_non_resettable_breaker_returns_err() {
        let mut bank = CircuitBreakerBank::new();
        let result = bank.reset(
            "CB-1",
            ResetReason::ManualOperator {
                operator: "ops".into(),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn bank_reset_unknown_breaker_returns_err() {
        let mut bank = CircuitBreakerBank::new();
        let result = bank.reset(
            "CB-99",
            ResetReason::ManualOperator {
                operator: "ops".into(),
            },
        );
        assert!(result.is_err());
    }

    // ── Property test ─────────────────────────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        #[test]
        #[ignore]
        fn prop_valid_inputs_never_panic(
            qty in 1_u64..1_000_u64,
            price in 1_u64..200_000_u64,
            gross_notional in 0_u64..2_000_000_u64,
            // nav=0 exercises the CB-4/CB-5 NAV≤0 guard (silent pass-through)
            nav in 0_u64..2_000_000_u64,
            // loss values above the 5%/2% caps exercise the HaltAll paths
            daily_pnl_loss in 0_u64..200_000_u64,
            hourly_pnl_loss in 0_u64..100_000_u64,
            firm_orders in 0_u32..200_u32,
            queue_depth in 0_usize..2_000_usize,
        ) {
            let config = test_config();
            let mut book = test_firm_book();
            book.gross_notional = Decimal::from(gross_notional);
            book.nav = Decimal::from(nav);
            book.daily_pnl = -Decimal::from(daily_pnl_loss);
            book.hourly_pnl = -Decimal::from(hourly_pnl_loss);
            let clock = SimulatedClock::at_epoch();
            let order = test_order(Decimal::from(qty), Decimal::from(price));
            let health = HashMap::from([(VenueId("bybit".into()), connected_venue_health())]);
            let stats: HashMap<VenueId, VenueErrorStats> = HashMap::new();
            let agent_orders: HashMap<AgentId, u32> = HashMap::new();
            let ctx = CircuitBreakerContext {
                order: Some(&order),
                firm_book: &book,
                venue_health: &health,
                venue_error_stats: &stats,
                firm_orders_last_60s: firm_orders,
                agent_orders_last_60s: &agent_orders,
                sequencer_queue_depth: queue_depth,
                execution_mode: ExecutionMode::Paper,
                operation_kind: crate::OperationKind::NewOrder,
                config: &config,
                clock: &clock,
            };

            let mut bank = CircuitBreakerBank::new();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bank.check_all(&ctx)
            }));
            prop_assert!(result.is_ok(), "check_all panicked on valid input");
        }
    }
}

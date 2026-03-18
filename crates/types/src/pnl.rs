//! Mark-to-market P&L and price source abstraction.
//!
//! The Sequencer queries `PriceSource` synchronously against a cache.
//! An async task (outside the Sequencer) updates the cache from venue feeds.
//! This separation keeps the hot path lock-free and deterministic.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::InstrumentId;

/// Synchronous price lookup — backed by an in-memory cache.
///
/// The Sequencer calls `latest_price()` during risk checks and P&L updates.
/// An async market data task updates the cache via interior mutability
/// (e.g., `DashMap` or `Arc<RwLock<HashMap>>` — implementation choice).
///
/// In backtest mode, the harness pre-loads prices before each tick.
pub trait PriceSource: Send + Sync {
    /// Get the latest cached price for an instrument.
    /// Returns `None` if no price has been received yet (data gap).
    fn latest_price(&self, instrument: &InstrumentId) -> Option<PriceSnapshot>;

    /// Check if price data is considered stale (older than threshold).
    /// Used by risk checks to reject orders when market data is unreliable.
    fn is_stale(&self, instrument: &InstrumentId, max_age: std::time::Duration) -> bool;
}

/// A single price observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSnapshot {
    pub instrument: InstrumentId,
    /// Best bid price.
    pub bid: Decimal,
    /// Best ask price.
    pub ask: Decimal,
    /// Mid price: (bid + ask) / 2.
    pub mid: Decimal,
    /// Last traded price.
    pub last: Decimal,
    /// When this price was observed (from venue timestamp, not local clock).
    pub observed_at: DateTime<Utc>,
    /// When this price was received locally (from Clock::now()).
    pub received_at: DateTime<Utc>,
}

impl PriceSnapshot {
    /// Spread in absolute terms.
    #[must_use]
    pub fn spread(&self) -> Decimal {
        self.ask - self.bid
    }

    /// Spread as basis points of mid price.
    /// Returns None if mid is zero (should not happen in practice).
    #[must_use]
    pub fn spread_bps(&self) -> Option<Decimal> {
        if self.mid.is_zero() {
            return None;
        }
        Some(self.spread() / self.mid * Decimal::new(10000, 0))
    }
}

/// Mark-to-market snapshot for a position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkToMarketSnapshot {
    pub instrument: InstrumentId,
    /// Current position quantity (signed: positive = long, negative = short).
    pub qty: Decimal,
    /// Average entry price.
    pub avg_entry_price: Decimal,
    /// Current mark price (mid price from PriceSource).
    pub mark_price: Decimal,
    /// Unrealized P&L: (mark_price - avg_entry) * qty.
    pub unrealized_pnl: Decimal,
    /// Notional value at current mark: |qty| * mark_price.
    pub notional_value: Decimal,
    /// When this snapshot was computed.
    pub computed_at: DateTime<Utc>,
}

impl MarkToMarketSnapshot {
    /// Compute mark-to-market from position data and current price.
    ///
    /// This is the canonical P&L formula. No other code should compute
    /// unrealized P&L differently.
    #[must_use]
    pub fn compute(
        instrument: InstrumentId,
        qty: Decimal,
        avg_entry_price: Decimal,
        mark_price: Decimal,
        now: DateTime<Utc>,
    ) -> Self {
        let unrealized_pnl = (mark_price - avg_entry_price) * qty;
        let notional_value = qty.abs() * mark_price;
        Self {
            instrument,
            qty,
            avg_entry_price,
            mark_price,
            unrealized_pnl,
            notional_value,
            computed_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_to_market_long_profit() {
        let mtm = MarkToMarketSnapshot::compute(
            InstrumentId("BTC".into()),
            Decimal::new(1, 0),     // 1 BTC long
            Decimal::new(60000, 0), // entry at 60,000
            Decimal::new(65000, 0), // mark at 65,000
            Utc::now(),
        );
        assert_eq!(mtm.unrealized_pnl, Decimal::new(5000, 0));
        assert_eq!(mtm.notional_value, Decimal::new(65000, 0));
    }

    #[test]
    fn test_mark_to_market_short_profit() {
        let mtm = MarkToMarketSnapshot::compute(
            InstrumentId("ETH".into()),
            Decimal::new(-10, 0),  // 10 ETH short
            Decimal::new(3000, 0), // entry at 3,000
            Decimal::new(2800, 0), // mark at 2,800
            Utc::now(),
        );
        // P&L = (2800 - 3000) * (-10) = (-200) * (-10) = 2000
        assert_eq!(mtm.unrealized_pnl, Decimal::new(2000, 0));
    }

    #[test]
    fn test_mark_to_market_loss() {
        let mtm = MarkToMarketSnapshot::compute(
            InstrumentId("BTC".into()),
            Decimal::new(1, 0),
            Decimal::new(60000, 0),
            Decimal::new(55000, 0), // mark below entry
            Utc::now(),
        );
        assert_eq!(mtm.unrealized_pnl, Decimal::new(-5000, 0));
    }

    #[test]
    fn test_price_snapshot_spread() {
        let snap = PriceSnapshot {
            instrument: InstrumentId("BTC".into()),
            bid: Decimal::new(64990, 0),
            ask: Decimal::new(65010, 0),
            mid: Decimal::new(65000, 0),
            last: Decimal::new(65005, 0),
            observed_at: Utc::now(),
            received_at: Utc::now(),
        };
        assert_eq!(snap.spread(), Decimal::new(20, 0));
    }
}

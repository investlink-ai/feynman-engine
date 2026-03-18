//! Fee model — trait and types for fee estimation, calculation, and scheduling.
//!
//! Fees determine whether a trade has edge. The fee model is identical
//! in backtest and live (Design Principle #3).
//!
//! Fee estimates inform execution strategy selection: if the maker rebate
//! exceeds the urgency cost of waiting, prefer limit orders.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::pipeline::{Fill, OrderCore};
use crate::{InstrumentId, VenueId};

/// Typed errors for fee operations.
#[derive(Debug, thiserror::Error)]
pub enum FeeError {
    #[error("no fee schedule for venue {0}")]
    NoSchedule(VenueId),

    #[error("no fee schedule for instrument {0} on venue {1}")]
    NoInstrumentSchedule(InstrumentId, VenueId),

    #[error("fee calculation failed: {0}")]
    Calculation(String),
}

/// Fee model for a trading venue.
///
/// Synchronous — called in the Sequencer hot path during sizing and risk checks.
/// Implementations must be fast (<100μs per call).
pub trait FeeModel: Send + Sync {
    /// Estimate fees for a prospective order (before submission).
    ///
    /// Returns both maker and taker estimates so the sizing engine can
    /// decide whether to use limit or market orders.
    fn estimate(&self, order: &OrderCore) -> std::result::Result<FeeEstimate, FeeError>;

    /// Calculate actual fees for a confirmed fill.
    ///
    /// Uses the fill's `is_maker` flag to select the correct rate.
    fn calculate(&self, fill: &Fill) -> std::result::Result<Fee, FeeError>;

    /// Current fee schedule for a venue.
    fn schedule(&self, venue_id: &VenueId) -> std::result::Result<&FeeSchedule, FeeError>;

    /// Hot-reload fee schedule (e.g., tier upgrade, new fee structure).
    fn update_schedule(&mut self, venue_id: VenueId, schedule: FeeSchedule);
}

/// Pre-trade fee estimate for maker and taker scenarios.
///
/// Used by the sizing engine to compute net expected value:
/// `edge = gross_alpha - worst_case_fee`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeEstimate {
    /// Fee if filled as maker (limit order, typically lower or negative/rebate).
    pub as_maker: Decimal,
    /// Fee if filled as taker (market order, typically higher).
    pub as_taker: Decimal,
    /// Worst-case fee (max of maker/taker, used for conservative sizing).
    pub worst_case: Decimal,
    /// Expected gas/network fee (for on-chain venues).
    pub gas_estimate: Decimal,
}

/// Actual fee computed from a confirmed fill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fee {
    /// Trading fee (positive = cost, negative = rebate).
    pub trading_fee: Decimal,
    /// Gas/network fee (zero for centralized venues).
    pub gas_fee: Decimal,
    /// Rebate earned (zero or negative trading fee decomposed).
    pub rebate: Decimal,
    /// Funding fee (for perpetual contracts, if applicable at fill time).
    pub funding_fee: Decimal,
    /// Net total: trading_fee + gas_fee - rebate + funding_fee.
    pub net: Decimal,
}

/// Fee schedule for a venue/instrument pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeSchedule {
    pub venue_id: VenueId,
    /// Current fee tier (e.g., "VIP-1", "maker-tier-3").
    pub tier: String,
    /// Maker fee rate (e.g., 0.0001 for 1bp). Negative means rebate.
    pub maker_rate: Decimal,
    /// Taker fee rate (e.g., 0.0006 for 6bp).
    pub taker_rate: Decimal,
    /// Gas model for on-chain venues.
    pub gas_model: GasModel,
    /// Per-instrument overrides (some instruments have different rates).
    pub instrument_overrides: Vec<InstrumentFeeOverride>,
}

/// Gas fee model for on-chain venues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GasModel {
    /// No gas fees (centralized exchange).
    None,
    /// Fixed gas fee per transaction.
    Fixed { fee_per_tx: Decimal },
    /// Dynamic gas (estimated from recent blocks).
    Dynamic {
        base_fee: Decimal,
        priority_fee: Decimal,
    },
}

/// Per-instrument fee rate override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentFeeOverride {
    pub instrument_id: InstrumentId,
    pub maker_rate: Decimal,
    pub taker_rate: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_estimate_worst_case() {
        let estimate = FeeEstimate {
            as_maker: Decimal::new(-1, 4),  // -0.0001 (rebate)
            as_taker: Decimal::new(6, 4),   //  0.0006
            worst_case: Decimal::new(6, 4), //  0.0006
            gas_estimate: Decimal::ZERO,
        };
        assert!(estimate.worst_case > estimate.as_maker);
    }

    #[test]
    fn test_fee_net_calculation() {
        let fee = Fee {
            trading_fee: Decimal::new(6, 2), // 0.06
            gas_fee: Decimal::ZERO,
            rebate: Decimal::ZERO,
            funding_fee: Decimal::new(-1, 2), // -0.01
            net: Decimal::new(5, 2),          // 0.05
        };
        assert_eq!(
            fee.net,
            fee.trading_fee + fee.gas_fee - fee.rebate + fee.funding_fee
        );
    }

    #[test]
    fn test_gas_model_none() {
        let schedule = FeeSchedule {
            venue_id: VenueId("bybit".into()),
            tier: "VIP-0".into(),
            maker_rate: Decimal::new(1, 4), // 0.0001
            taker_rate: Decimal::new(6, 4), // 0.0006
            gas_model: GasModel::None,
            instrument_overrides: vec![],
        };
        assert!(matches!(schedule.gas_model, GasModel::None));
    }
}

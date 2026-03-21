//! Signal types — market signals from strategy agents.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{AgentId, BasketId, InstrumentId, SignalId};

/// Market signal from an LLM trader agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: SignalId,
    /// Groups related legs when a signal expands into a multi-leg structure.
    pub basket_id: Option<BasketId>,
    pub agent: AgentId,
    pub instrument: InstrumentId,
    pub direction: Side,
    /// Conviction strength: 0.0 (no conviction) to 1.0 (maximum).
    pub conviction: Decimal,
    /// Optional notional target (engine sizes if absent).
    pub sizing_hint: Option<Decimal>,
    /// Arbitrage type classification.
    pub arb_type: String,
    /// Required for SubmitSignal — engine rejects signals without a stop loss.
    pub stop_loss: Decimal,
    pub take_profit: Option<Decimal>,
    pub thesis: String,
    pub urgency: Urgency,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Urgency {
    Low,
    Normal,
    High,
    Immediate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_serde() {
        let signal = Signal {
            id: SignalId("sig-123".into()),
            basket_id: None,
            agent: AgentId("satoshi".into()),
            instrument: InstrumentId("BTC".into()),
            direction: Side::Buy,
            conviction: Decimal::new(75, 2),
            sizing_hint: Some(Decimal::new(50000, 0)),
            arb_type: "funding_rate".into(),
            stop_loss: Decimal::new(60000, 0),
            take_profit: Some(Decimal::new(70000, 0)),
            thesis: "Negative funding on BTCUSDT".into(),
            urgency: Urgency::Normal,
            metadata: serde_json::json!({"source": "bybit"}),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&signal).unwrap();
        let deserialized: Signal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, signal.id);
    }

    #[test]
    fn test_urgency_ordering() {
        assert!(Urgency::Low < Urgency::Immediate);
        assert!(Urgency::Normal < Urgency::High);
    }
}

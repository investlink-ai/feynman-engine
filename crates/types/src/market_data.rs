//! Orderbook and market-data snapshot types used by venue adapters and paper fills.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{InstrumentId, MarketId, VenueId};

/// Typed errors for invalid market data.
#[derive(Debug, thiserror::Error)]
pub enum MarketDataError {
    #[error("non-positive price on {side} side at level {level}: {price}")]
    NonPositivePrice {
        side: &'static str,
        level: usize,
        price: Decimal,
    },

    #[error("non-positive quantity on {side} side at level {level}: {qty}")]
    NonPositiveQty {
        side: &'static str,
        level: usize,
        qty: Decimal,
    },

    #[error("bids must be sorted descending; level {level} is {current} after {previous}")]
    BidsUnsorted {
        level: usize,
        previous: Decimal,
        current: Decimal,
    },

    #[error("asks must be sorted ascending; level {level} is {current} after {previous}")]
    AsksUnsorted {
        level: usize,
        previous: Decimal,
        current: Decimal,
    },

    #[error("crossed book: best bid {best_bid} > best ask {best_ask}")]
    CrossedBook {
        best_bid: Decimal,
        best_ask: Decimal,
    },
}

/// One orderbook level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: Decimal,
    pub qty: Decimal,
}

impl PriceLevel {
    /// Total notional resting at this level.
    #[must_use]
    pub fn notional(&self) -> Decimal {
        self.price * self.qty
    }
}

/// Full depth snapshot used by paper fill simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookSnapshot {
    pub venue_id: VenueId,
    pub instrument_id: InstrumentId,
    pub market_id: MarketId,
    /// Highest bid first.
    pub bids: Vec<PriceLevel>,
    /// Lowest ask first.
    pub asks: Vec<PriceLevel>,
    /// Venue timestamp for the snapshot.
    pub observed_at: DateTime<Utc>,
    /// Local receipt time.
    pub received_at: DateTime<Utc>,
}

impl OrderbookSnapshot {
    /// Best bid, if any.
    #[must_use]
    pub fn best_bid(&self) -> Option<&PriceLevel> {
        self.bids.first()
    }

    /// Best ask, if any.
    #[must_use]
    pub fn best_ask(&self) -> Option<&PriceLevel> {
        self.asks.first()
    }

    /// Mid price derived from top-of-book.
    #[must_use]
    pub fn mid_price(&self) -> Option<Decimal> {
        let best_bid = self.best_bid()?;
        let best_ask = self.best_ask()?;
        Some((best_bid.price + best_ask.price) / Decimal::new(2, 0))
    }

    /// Validate depth ordering and positive values before using the snapshot for fills.
    pub fn validate(&self) -> std::result::Result<(), MarketDataError> {
        validate_side(&self.bids, "bid", true)?;
        validate_side(&self.asks, "ask", false)?;

        if let (Some(best_bid), Some(best_ask)) = (self.best_bid(), self.best_ask()) {
            if best_bid.price > best_ask.price {
                return Err(MarketDataError::CrossedBook {
                    best_bid: best_bid.price,
                    best_ask: best_ask.price,
                });
            }
        }

        Ok(())
    }
}

fn validate_side(
    levels: &[PriceLevel],
    side: &'static str,
    descending: bool,
) -> std::result::Result<(), MarketDataError> {
    let mut previous_price: Option<Decimal> = None;

    for (index, level) in levels.iter().enumerate() {
        if level.price <= Decimal::ZERO {
            return Err(MarketDataError::NonPositivePrice {
                side,
                level: index,
                price: level.price,
            });
        }
        if level.qty <= Decimal::ZERO {
            return Err(MarketDataError::NonPositiveQty {
                side,
                level: index,
                qty: level.qty,
            });
        }

        if let Some(previous) = previous_price {
            let sorted = if descending {
                previous >= level.price
            } else {
                previous <= level.price
            };
            if !sorted {
                return if descending {
                    Err(MarketDataError::BidsUnsorted {
                        level: index,
                        previous,
                        current: level.price,
                    })
                } else {
                    Err(MarketDataError::AsksUnsorted {
                        level: index,
                        previous,
                        current: level.price,
                    })
                };
            }
        }

        previous_price = Some(level.price);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> OrderbookSnapshot {
        let now = Utc::now();
        OrderbookSnapshot {
            venue_id: VenueId("paper".into()),
            instrument_id: InstrumentId("BTC".into()),
            market_id: MarketId("BTCUSDT".into()),
            bids: vec![
                PriceLevel {
                    price: Decimal::new(10_000, 0),
                    qty: Decimal::new(2, 0),
                },
                PriceLevel {
                    price: Decimal::new(9_990, 0),
                    qty: Decimal::new(3, 0),
                },
            ],
            asks: vec![
                PriceLevel {
                    price: Decimal::new(10_010, 0),
                    qty: Decimal::new(1, 0),
                },
                PriceLevel {
                    price: Decimal::new(10_020, 0),
                    qty: Decimal::new(4, 0),
                },
            ],
            observed_at: now,
            received_at: now,
        }
    }

    #[test]
    fn orderbook_snapshot_mid_price_uses_top_of_book() {
        let book = snapshot();
        assert_eq!(book.mid_price(), Some(Decimal::new(10_005, 0)));
    }

    #[test]
    fn orderbook_snapshot_validation_rejects_unsorted_bids() {
        let mut book = snapshot();
        book.bids.swap(0, 1);
        let err = book.validate().unwrap_err();
        assert!(matches!(err, MarketDataError::BidsUnsorted { .. }));
    }

    #[test]
    fn orderbook_snapshot_validation_rejects_crossed_book() {
        let mut book = snapshot();
        book.bids[0].price = Decimal::new(10_015, 0);
        let err = book.validate().unwrap_err();
        assert!(matches!(err, MarketDataError::CrossedBook { .. }));
    }
}

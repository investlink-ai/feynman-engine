//! Core types for the Feynman Capital execution engine.
//!
//! This crate is the foundation — every other crate depends on it.
//! All financial math uses `rust_decimal::Decimal` — no `f64` for money.
//!
//! # Module structure
//!
//! | Module           | What                                              |
//! |------------------|---------------------------------------------------|
//! | `ids`            | Newtype identifiers, `ClientOrderId` generation   |
//! | `clock`          | `Clock` trait, `WallClock`, `SimulatedClock`       |
//! | `event`          | `SequencedEvent`, `EngineEvent` (event sourcing)   |
//! | `signal`         | Strategy signals from agents                      |
//! | `risk`           | Risk limits, violations, outcomes                 |
//! | `portfolio`      | Firm book, positions, exposure tracking            |
//! | `venue`          | Connection health, heartbeat, timeouts             |
//! | `pnl`            | Price source, mark-to-market                       |
//! | `reconciliation` | Divergence detection and reporting                 |
//! | `health`         | Subsystem health, degradation policies             |
//! | `pipeline`       | Type-state order pipeline, `OrderCore`, `Fill`     |
//! | `fees`           | Fee model trait, estimates, schedules              |

pub mod clock;
pub mod event;
pub mod fees;
pub mod health;
pub mod ids;
pub mod market_data;
pub mod pipeline;
pub mod pnl;
pub mod portfolio;
pub mod reconciliation;
pub mod risk;
pub mod signal;
pub mod venue;

// Re-export everything for ergonomic `use types::OrderId` imports.
pub use clock::*;
pub use event::*;
pub use fees::*;
pub use health::*;
pub use ids::*;
pub use market_data::*;
pub use pipeline::*;
pub use pnl::*;
pub use portfolio::*;
pub use reconciliation::*;
pub use risk::*;
pub use signal::*;
pub use venue::*;

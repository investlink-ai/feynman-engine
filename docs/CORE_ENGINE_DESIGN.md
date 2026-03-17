# Feynman Capital — Core Engine Design

**Version:** 1.2.0-draft
**Status:** Canonical Architecture (2026-03-17) — NautilusTrader hybrid path rejected
**Scope:** Unified Execution Engine + Message Bus + Observability + Multi-Client API
**Language:** Rust (tokio async runtime)

---

> This document defines the contract-level architecture for the Rust core engine that replaces the Node.js executor and SQLite message bus. It covers: unified order model, venue adapters, fee engine, risk gates, event sourcing, the backtest/paper/live execution boundary, observability, and verification strategy.
>
> **This is the canonical architecture.** The NautilusTrader hybrid approach ([HYBRID_ENGINE_ARCHITECTURE.md](./HYBRID_ENGINE_ARCHITECTURE.md)) was evaluated and rejected on 2026-03-17. The engine is fully custom Rust, using per-exchange client crates for venue connectivity.
>
> **Companion docs:** [STRATEGY_ENGINE_BOUNDARY.md](./STRATEGY_ENGINE_BOUNDARY.md) (strategy ↔ engine contract), [SOLUTION-DESIGN.md](./SOLUTION-DESIGN.md) (agent architecture), [ARCHITECTURE.md](./ARCHITECTURE.md) (system topology), [MVP.md](./MVP.md) (phased rollout).

---

## Table of Contents

1. [Design Principles](#1-design-principles)
2. [System Topology](#2-system-topology)
3. [Core Types](#3-core-types)
4. [Unified Order Model](#4-unified-order-model)
5. [Venue Adapter Contract](#5-venue-adapter-contract)
6. [Fee Engine](#6-fee-engine)
7. [Risk Architecture](#7-risk-architecture)
8. [Execution Gateway](#8-execution-gateway)
9. [Journal & State Recovery](#9-journal--state-recovery)
10. [Execution Context (Backtest / Paper / Live)](#10-execution-context)
11. [Message Bus](#11-message-bus)
12. [Position Model](#12-position-model)
13. [Observability & Dashboard](#13-observability--dashboard)
14. [Verification Strategy](#14-verification-strategy)
15. [Concurrency & Parallelism](#15-concurrency--parallelism)
16. [Crate Structure](#16-crate-structure)
17. [Migration Path](#17-migration-path)
18. [Appendix A: Venue Order Type Matrix](#appendix-a-venue-order-type-matrix)

---

## 1. Design Principles

| # | Principle | Rationale |
|---|-----------|-----------|
| 1 | **One trait, three modes** | Strategy code is identical across backtest, paper, and live. The `ExecutionContext` trait is the boundary. If strategy code compiles, it works in all modes. |
| 2 | **Venue is source of truth** | Internal state is a cache. The exchange knows what you actually hold. Reconciliation is continuous, not periodic. |
| 3 | **Fees are first-class** | Fees determine whether a trade has edge. Fee model is identical in backtest and live. Fee estimates inform execution strategy selection. |
| 4 | **Layered risk defense** | Layer 0 (circuit breakers, compiled-in) → Layer 1 (programmatic risk gate, configurable) → Layer 2 (LLM risk agent) → Layer 3 (human CIO). Each layer is independent. |
| 5 | **Journaled order path** | Every state change is written to an append-only journal. Current state is a materialized view with periodic snapshots. Crash recovery replays the journal tail. |
| 6 | **Rust enforces correctness** | Illegal order state transitions don't compile. Financial math uses `rust_decimal`. Concurrency bugs are caught by the type system. |
| 7 | **Adapters are dumb pipes** | No business logic in venue adapters. They translate between canonical format and venue-native API. All intelligence lives in the gateway. |
| 8 | **Design for fast-quant, build for strategic** | Contracts support sub-50ms signal-to-order latency. Current implementation targets 50ms–1s. Architecture doesn't preclude HFT-tier if needed. |
| 9 | **Deterministic event sequencing** | All events flow through a single sequencer. Backtest and live produce identical state given identical inputs. Non-determinism is a bug. |
| 10 | **Idempotent order submission** | `submit(order)` is idempotent on `OrderId`. Duplicate calls return cached ack. No operation in the system produces duplicate orders on retry. |
| 11 | **Explicit over implicit** | No silent fallbacks, no hidden defaults, no magic. If a venue doesn't support trailing stops, return `Err(UnsupportedOrderType)` — don't silently emulate. If a config value is missing, fail startup — don't substitute a default. Every behavior path must be visible in the code and traceable in the logs. Implicit behavior is a production debugging nightmare. |
| 12 | **Emulation is opt-in, not automatic** | Order emulation (trailing stop on a venue that lacks it, bracket decomposition) is available but must be explicitly requested via `exec_hint` or config. The caller decides whether emulation is acceptable — the engine never silently substitutes behavior. |

---

## 2. System Topology

```
┌──────────────────────────────────────────────────────────────────────┐
│                      AGENT LAYER (Node.js / OpenClaw)                │
│                                                                      │
│  Satoshi    Taleb    Feynman    Graham    Soros    (future agents)    │
│     │         │         │         │         │                        │
│     └────┬────┘─────────┘─────────┘─────────┘                        │
│          ▼                                                           │
│   ┌──────────────┐   MCP facade (thin Node.js gRPC client)          │
│   │  MCP Bridge  │──────────────────────────┐                        │
│   └──────────────┘                          │                        │
└─────────────────────────────────────────────┼────────────────────────┘
                                              │ gRPC (tonic)
                                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│                      RUST CORE ENGINE (single binary, tokio)         │
│                                                                      │
│  ┌────────────┐   ┌──────────────┐   ┌─────────────────────────┐    │
│  │  gRPC API  │   │  Message Bus │   │   Execution Gateway     │    │
│  │  (tonic)   │   │  (Redis      │   │                         │    │
│  │            │   │   Streams)   │   │  ┌─────────┐            │    │
│  │  Streams:  │   │              │   │  │ Router  │            │    │
│  │  -signals  │   │  Consumer    │   │  └────┬────┘            │    │
│  │  -approved │   │  groups,     │   │       ▼                 │    │
│  │  -fills    │   │  XPENDING    │   │  ┌─────────┐           │    │
│  │  -system   │   │  reconciler  │   │  │Strategy │           │    │
│  │            │   │              │   │  │Selector │           │    │
│  └────────────┘   └──────────────┘   │  └────┬────┘           │    │
│                                      │       ▼                 │    │
│  ┌──────────────────────────┐        │  ┌─────────────────┐   │    │
│  │    Risk System           │        │  │ Venue Adapters  │   │    │
│  │                          │        │  │                 │   │    │
│  │  L0: Circuit Breakers    │◄──────►│  │  Bybit          │   │    │
│  │  (compiled, immutable)   │        │  │  Hyperliquid    │   │    │
│  │                          │        │  │  Polymarket     │   │    │
│  │  L1: Programmatic Gate   │        │  │  Binance        │   │    │
│  │  (configurable limits)   │        │  │  Alpaca         │   │    │
│  │                          │        │  │  IBKR           │   │    │
│  │  L2: Risk Agent (Taleb)  │        │  │  Deribit        │   │    │
│  │  (adjusts L1 config)     │        │  │  [Simulated]    │   │    │
│  └──────────────────────────┘        │  └─────────────────┘   │    │
│                                      │                         │    │
│                                      │  Order Emulator         │    │
│                                      │  Fee Engine             │    │
│                                      │  Order Lifecycle FSM    │    │
│                                      │  Position Reconciler    │    │
│                                      └─────────────────────────┘    │
│                                                                      │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │  Event Sequencer (deterministic ordering for backtest parity) │    │
│  └──────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Event Store (append-only)  →  State Store (materialized view) │  │
│  │  SQLite (now) / Postgres (later)                               │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Observability: metrics (Prometheus) + traces (OTLP) + events  │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 3. Core Types

### 3.1 Identifiers

Type-safe, opaque identifiers prevent accidental mixing (e.g., passing an `OrderId` where a `VenueOrderId` is expected).

```rust
/// All IDs are newtypes over String. Clone + Hash + Eq + Serialize.
pub struct OrderId(pub String);         // internal idempotency key
pub struct VenueOrderId(pub String);    // exchange-assigned order ID
pub struct AgentId(pub String);         // "satoshi", "graham", etc.
pub struct VenueId(pub String);         // "bybit", "hyperliquid", "polymarket"
pub struct AccountId(pub String);       // sub-account or wallet address
pub struct InstrumentId(pub String);    // unified underlying: "BTC", "ETH", "AAPL"
pub struct SignalId(pub String);        // traces order back to originating signal
pub struct MessageId(pub String);       // bus message identifier
```

### 3.2 Market Identity

```rust
pub struct MarketId {
    pub venue: VenueId,
    pub symbol: String,           // venue-native: "BTCUSDT", "BTC-PERP", etc.
    pub kind: MarketKind,
}

pub enum MarketKind {
    Spot,
    PerpetualSwap,
    Future { expiry: DateTime<Utc> },
    Option {
        underlying: String,
        strike: Decimal,
        expiry: DateTime<Utc>,
        option_type: OptionType,   // Call | Put
    },
    Prediction {
        outcome: PredictionOutcome, // Yes | No
        resolution_source: String,
    },
    CFD,
}

pub enum PredictionOutcome {
    Yes,
    No,
    /// Multi-outcome markets (e.g., "Who wins the election?").
    /// Each option_id maps to a separate CLOB position.
    Categorical { option_id: String },
}
pub enum OptionType { Call, Put }
```

`MarketId` maps to a unified `InstrumentId` via `MarketId::instrument()` for cross-venue exposure calculation.

### 3.3 Time

```rust
/// All timestamps are UTC. Backtest uses a simulated clock.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct WallClock;                          // Live + Paper
pub struct SimulatedClock { current: DateTime<Utc> }  // Backtest
```

### 3.4 Deterministic Event Sequencer

Critical for backtest-live parity. All events — market data ticks, fills, timer callbacks — flow through a single sequencer that assigns monotonic timestamps. In backtest, this guarantees deterministic replay regardless of wall-clock timing.

```rust
/// Single-threaded event sequencer. Every event in the system passes through here.
/// In backtest mode: events are sorted by simulated time → deterministic replay.
/// In live mode: events are stamped with wall clock → causal ordering preserved.
pub trait EventSequencer: Send + Sync {
    /// Ingest an event from any source (market data, fill, timer, reconciliation).
    fn ingest(&self, event: Event);

    /// Drain all queued events in deterministic order.
    /// Backtest: sorted by simulated timestamp, then by source priority.
    /// Live: FIFO (events are already wall-clock ordered).
    fn drain(&self) -> Vec<Event>;
}

/// Event source priority (breaks ties when timestamps are equal in backtest).
pub enum EventPriority {
    MarketData = 0,    // process market state first
    Fill = 1,          // then fills (may depend on market state)
    Timer = 2,         // then scheduled callbacks
    Reconciliation = 3, // then reconciliation corrections
}
```

---

## 4. Unified Order Model

### 4.1 Design Rationale

Every venue represents orders differently:
- Bybit: limit + triggerPrice field makes it conditional
- Binance: 7+ explicit order types (STOP_MARKET, TAKE_PROFIT_LIMIT, etc.)
- IBKR: 20+ orderType strings + conditions + pegged + scale + algo
- Hyperliquid: limit-only with trigger object and grouping semantics
- Polymarket: TIF *is* the order type (GTC, GTD, FOK, FAK)

The unified model separates **intent** (what you want) from **mechanism** (how the venue implements it):

1. **Base order** — side, quantity, pricing
2. **Trigger condition** — when to activate (stop, take-profit, trailing)
3. **Execution constraint** — TIF, post-only, reduce-only, display qty
4. **Linked orders** — TP/SL, bracket, OCO, OTO
5. **Venue hints** — adapter-specific params that don't fit the canonical model

### 4.2 Canonical Order

```rust
pub struct CanonicalOrder {
    // Identity
    pub id: OrderId,                    // idempotency key (client-generated)
    pub agent: AgentId,                 // originating agent
    pub account: AccountId,             // target account/wallet
    pub signal_id: Option<SignalId>,    // traces to originating signal

    // Market
    pub market: MarketId,

    // Intent
    pub side: Side,
    pub qty: OrderQty,
    pub pricing: OrderPricing,

    // Activation (optional — if present, order is conditional)
    pub trigger: Option<TriggerCondition>,

    // Constraints
    pub constraints: OrderConstraints,

    // Linked orders (optional — TP/SL, bracket, OCO)
    pub linked: Option<LinkedOrders>,

    // Execution preference (hint to strategy selector)
    pub exec_hint: ExecHint,

    // Safety
    pub dry_run: bool,                  // DEFAULT: true. Always.

    // Venue-specific overrides (escape hatch — see §4.9 Typed Extensions)
    pub venue_params: VenueParams,

    // Metadata
    pub created_at: DateTime<Utc>,
}
```

### 4.3 Side & Quantity

```rust
pub enum Side { Buy, Sell }

/// Quantity can be expressed in base units, quote units, or notional.
/// Venue adapter translates to venue-native representation.
pub enum OrderQty {
    /// Base asset quantity (e.g., 0.5 BTC)
    Base(Decimal),
    /// Quote asset quantity (e.g., $10,000 USDT) — Binance quoteOrderQty
    Quote(Decimal),
    /// Notional value (e.g., $50,000 USD) — IBKR cashQty
    Notional(Decimal),
    /// Fractional shares — IBKR, Alpaca
    Fractional(Decimal),
    /// Number of contracts — Deribit, futures
    Contracts(Decimal),
}
```

### 4.4 Pricing

```rust
/// How the order is priced. Venue adapter maps to venue-native order type.
pub enum OrderPricing {
    /// Market order — best available price.
    /// On Hyperliquid: aggressive IOC limit. On Polymarket: not supported.
    Market,

    /// Limit at exact price.
    Limit { price: Decimal },

    /// Market-to-limit: executes as market, remainder rests as limit.
    /// Supported: IBKR (MTL), Deribit (market_limit). Simulated elsewhere.
    MarketToLimit,

    /// Pegged to a reference price with optional offset.
    /// Supported: IBKR (PEG MKT, PEG MID, REL, etc.), Bybit (BBO).
    Pegged {
        reference: PegReference,
        offset: Option<Decimal>,       // positive = more aggressive
    },

    /// Priced by implied volatility — options only.
    /// Supported: Deribit (advanced: "implv"), Bybit (orderIv).
    ImpliedVol { iv: Decimal },

    /// Priced in USD notional — options only.
    /// Supported: Deribit (advanced: "usd").
    UsdNotional { price_usd: Decimal },
}

pub enum PegReference {
    Midpoint,       // PEG MID, snap to midpoint
    BestBid,        // PEG MKT (buy side)
    BestAsk,        // PEG MKT (sell side)
    Primary,        // REL / pegged to primary
    LastTrade,
    IndexPrice,
    MarkPrice,
    /// Bybit BBO queue/counterparty with level.
    BBO { side_type: BboSideType, level: u8 },
}

pub enum BboSideType { Queue, Counterparty }
```

### 4.5 Trigger Conditions

Triggers turn a resting order into a conditional order (stop, take-profit, trailing stop).

```rust
/// If present, the order activates only when the trigger condition is met.
pub struct TriggerCondition {
    pub trigger_type: TriggerType,
    pub reference_price: TriggerReference,
}

pub enum TriggerType {
    /// Activates when price crosses trigger level.
    /// Direction inferred from side: buy-stop triggers above, sell-stop below.
    StopLoss { trigger_price: Decimal },

    /// Activates when price reaches profit target.
    TakeProfit { trigger_price: Decimal },

    /// Trailing stop — maintains distance from peak/trough.
    Trailing {
        /// Callback distance. Interpretation depends on `trailing_unit`.
        distance: Decimal,
        unit: TrailingUnit,
        /// Activation price — trailing starts tracking only after this level.
        /// Supported: Binance (activationPrice), Deribit. Optional.
        activation_price: Option<Decimal>,
    },

    /// Market-if-touched — opposite of stop (buy below, sell above).
    /// Supported: IBKR (MIT), simulated elsewhere.
    MarketIfTouched { trigger_price: Decimal },

    /// Limit-if-touched — limit version of MIT.
    /// Supported: IBKR (LIT).
    LimitIfTouched { trigger_price: Decimal },

    /// Time-based activation — order activates at a specific time.
    /// Supported: IBKR (GoodAfterTime).
    TimeActivated { activate_at: DateTime<Utc> },

    /// Complex conditions — IBKR only. Price, time, margin, volume, % change.
    Complex { conditions: Vec<OrderCondition> },
}

pub enum TrailingUnit {
    Absolute,       // dollar/price distance
    Percent,        // percentage distance
    Bips,           // Binance trailingDelta (basis points)
}

pub enum TriggerReference {
    LastPrice,
    MarkPrice,
    IndexPrice,
    /// IBKR trigger methods (double-bid/ask, last, midpoint, etc.)
    VenueDefault,
}

/// IBKR complex conditions.
pub enum OrderCondition {
    Price { contract: String, operator: CondOp, price: Decimal, trigger_method: u8 },
    Time { operator: CondOp, time: DateTime<Utc> },
    Margin { operator: CondOp, cushion_pct: Decimal },
    Volume { contract: String, operator: CondOp, volume: u64, period: Duration },
    PercentChange { contract: String, operator: CondOp, change_pct: Decimal },
}

pub enum CondOp { GreaterThan, LessThan }
```

### 4.6 Order Constraints

```rust
pub struct OrderConstraints {
    pub time_in_force: TimeInForce,
    pub post_only: bool,              // reject if would take liquidity
    pub reduce_only: bool,            // only reduce existing position
    pub close_position: bool,         // close entire position (Binance, Bybit)
    pub hidden: bool,                 // fully hidden order (IBKR)
    pub display_qty: Option<Decimal>, // iceberg visible qty (Binance, IBKR, Deribit)
    pub min_qty: Option<Decimal>,     // minimum fill qty (IBKR)
    pub self_trade_prevention: Option<SelfTradePrevention>,
    pub market_maker_protection: bool, // Bybit options, Deribit
    pub slippage_tolerance: Option<SlippageTolerance>,
}

pub enum TimeInForce {
    /// Good-til-cancelled. Default.
    GTC,
    /// Immediate-or-cancel.
    IOC,
    /// Fill-or-kill — all or nothing.
    FOK,
    /// Day order — cancelled at market close. Alpaca, IBKR.
    Day,
    /// Good-til-date — expires at specified time.
    GTD { expiry: DateTime<Utc> },
    /// At the opening auction. Alpaca (opg), IBKR (OPG).
    AtOpen,
    /// At the closing auction. Alpaca (cls), IBKR (MOC/LOC).
    AtClose,
    /// Auction. IBKR (AUC).
    Auction,
    /// Fill-and-kill — partial fill ok, cancel remainder. Polymarket (FAK).
    FAK,
}

pub enum SelfTradePrevention {
    CancelMaker,        // Bybit, Binance
    CancelTaker,        // Bybit, Binance
    CancelBoth,         // Bybit, Binance
    CancelOldest,
}

pub enum SlippageTolerance {
    Percent(Decimal),
    TickSize(Decimal),
}
```

### 4.7 Linked Orders

Linked orders express relationships between orders: bracket (entry + TP + SL), OCO (one cancels other), OTO (one triggers other).

```rust
/// Linked order group. The adapter translates to venue-native representation:
/// - Bybit: inline TP/SL fields on the parent order
/// - Binance: OTOCO atomic endpoint
/// - IBKR: parentId linking
/// - Deribit: otoco_config array
/// - Hyperliquid: grouping field ("normalTpsl", "positionTpsl")
/// - Alpaca: order_class ("bracket", "oco", "oto")
pub enum LinkedOrders {
    /// Take-profit and/or stop-loss attached to this order.
    /// Most common case. All venues support some form of this.
    TakeProfitStopLoss {
        take_profit: Option<LinkedLeg>,
        stop_loss: Option<LinkedLeg>,
        /// Full position or partial. Bybit tpslMode.
        scope: TpSlScope,
    },

    /// One-cancels-other: two exit orders, first fill cancels the other.
    OCO {
        legs: [LinkedLeg; 2],
    },

    /// One-triggers-other: this order's fill triggers the child.
    OTO {
        child: Box<CanonicalOrder>,
    },

    /// Full bracket: entry + TP + SL. All three linked.
    /// IBKR: parent + two children. Alpaca: bracket order_class.
    /// Binance: OTOCO. Deribit: otoco_config.
    Bracket {
        take_profit: LinkedLeg,
        stop_loss: LinkedLeg,
    },

    /// IBKR One-Cancels-All group (arbitrary number of linked orders).
    OCA {
        group_id: String,
        policy: OcaPolicy,
    },
}

pub struct LinkedLeg {
    pub pricing: OrderPricing,             // usually Limit
    pub trigger: Option<TriggerCondition>, // for stop-loss legs
    pub qty: Option<OrderQty>,             // if partial; None = match parent qty
}

pub enum TpSlScope {
    Full,            // entire position
    Partial,         // specific quantity
}

pub enum OcaPolicy {
    CancelBlock,     // cancel all on any fill
    ReduceWithBlock, // reduce others proportionally
    ReduceOverfill,  // allow overfill
}
```

### 4.8 Execution Hints

Hints guide the execution strategy selector. They are preferences, not guarantees.

```rust
pub struct ExecHint {
    pub urgency: Urgency,
    pub strategy: Option<ExecStrategyPreference>,
    pub max_slippage_bps: Option<Decimal>,
    /// Whether the engine may emulate unsupported order types locally.
    /// Default: false. Caller must explicitly opt-in.
    /// See §8.4 for emulation caveats (lost on crash, polling latency, etc.)
    pub allow_emulation: bool,
}

pub enum Urgency {
    Low,        // can wait, optimize for fees
    Normal,     // default
    High,       // prioritize speed over fees
    Immediate,  // market order, no delay
}

pub enum ExecStrategyPreference {
    /// Just send as-is (market or limit).
    Direct,
    /// Place limit, chase if not filled within patience window.
    LimitChase { patience: Duration, max_chase_bps: Decimal },
    /// Time-weighted: split over duration.
    /// Native on Hyperliquid, simulated elsewhere.
    TWAP { duration: Duration, randomize: bool },
    /// Volume-weighted: execute as % of volume.
    /// IBKR algo, simulated elsewhere.
    VWAP { target_pct: Decimal, max_duration: Duration },
    /// Depth-aware: split based on orderbook depth.
    DepthAware { max_impact_bps: Decimal },
    /// IBKR-specific algorithm.
    IBKRAlgo { algo_name: String, params: HashMap<String, String> },
    /// Scale order: multiple orders at price increments. IBKR.
    Scale {
        init_size: Decimal,
        subsequent_size: Decimal,
        price_increment: Decimal,
        num_levels: u32,
    },
}
```

### 4.9 Typed Venue Extensions

Instead of `HashMap<String, Value>`, use strongly-typed per-venue extension structs. This catches misconfiguration at compile time and makes adapter code self-documenting.

```rust
/// Venue-specific extensions. Each variant is only read by its own adapter.
/// The `None` case means no venue-specific params — the common path.
pub enum VenueParams {
    None,
    Bybit(BybitParams),
    Binance(BinanceParams),
    Hyperliquid(HyperliquidParams),
    Polymarket(PolymarketParams),
    Ibkr(IbkrParams),
    Deribit(DeribitParams),
    Alpaca(AlpacaParams),
}

pub struct BybitParams {
    pub position_idx: Option<u8>,          // 0=one-way, 1=buy, 2=sell (hedge mode)
    pub bbo_side_type: Option<BboSideType>,
}

pub struct BinanceParams {
    pub price_match: Option<String>,       // "OPPONENT_5", "QUEUE_5", etc.
}

pub struct HyperliquidParams {
    pub vault_address: Option<String>,
    pub builder: Option<HyperliquidBuilder>,
}

pub struct HyperliquidBuilder {
    pub address: String,
    pub fee_bps: u32,
}

pub struct PolymarketParams {
    pub neg_risk: bool,                    // multi-outcome CTF market
    pub tick_size: Decimal,                // tick size enforcement
}

pub struct IbkrParams {
    pub combo_legs: Option<Vec<IbkrComboLeg>>,
    pub delta_neutral: Option<serde_json::Value>,  // complex structure
    pub outside_rth: bool,                 // extended hours trading
}

pub struct DeribitParams {
    pub trigger_fill_condition: Option<String>, // "complete_fill", "incremental"
}

pub struct AlpacaParams {
    pub extended_hours: bool,
}
```

**Rule:** If a parameter is used by 3+ venues, promote it to the canonical model. If it's venue-specific, it stays in `VenueParams`. The adapter pattern-matches its own variant and ignores others.

### 4.10 Order Lifecycle State Machine

```rust
pub enum OrderState {
    /// Created but not yet submitted to risk gate.
    New,

    /// Passed risk gate, submitted to venue, awaiting acknowledgment.
    Submitted {
        venue_order_id: VenueOrderId,
        submitted_at: DateTime<Utc>,
    },

    /// Acknowledged by venue, resting on book (limit orders).
    Open {
        venue_order_id: VenueOrderId,
        opened_at: DateTime<Utc>,
    },

    /// Conditional/trigger order accepted, waiting for trigger condition.
    Triggered {
        venue_order_id: VenueOrderId,
        // Not yet active on book — waiting for trigger price
    },

    /// Partially filled — some quantity executed.
    PartiallyFilled {
        venue_order_id: VenueOrderId,
        filled_qty: Decimal,
        remaining_qty: Decimal,
        avg_fill_price: Decimal,
        total_fee: Fee,
    },

    /// Fully filled.
    Filled {
        venue_order_id: VenueOrderId,
        total_qty: Decimal,
        avg_fill_price: Decimal,
        total_fee: Fee,
        filled_at: DateTime<Utc>,
    },

    /// Cancelled (may have partial fills).
    Cancelled {
        reason: CancelReason,
        filled_qty: Decimal,            // may be > 0 if partially filled before cancel
        cancelled_at: DateTime<Utc>,
    },

    /// Rejected by venue or risk gate.
    Rejected {
        reason: String,
        rejected_by: RejectedBy,
        rejected_at: DateTime<Utc>,
    },

    /// Expired (GTD reached expiry, DAY order after close).
    Expired {
        filled_qty: Decimal,
        expired_at: DateTime<Utc>,
    },
}

pub enum CancelReason {
    UserRequested,
    AgentRequested { agent: AgentId },
    RiskGateKilled,
    CircuitBreakerTripped,
    LinkedOrderFilled,      // OCO counterpart filled
    Timeout,
    InsufficientBalance,
    VenueCancelled { venue_reason: String },
    SelfTradePreventionTriggered,
}

pub enum RejectedBy {
    CircuitBreaker,
    RiskGate { violations: Vec<RiskViolation> },
    Venue { error_code: String },
}

// Compile-time enforcement: only valid transitions exist as methods.
impl OrderState {
    pub fn on_submit(self, vid: VenueOrderId) -> Result<Self>;
    pub fn on_open(self) -> Result<Self>;
    pub fn on_fill(self, fill: &Fill) -> Result<Self>;
    pub fn on_cancel(self, reason: CancelReason) -> Result<Self>;
    pub fn on_reject(self, reason: String, by: RejectedBy) -> Result<Self>;
    pub fn on_expire(self) -> Result<Self>;
    // Invalid transitions: calling on_fill on a Cancelled order → Err
}
```

**Valid state transitions:**

```
New ──► Submitted ──► Open ──► PartiallyFilled ──► Filled
 │          │          │              │
 │          │          │              └──► Cancelled
 │          │          │              └──► Expired
 │          │          └──► Filled
 │          │          └──► Cancelled
 │          │          └──► Expired
 │          └──► Triggered ──► Open ──► ...
 │          └──► Rejected
 │          └──► Cancelled
 └──► Rejected (by risk gate, before submission)
```

---

## 5. Venue Adapter Contract

### 5.1 Core Trait

```rust
/// One adapter instance per (venue, account) pair.
/// Adapters are dumb pipes — translate canonical ↔ venue-native.
/// No risk logic, no business logic, no state management.
#[async_trait]
pub trait VenueAdapter: Send + Sync {
    fn venue_id(&self) -> &VenueId;
    fn account_id(&self) -> &AccountId;

    // ── Lifecycle ──

    async fn connect(&mut self) -> Result<()>;
    async fn disconnect(&mut self) -> Result<()>;
    fn connection_health(&self) -> ConnectionHealth;

    // ── Execution (core — all venues must implement) ──

    /// Submit order. Translates CanonicalOrder to venue-native format.
    /// Returns venue-assigned order ID on acknowledgment.
    /// Adapter checks for existing order via clientOrderId before submitting
    /// (idempotent: duplicate submit returns existing VenueOrderId).
    async fn submit_order(&self, order: &CanonicalOrder) -> Result<VenueOrderId>;

    /// Cancel order by venue ID.
    async fn cancel_order(&self, venue_order_id: &VenueOrderId) -> Result<()>;

    /// Cancel all open orders. Emergency use.
    async fn cancel_all(&self) -> Result<u32>;

    // ── State Queries (source of truth for reconciliation) ──

    async fn get_positions(&self) -> Result<Vec<VenuePosition>>;
    async fn get_open_orders(&self) -> Result<Vec<VenueOrder>>;
    async fn get_balance(&self) -> Result<VenueBalance>;
    async fn get_order_status(&self, venue_order_id: &VenueOrderId) -> Result<VenueOrderStatus>;

    // ── Market Data ──

    async fn get_orderbook(&self, symbol: &str, depth: usize) -> Result<OrderbookSnapshot>;

    /// Subscribe to real-time fills via WebSocket where supported,
    /// polling fallback otherwise. Adapter handles reconnection internally.
    fn subscribe_fills(&self) -> mpsc::UnboundedReceiver<Fill>;

    fn subscribe_market_data(
        &self,
        symbol: &str,
        events: &[MarketDataSubscription],
    ) -> mpsc::UnboundedReceiver<MarketEvent>;

    // ── Fee Info ──

    async fn get_fee_schedule(&self) -> Result<FeeSchedule>;

    // ── Capabilities ──

    fn capabilities(&self) -> &VenueCapabilities;

    // ── Capability-Gated Extensions ──
    // Instead of returning Err(Unsupported) from amend_order, adapters opt-in
    // to optional capabilities via these trait-object downcasts.
    // Gateway checks `as_amendable().is_some()` before attempting amend.

    fn as_amendable(&self) -> Option<&dyn AmendableVenue> { None }
    fn as_streaming(&self) -> Option<&dyn StreamingVenue> { None }

    // ── Rate Limiter ──

    fn rate_limiter(&self) -> &dyn RateLimiter;
}

/// Venue supports order amendment (Bybit, Binance, IBKR, Deribit).
#[async_trait]
pub trait AmendableVenue: Send + Sync {
    async fn amend_order(
        &self,
        venue_order_id: &VenueOrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()>;
}

/// Venue supports streaming market data (WebSocket-based venues).
#[async_trait]
pub trait StreamingVenue: Send + Sync {
    fn subscribe_orderbook(
        &self,
        symbol: &str,
        depth: usize,
    ) -> mpsc::UnboundedReceiver<OrderbookDelta>;
}

/// Per-adapter rate limiter. Each adapter owns one.
/// Gateway calls `acquire()` before dispatching to adapter.
pub trait RateLimiter: Send + Sync {
    /// Block until rate budget is available. `weight` = cost of this call
    /// (most calls = 1; batch cancel = N).
    async fn acquire(&self, weight: u32) -> Result<()>;
    /// Current utilization (0.0 = idle, 1.0 = at limit).
    fn utilization(&self) -> f64;
}
```

### 5.1.1 Connection Health & Reconnection

```rust
/// Replaces the simple `is_connected() -> bool` with richer health status.
/// Gateway uses this to route orders away from degraded adapters.
pub enum ConnectionHealth {
    Connected { latency_ms: u64 },
    Reconnecting { attempt: u32, since: DateTime<Utc> },
    Degraded { reason: String },
    Disconnected { reason: String },
}

/// Reconnection policy — configured per adapter.
pub struct ReconnectionPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,          // exponential backoff base
    pub max_delay: Duration,           // backoff cap
    pub jitter: bool,                  // randomize to avoid thundering herd
}
```
```

### 5.2 Venue Capabilities

The gateway uses capabilities to decide what's possible before attempting it.

```rust
pub struct VenueCapabilities {
    // Order types supported
    pub order_types: HashSet<SupportedOrderType>,
    pub time_in_force: HashSet<SupportedTIF>,

    // Features
    pub supports_amend: bool,
    pub supports_post_only: bool,
    pub supports_reduce_only: bool,
    pub supports_stop_orders: bool,
    pub supports_trailing_stop: bool,
    pub supports_bracket: bool,
    pub supports_oco: bool,
    pub supports_iceberg: bool,
    pub supports_twap: bool,

    // Real-time
    pub supports_websocket_fills: bool,
    pub supports_websocket_orderbook: bool,

    // Rate limits
    pub max_orders_per_second: u32,
    pub max_open_orders: u32,

    // Settlement
    pub settlement: SettlementType,

    // Auth
    pub auth_type: AuthType,

    // Precision
    pub qty_precision: HashMap<String, u32>,   // per-symbol
    pub price_precision: HashMap<String, u32>, // per-symbol
}

pub enum SupportedOrderType {
    Market, Limit, StopMarket, StopLimit,
    TakeProfit, TakeProfitLimit, TrailingStop,
    MarketToLimit, MarketIfTouched, LimitIfTouched,
    Pegged, ImpliedVol, Scale,
}

pub enum SupportedTIF { GTC, IOC, FOK, Day, GTD, AtOpen, AtClose, FAK, Auction }

pub enum SettlementType {
    Custodial,
    OnChain { chain: String },
}

pub enum AuthType {
    ApiKey,
    WalletSigning { chain: String },
}
```

### 5.3 Adapter Registry

```rust
/// Registry of all active venue adapters.
/// One adapter per (venue, account) — supports multi-account.
pub trait AdapterRegistry: Send + Sync {
    fn get(&self, venue: &VenueId, account: &AccountId) -> Option<&dyn VenueAdapter>;
    fn get_default(&self, venue: &VenueId) -> Option<&dyn VenueAdapter>;
    fn all_adapters(&self) -> Vec<(&VenueId, &AccountId, &dyn VenueAdapter)>;
    fn register(&mut self, adapter: Box<dyn VenueAdapter>);
}
```

---

## 6. Fee Engine

### 6.1 Fee Model Trait

```rust
pub trait FeeModel: Send + Sync {
    /// Pre-trade estimate (for sizing and strategy selection).
    /// Must be fast (<1μs) — called on every order evaluation.
    fn estimate(&self, order: &CanonicalOrder) -> Result<FeeEstimate>;

    /// Post-trade actual fee calculation from fill data.
    fn calculate(&self, fill: &Fill) -> Result<Fee>;

    /// Current fee schedule (for display/monitoring).
    fn schedule(&self) -> &FeeSchedule;

    /// Update schedule (after volume tier change, manual override).
    fn update_schedule(&mut self, schedule: FeeSchedule);
}
```

### 6.2 Fee Types

```rust
pub struct Fee {
    pub trading_fee: Decimal,        // maker or taker fee
    pub gas_fee: Option<Decimal>,    // on-chain tx cost (Hyperliquid, Polymarket)
    pub rebate: Option<Decimal>,     // maker rebate or reward tokens
    pub funding_fee: Option<Decimal>,// funding rate charge/credit (perps)
    pub net: Decimal,                // trading_fee + gas - rebate + funding
    pub currency: String,            // fee denomination
}

pub struct FeeEstimate {
    pub as_maker: Fee,               // if order rests on book
    pub as_taker: Fee,               // if order crosses spread
    pub worst_case: Decimal,         // taker + gas (what sizing must assume)
}

pub struct FeeSchedule {
    pub venue: VenueId,
    pub tier: String,                // "VIP1", "regular", "market_maker"
    pub maker_rate: Decimal,
    pub taker_rate: Decimal,
    pub volume_30d: Decimal,         // for tier calculation
    pub gas_model: Option<GasModel>,
    pub rebate_model: Option<RebateModel>,
    pub updated_at: DateTime<Utc>,
}

pub enum GasModel {
    Fixed { cost_usd: Decimal },
    Dynamic { estimator_name: String },  // resolved at runtime
}

pub enum RebateModel {
    MakerRebate { rate: Decimal },
    RewardToken { rate: Decimal, token: String },  // Polymarket rewards
    NativeTokenDiscount { discount_pct: Decimal, token: String }, // BNB, etc.
}
```

### 6.3 Fee-Aware Execution

The execution strategy selector uses fee estimates to choose between limit (capture maker rebate) and market (pay taker fee):

```rust
// Pseudocode — not a trait, but the decision logic
fn select_strategy(order: &CanonicalOrder, fees: &FeeEstimate) -> ExecStrategyPreference {
    let maker_taker_spread = fees.as_taker.net - fees.as_maker.net;

    match order.exec_hint.urgency {
        Urgency::Immediate => ExecStrategyPreference::Direct,  // always market
        Urgency::High => ExecStrategyPreference::LimitChase { ... },
        _ if maker_taker_spread > Decimal::from_str("0.0005")? => {
            // >5bps spread: worth trying limit first
            ExecStrategyPreference::LimitChase { ... }
        }
        _ => ExecStrategyPreference::Direct,
    }
}
```

---

## 7. Risk Architecture

### 7.1 Layer 0: Circuit Breakers (Compiled In)

Hardcoded in the execution engine binary. Not configurable at runtime. Changed only via code deploy + review. The absolute last line of defense.

```rust
pub trait CircuitBreaker: Send + Sync {
    /// Returns Err if order must be killed unconditionally.
    fn check(&self, order: &CanonicalOrder) -> Result<(), CircuitBreakerTrip>;

    /// System-wide kill switch. When tripped, ALL execution halts.
    fn is_halted(&self) -> bool;

    /// Trip the breaker manually (emergency halt).
    fn trip(&mut self, reason: String);

    /// Reset after investigation (requires explicit action).
    fn reset(&mut self) -> Result<()>;
}

pub struct CircuitBreakerTrip {
    pub breaker: String,          // which breaker tripped
    pub reason: String,
    pub tripped_at: DateTime<Utc>,
}
```

**Hardcoded breakers (examples, exact values set at compile time):**

| Breaker | Condition | Action |
|---------|-----------|--------|
| Max single order size | Notional > $X | Reject |
| Max position per symbol | Net exposure > Y contracts | Reject |
| Max drawdown | Daily P&L < -Z% NAV | Halt all trading |
| Max orders per minute | Rate > N/min | Reject |
| Dry-run bypass | `dry_run == false` without explicit flag | Reject |

### 7.2 Layer 1: Programmatic Risk Gate

Stateful risk evaluation. Knows current firm book. Configurable limits (hot-reloadable by Layer 2 or Layer 3). Fast (<1ms per evaluation).

```rust
pub trait RiskGate: Send + Sync {
    /// Evaluate order against all risk limits.
    /// Returns Ok(approved) or Err(violations).
    fn evaluate(&self, order: &CanonicalOrder) -> Result<RiskApproval, Vec<RiskViolation>>;

    /// Current firm book snapshot.
    fn firm_book(&self) -> &FirmBook;

    /// Hot-reload limits (called by Taleb or Feynman).
    fn update_limits(&mut self, limits: RiskLimits);

    /// Update internal state on fill.
    fn on_fill(&mut self, fill: &Fill);

    /// Update internal state on reconciliation.
    fn on_position_corrected(&mut self, market: &MarketId, new_qty: Decimal);

    /// Current limits (for display/monitoring).
    fn current_limits(&self) -> &RiskLimits;
}

pub struct RiskApproval {
    pub approved_at: DateTime<Utc>,
    pub warnings: Vec<RiskViolation>,  // non-blocking warnings (approaching limits)
    pub checks_performed: Vec<RiskCheckResult>,
}
```

### 7.3 Risk Limits

```rust
pub struct RiskLimits {
    pub firm: FirmRiskLimits,
    pub per_agent: HashMap<AgentId, AgentRiskLimits>,
    pub per_instrument: HashMap<InstrumentId, InstrumentRiskLimits>,
    pub per_venue: HashMap<VenueId, VenueRiskLimits>,
    pub prediction_market: PredictionMarketLimits,
}

pub struct FirmRiskLimits {
    pub max_gross_notional: Decimal,       // total absolute exposure
    pub max_net_notional: Decimal,         // net directional exposure
    pub max_drawdown_pct: Decimal,         // halt if breached
    pub max_daily_loss: Decimal,           // halt if breached
    pub max_open_orders: u32,
}

pub struct AgentRiskLimits {
    pub allocated_capital: Decimal,        // capital budget
    pub max_position_notional: Decimal,    // max single position
    pub max_gross_notional: Decimal,       // total across all positions
    pub max_drawdown_pct: Decimal,         // pause agent if breached
    pub max_daily_loss: Decimal,
    pub max_open_orders: u32,
    pub allowed_instruments: Option<HashSet<InstrumentId>>, // whitelist
    pub allowed_venues: Option<HashSet<VenueId>>,           // whitelist
    pub allowed_market_kinds: Option<HashSet<MarketKindTag>>, // spot, perp, option, prediction
}

pub struct InstrumentRiskLimits {
    pub max_net_qty: Decimal,              // max directional exposure
    pub max_gross_qty: Decimal,            // max absolute exposure across venues
    pub max_concentration_pct: Decimal,    // max % of NAV
}

pub struct VenueRiskLimits {
    pub max_notional: Decimal,             // counterparty risk limit
    pub max_positions: u32,
}

pub struct PredictionMarketLimits {
    pub max_total_notional: Decimal,       // all prediction positions
    pub max_per_market_notional: Decimal,  // single prediction market
    pub max_pct_of_nav: Decimal,           // e.g., 10% cap
    pub max_unresolved_markets: u32,       // diversification
}
```

### 7.4 Layer 2: Risk Agent Interface

Taleb (LLM risk agent) runs asynchronously on a slower loop. His output is **configuration changes to Layer 1**, not per-order decisions.

```rust
/// Interface that the risk agent (Taleb) calls via gRPC.
/// These are NOT in the order hot path.
pub trait RiskAgentInterface: Send + Sync {
    /// Tighten or loosen agent-level limits.
    async fn adjust_agent_limits(&self, agent: &AgentId, limits: AgentRiskLimits) -> Result<()>;

    /// Tighten or loosen instrument-level limits.
    async fn adjust_instrument_limits(&self, instrument: &InstrumentId, limits: InstrumentRiskLimits) -> Result<()>;

    /// Emergency: pause a specific agent (all orders rejected).
    async fn pause_agent(&self, agent: &AgentId, reason: String) -> Result<()>;

    /// Resume a paused agent.
    async fn resume_agent(&self, agent: &AgentId) -> Result<()>;

    /// Emergency: halt all trading.
    async fn halt_all(&self, reason: String) -> Result<()>;

    /// Get current risk snapshot (for Taleb's assessment).
    async fn risk_snapshot(&self) -> Result<RiskSnapshot>;
}

pub struct RiskSnapshot {
    pub firm_book: FirmBook,
    pub current_limits: RiskLimits,
    pub recent_violations: Vec<RiskViolation>,
    pub agent_statuses: HashMap<AgentId, AgentStatus>,
    pub as_of: DateTime<Utc>,
}

pub enum AgentStatus { Active, Paused { reason: String, since: DateTime<Utc> }, DrawdownBreached }
```

---

## 8. Execution Gateway

### 8.1 Gateway Trait

The gateway orchestrates the full pipeline: validation → risk check → emulation → strategy selection → adapter dispatch → lifecycle tracking.

**Idempotency invariant:** `submit()` is idempotent on `OrderId`. First call submits the order; subsequent calls with the same `OrderId` return the cached `OrderAck`. This prevents duplicate orders on retry/restart.

```rust
#[async_trait]
pub trait ExecutionGateway: Send + Sync {
    /// Submit order through the full pipeline:
    /// Validate → Circuit breaker → Risk gate → Emulate (if needed) → Strategy selector → Adapter
    /// Idempotent on order.id — duplicate submissions return cached ack.
    async fn submit(&self, order: CanonicalOrder) -> Result<OrderAck>;

    /// Cancel an open order.
    async fn cancel(&self, order_id: &OrderId) -> Result<()>;

    /// Amend an open order (if venue supports it).
    async fn amend(
        &self,
        order_id: &OrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()>;

    /// Emergency: cancel all orders across all venues.
    async fn cancel_all(&self) -> Result<u32>;

    /// Get order by internal ID.
    async fn get_order(&self, order_id: &OrderId) -> Result<Option<OrderRecord>>;

    /// Get all open orders.
    async fn open_orders(&self) -> Result<Vec<OrderRecord>>;

    /// Fill stream (for downstream consumers).
    fn fill_stream(&self) -> mpsc::UnboundedReceiver<Fill>;
}

pub struct OrderAck {
    pub order_id: OrderId,
    pub state: OrderState,
    pub risk_approval: Option<RiskApproval>,
    pub strategy_selected: String,
    pub accepted_at: DateTime<Utc>,
}

pub struct OrderRecord {
    pub order: CanonicalOrder,
    pub state: OrderState,
    pub fills: Vec<Fill>,
    pub events: Vec<Event>,
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}
```

### 8.2 Execution Strategy Trait

```rust
#[async_trait]
pub trait ExecStrategy: Send + Sync {
    fn name(&self) -> &str;

    /// Can this strategy handle the given order on this venue?
    fn accepts(&self, order: &CanonicalOrder, caps: &VenueCapabilities) -> bool;

    /// Execute the order. Returns final report with all fills.
    async fn execute(
        &self,
        order: &CanonicalOrder,
        adapter: &dyn VenueAdapter,
        fee_model: &dyn FeeModel,
        clock: &dyn Clock,
    ) -> Result<ExecReport>;
}

pub struct ExecReport {
    pub fills: Vec<Fill>,
    pub final_state: OrderState,
    pub strategy_used: String,
    pub execution_time: Duration,
    pub slippage_bps: Option<Decimal>,  // vs arrival price
}
```

### 8.3 Order Validation (Pre-Risk, Stateless)

Fast, stateless validation before the order enters the pipeline. Catches malformed orders immediately without touching the risk gate or venue.

```rust
/// Stateless order validator. Runs before risk gate.
/// Rejects orders that are structurally invalid regardless of current state.
pub trait OrderValidator: Send + Sync {
    fn validate(&self, order: &CanonicalOrder, caps: &VenueCapabilities) -> Result<(), Vec<ValidationError>>;
}

pub enum ValidationError {
    InvalidQty { reason: String },           // zero, negative, below min lot
    InvalidPrice { reason: String },         // negative, wrong tick size
    UnsupportedOrderType { requested: String, venue: VenueId },
    UnsupportedTIF { requested: TimeInForce, venue: VenueId },
    MissingRequiredField { field: String },
    IncompatibleConstraints { reason: String }, // e.g., post_only + market
    PrecisionExceeded { field: String, max_decimals: u32, actual: u32 },
}
```

### 8.4 Order Emulation Layer

> **Principle #12: Emulation is opt-in, not automatic.**
>
> When a venue doesn't support a requested order type, the engine does NOT silently
> emulate. The default behavior is to return `Err(UnsupportedOrderType)`. The caller
> must explicitly opt-in to emulation via `exec_hint.allow_emulation = true`.
>
> This makes the behavior traceable: the caller knows whether their trailing stop is
> native (managed by the exchange) or emulated (managed by the engine, with different
> failure modes — e.g., engine crash loses the trailing stop).

```rust
/// Emulation layer sits between risk gate and venue adapter.
///
/// IMPORTANT: Emulation is never automatic. Orders are only emulated when:
///   1. The venue doesn't support the order type natively, AND
///   2. The order's exec_hint.allow_emulation == true
///
/// If allow_emulation is false (the default), unsupported order types
/// return Err(UnsupportedOrderType) — the caller must decide what to do.
#[async_trait]
pub trait OrderEmulator: Send + Sync {
    /// Check if this order would require emulation on this venue.
    /// Does NOT perform emulation — just reports the situation.
    fn check_emulation(
        &self,
        order: &CanonicalOrder,
        caps: &VenueCapabilities,
    ) -> EmulationCheck;

    /// Start emulating. Only called if check_emulation returned NeedsEmulation
    /// AND the order's allow_emulation flag is true.
    fn emulate(
        &self,
        order: &CanonicalOrder,
        adapter: &dyn VenueAdapter,
        clock: &dyn Clock,
    ) -> Result<EmulationHandle>;
}

pub enum EmulationCheck {
    /// Venue supports this order type natively. No emulation needed.
    NativeSupport,
    /// Venue does not support this order type. Emulation is available.
    /// Includes what the emulation would do (for caller to make informed decision).
    NeedsEmulation {
        emulation_type: EmulationType,
        /// What the caller should know before opting in.
        caveats: Vec<EmulationCaveat>,
    },
    /// Venue does not support this order type and emulation is not possible.
    Unsupported { reason: String },
}

pub enum EmulationCaveat {
    /// Engine crash loses the emulated order (not persisted on exchange).
    LostOnCrash,
    /// Emulated order checks price on a polling interval, not tick-by-tick.
    PollingLatency { interval: Duration },
    /// Emulated bracket decomposes into independent orders (not atomically linked).
    NonAtomicLegs,
}

pub struct EmulationHandle {
    pub emulated_order_id: OrderId,
    pub emulation_type: EmulationType,
    /// Child orders managed by this emulation.
    pub child_order_ids: Vec<OrderId>,
}

pub enum EmulationType {
    TrailingStop,       // locally tracked, repriced on tick
    Bracket,            // decomposed into independent TP/SL
    OCO,                // locally linked pair
    MarketToLimit,      // market order → remainder rests as limit
    TWAP,               // time-sliced into smaller orders
}
```

### 8.5 Portfolio / Signal-to-Order Generator

Converts agent signals into concrete `CanonicalOrder`s. This is the bridge between the agent world (signals with conviction) and the execution world (orders with price/qty).

```rust
/// Translates signals into orders using current portfolio state.
/// Handles sizing, venue selection, and order construction.
pub trait Portfolio: Send + Sync {
    /// Convert a signal into zero or more orders.
    /// May produce zero orders if the signal duplicates an existing position,
    /// or multiple orders if splitting across venues.
    fn on_signal(&mut self, signal: &Signal) -> Result<Vec<CanonicalOrder>>;

    /// Current firm book (read-only view).
    fn state(&self) -> &FirmBook;
}

pub struct Signal {
    pub id: SignalId,
    pub agent: AgentId,
    pub instrument: InstrumentId,
    pub direction: Side,
    pub conviction: Decimal,         // 0.0 – 1.0
    pub sizing_hint: Option<Decimal>, // optional notional target
    pub arb_type: String,            // e.g., "funding_rate", "basis", "directional"
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}
```

### 8.6 Pipeline Stages (Composable)

The gateway pipeline is a sequence of composable stages. Each stage can pass, reject, or transform the order. This makes the pipeline extensible without modifying gateway core logic.

```rust
/// A single stage in the order processing pipeline.
/// Stages are composed in order: Validate → Risk → Emulate → Route → Submit.
#[async_trait]
pub trait PipelineStage: Send + Sync {
    fn name(&self) -> &str;

    /// Process order through this stage.
    /// Ok(order) = pass to next stage (may be modified).
    /// Err = reject with reason.
    async fn process(&self, order: CanonicalOrder) -> Result<CanonicalOrder>;
}

/// The full pipeline is a `Vec<Box<dyn PipelineStage>>`.
/// Gateway iterates stages in order; first rejection stops the chain.
/// Default stages: [Validator, CircuitBreaker, RiskGate, Emulator, Router]
/// Additional stages (e.g., fee optimizer, order dedup) can be inserted.
```

### 8.7 Graceful Shutdown

Trading systems can't just `process::exit`. Open orders, in-flight submissions, and pending acknowledgments must be handled cleanly.

```rust
pub struct ShutdownPolicy {
    /// Maximum time to wait for in-flight orders to receive venue acknowledgment.
    pub drain_timeout: Duration,
    /// Cancel open orders on shutdown?
    pub cancel_on_exit: CancelPolicy,
    /// Persist state snapshot before exit (for faster restart recovery)?
    pub snapshot_on_exit: bool,
}

pub enum CancelPolicy {
    /// Leave orders resting on venues (default for planned restarts).
    LeaveOpen,
    /// Cancel all open orders (for emergency shutdown).
    CancelAll,
    /// Cancel only orders placed by specific agents.
    CancelByAgent(Vec<AgentId>),
}
```

---

## 9. Journal & State Recovery

> **Design philosophy:** WAL + materialized state, not full event sourcing.
>
> The materialized state store is the **primary** read path. The journal is an
> append-only write-ahead log used for three things:
>
> 1. **Crash recovery** — restore last snapshot, replay tail events
> 2. **Audit trail** — what happened, when, and why (forensics after a loss)
> 3. **Regression replay** — replay production sequences against new code to catch regressions
>
> We do **not** derive state by replaying all events from seq 0. That's fragile
> across schema migrations and unnecessary for our use case. Snapshots are the
> fast path; tail replay covers the gap between last snapshot and crash point.
>
> **What replay is NOT for:** Backtest-live parity at the order level. Replaying
> events through new risk gate logic is valid (deterministic internal decisions).
> Replaying fills to get the same P&L is not (fills depend on orderbook state,
> latency, other market participants). Design the journal for decision verification,
> not outcome reproduction.

### 9.1 Event Types

```rust
pub struct Event {
    pub seq: u64,                   // monotonic sequence number
    pub timestamp: DateTime<Utc>,
    pub kind: EventKind,
}

pub enum EventKind {
    // ── Order Lifecycle ──
    OrderCreated { order: CanonicalOrder },
    OrderSubmitted { order_id: OrderId, venue_order_id: VenueOrderId },
    OrderOpened { order_id: OrderId },
    OrderFilled { fill: Fill },
    OrderCancelled { order_id: OrderId, reason: CancelReason },
    OrderRejected { order_id: OrderId, reason: String, by: RejectedBy },
    OrderExpired { order_id: OrderId, filled_qty: Decimal },
    OrderAmended { order_id: OrderId, new_price: Option<Decimal>, new_qty: Option<Decimal> },

    // ── Risk ──
    RiskApproved { order_id: OrderId, approval: RiskApproval },
    RiskRejected { order_id: OrderId, violations: Vec<RiskViolation> },
    CircuitBreakerTripped { breaker: String, reason: String },
    CircuitBreakerReset { breaker: String },
    LimitsAdjusted { agent: Option<AgentId>, new_limits: serde_json::Value },
    AgentPaused { agent: AgentId, reason: String },
    AgentResumed { agent: AgentId },

    // ── Reconciliation ──
    PositionDivergence {
        market: MarketId,
        internal_qty: Decimal,
        venue_qty: Decimal,
    },
    PositionCorrected { market: MarketId, corrected_to: Decimal },

    // ── Funding (Perpetuals) ──
    FundingPayment {
        market: MarketId,
        rate: Decimal,          // funding rate (positive = longs pay shorts)
        payment: Decimal,       // actual payment (negative = paid, positive = received)
        position_qty: Decimal,  // position size at time of funding
    },

    // ── Prediction Market ──
    MarketResolved { market: MarketId, outcome: PredictionOutcome },
    PredictionPositionSettled { market: MarketId, pnl: Decimal },

    // ── System ──
    VenueConnected { venue: VenueId, account: AccountId },
    VenueDisconnected { venue: VenueId, reason: String },
    EngineStarted { version: String },
    EngineShutdown { reason: String },
}
```

### 9.2 Journal Trait (Write-Ahead Log)

```rust
#[async_trait]
pub trait Journal: Send + Sync {
    /// Append event. Returns assigned sequence number.
    /// Events are durable after this returns — fsync'd or equivalent.
    async fn append(&self, event: EventKind) -> Result<u64>;

    /// Append batch (atomic — all or none are written).
    async fn append_batch(&self, events: Vec<EventKind>) -> Result<u64>;

    /// Replay events from a sequence number (for crash recovery).
    /// Typical use: replay_from(last_snapshot_seq + 1)
    async fn replay_from(&self, from_seq: u64) -> Result<Vec<Event>>;

    /// Replay events for a specific order (for audit/forensics).
    async fn replay_order(&self, order_id: &OrderId) -> Result<Vec<Event>>;

    /// Replay events in time range (for regression testing).
    async fn replay_range(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Event>>;

    /// Latest sequence number.
    async fn latest_seq(&self) -> Result<u64>;

    /// Subscribe to new events (for real-time consumers: dashboard, bus bridge).
    fn subscribe(&self) -> mpsc::UnboundedReceiver<Event>;
}
```

### 9.2.1 Crash Recovery Protocol

```
Engine starts
  │
  ├─ 1. Load latest snapshot from StateStore
  │     → returns Option<(seq, state)>
  │
  ├─ 2. If snapshot exists at seq N:
  │       replay journal from seq N+1 → latest
  │       apply each event to state via StateStore::apply_event()
  │
  │     If no snapshot:
  │       query all venues for positions, balances, open orders
  │       build state from venue truth (cold start)
  │       write snapshot at current seq
  │
  ├─ 3. Run immediate reconciliation against all venues
  │       any divergence → PositionDivergence event → alert
  │
  └─ 4. Resume normal operation
```

**Snapshot frequency:** Every N minutes or every M events, whichever comes first.
Snapshots are cheap (serialize materialized state to disk). Journal tail replay
is bounded by snapshot interval — worst case replays M events, not the full history.

**Journal retention:** Keep 30 days for audit trail and regression replay.
Older events are archived to cold storage (S3/GCS), not deleted. Snapshots older
than 7 days are pruned (only latest + daily are retained).
```

### 9.3 State Store (Materialized View)

```rust
#[async_trait]
pub trait StateStore: Send + Sync {
    // ── Positions ──
    async fn get_strategy_positions(&self) -> Result<Vec<StrategyPosition>>;
    async fn get_strategy_positions_by_agent(&self, agent: &AgentId) -> Result<Vec<StrategyPosition>>;
    async fn get_instrument_exposures(&self) -> Result<Vec<InstrumentExposure>>;
    async fn get_firm_book(&self) -> Result<FirmBook>;

    // ── Orders ──
    async fn get_open_orders(&self) -> Result<Vec<OrderRecord>>;
    async fn get_order(&self, order_id: &OrderId) -> Result<Option<OrderRecord>>;
    async fn get_orders_by_agent(&self, agent: &AgentId) -> Result<Vec<OrderRecord>>;

    // ── Fills ──
    async fn get_fills(&self, filter: &FillFilter) -> Result<Vec<Fill>>;
    async fn get_fills_by_order(&self, order_id: &OrderId) -> Result<Vec<Fill>>;

    // ── P&L ──
    async fn get_pnl_by_agent(&self, agent: &AgentId) -> Result<PnLSummary>;
    async fn get_pnl_by_instrument(&self, instrument: &InstrumentId) -> Result<PnLSummary>;
    async fn get_firm_pnl(&self) -> Result<PnLSummary>;

    // ── Event Projection ──
    async fn apply_event(&mut self, event: &Event) -> Result<()>;
    async fn rebuild_from(&mut self, events: &[Event]) -> Result<()>;

    // ── Snapshots ──
    // Save/restore materialized state to avoid replaying entire event log on restart.
    // Snapshot at seq N means: replay only events > N to recover current state.
    async fn save_snapshot(&self, at_seq: u64) -> Result<()>;
    async fn restore(&mut self) -> Result<Option<u64>>;  // Returns seq of latest snapshot, or None
}

pub struct FillFilter {
    pub agent: Option<AgentId>,
    pub venue: Option<VenueId>,
    pub instrument: Option<InstrumentId>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
}

pub struct PnLSummary {
    pub realized: Decimal,
    pub unrealized: Decimal,
    pub total_fees: Decimal,
    pub net: Decimal,                   // realized + unrealized - fees
    pub as_of: DateTime<Utc>,
}
```

---

## 10. Execution Context

The trait that strategy code programs against. Identical interface for backtest, paper, and live.

### 10.1 Core Traits (Split)

The monolithic `ExecutionContext` is split into focused sub-traits. This follows the Interface Segregation Principle — strategies that only need market data don't depend on order submission, and test doubles are simpler.

```rust
/// Market data provider. Used by strategies that need prices.
pub trait MarketDataProvider: Send + Sync {
    fn subscribe_market_data(
        &self,
        market: &MarketId,
        events: &[MarketDataSubscription],
    ) -> mpsc::UnboundedReceiver<MarketEvent>;
}

/// Order submitter. Used by strategies that need to place orders.
#[async_trait]
pub trait OrderSubmitter: Send + Sync {
    async fn submit(&self, order: CanonicalOrder) -> Result<OrderAck>;
    async fn cancel(&self, order_id: &OrderId) -> Result<()>;
    async fn amend(
        &self,
        order_id: &OrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()>;
    fn fill_stream(&self) -> mpsc::UnboundedReceiver<Fill>;
}

/// Portfolio reader. Used by strategies that need position/balance info.
#[async_trait]
pub trait PortfolioReader: Send + Sync {
    async fn positions(&self) -> Result<Vec<StrategyPosition>>;
    async fn positions_by_agent(&self, agent: &AgentId) -> Result<Vec<StrategyPosition>>;
    async fn firm_book(&self) -> Result<FirmBook>;
    async fn open_orders(&self) -> Result<Vec<OrderRecord>>;
    async fn balance(&self, venue: &VenueId) -> Result<VenueBalance>;
    fn estimate_fees(&self, order: &CanonicalOrder) -> Result<FeeEstimate>;
}

/// Unified strategy context — composes all sub-traits.
/// This is what strategy code programs against. Identical for backtest, paper, live.
#[async_trait]
pub trait StrategyContext: MarketDataProvider + OrderSubmitter + PortfolioReader + Send + Sync {
    fn mode(&self) -> ExecutionMode;
    fn clock(&self) -> &dyn Clock;
}

pub enum ExecutionMode { Backtest, Paper, Live }

pub enum MarketDataSubscription {
    Trades,
    BookUpdates { depth: usize },
    Klines { interval: Duration },
    FundingRate,
    Liquidations,
}
```

### 10.2 Implementation Modes

All modes share `EngineCore` (§15.2) for decision logic. The mode is a deployment
config (`FEYNMAN_MODE` env var), not a code change. Strategies talk to the engine
via gRPC in all modes — they don't know which mode the engine is running in.

| Mode | Market Data | Fills | State | Clock | Fees |
|------|-------------|-------|-------|-------|------|
| **Live** | WebSocket/polling from venues | Real fills from venues | Persisted (SQLite/Postgres) | Wall clock | Real fee schedules |
| **Paper** | Live feeds from venues | Simulated against live orderbook | Persisted | Wall clock | Real fee schedules |
| **Backtest** | N/A (engine doesn't distribute data) | `SimulatedVenue` adapter | In-memory | `SimulatedClock` (advanced via `AdvanceClock` RPC) | Same fee schedules as live |

**The engine is always a service.** No strategy imports `EngineCore` directly. The gRPC
boundary is the only interface — this guarantees risk enforcement is never bypassed.
See [STRATEGY_ENGINE_BOUNDARY.md](./STRATEGY_ENGINE_BOUNDARY.md) for the full contract.

**In backtest mode**, the strategy layer (not the engine) owns data replay, clock
advancement, and backtest orchestration. The engine receives orders, runs risk checks,
and returns fills from its `SimulatedVenue` — same as live, different adapters.

See §15.7 for the full backtest architecture.

### 10.3 Fill Simulation (Paper + Backtest)

The engine's internal fill simulator, used when real venue fills are not available.
For high-fidelity fill simulation with queue-position modeling and latency effects,
the strategy layer should use [HFTBacktest](https://github.com/nkaz001/hftbacktest)
as its backtest harness — but that runs outside the engine.
See [STRATEGY_ENGINE_BOUNDARY.md](./STRATEGY_ENGINE_BOUNDARY.md) §3.4.

```rust
pub trait FillSimulator: Send + Sync {
    /// Simulate fill against orderbook state.
    fn simulate(
        &self,
        order: &CanonicalOrder,
        book: &OrderbookSnapshot,
        fee_model: &dyn FeeModel,
    ) -> Result<SimulatedFill>;
}

pub struct SimulatedFill {
    pub fill: Fill,
    pub slippage_bps: Decimal,
    pub market_impact_bps: Decimal,
}

/// Historical data replay source (Internal Backtest only).
pub trait DataReplay: Send + Sync {
    fn subscribe(&self, market: &MarketId) -> mpsc::UnboundedReceiver<MarketEvent>;
    fn advance_to(&mut self, time: DateTime<Utc>);
    fn data_level(&self) -> DataLevel;
    fn time_range(&self) -> (DateTime<Utc>, DateTime<Utc>);
}

pub enum DataLevel {
    OHLCV,      // candle bars — basic slippage assumption
    L1BBO,      // best bid/ask — spread-aware simulation
    L2Depth,    // full orderbook — realistic fill simulation
    L3Trades,   // individual trades — queue position modeling
}
```

**When to use which fill model:**

| Need | Fill Model |
|------|-----------|
| Test risk gate / pipeline logic | Engine's `SimulatedVenue` — fill realism doesn't matter |
| Validate against production journals | Engine's `SimulatedVenue` — replay exact events |
| Estimate strategy alpha / P&L | HFTBacktest (strategy layer) — realistic fills critical |
| Study queue position / latency effects | HFTBacktest (strategy layer) — only tool that models this |
```

---

## 11. Message Bus

### 11.1 Bus Trait

```rust
#[async_trait]
pub trait MessageBus: Send + Sync {
    /// Publish payload to a topic. Returns message ID.
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId>;

    /// Subscribe to topic with consumer group semantics.
    /// Each message delivered to exactly one consumer in the group.
    async fn subscribe(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
    ) -> Result<mpsc::UnboundedReceiver<BusMessage>>;

    /// Acknowledge message processing.
    async fn ack(&self, topic: &str, group: &str, msg_id: &MessageId) -> Result<()>;

    /// Get pending (unacknowledged) messages older than min_idle.
    async fn pending(
        &self,
        topic: &str,
        group: &str,
        min_idle: Duration,
    ) -> Result<Vec<BusMessage>>;

    /// Claim stuck messages for reprocessing.
    async fn claim(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
        msg_ids: &[MessageId],
    ) -> Result<Vec<BusMessage>>;

    /// Topic metadata.
    async fn topic_info(&self, topic: &str) -> Result<TopicInfo>;
}

pub struct BusMessage {
    pub id: MessageId,
    pub topic: String,
    pub payload: Vec<u8>,
    pub published_at: DateTime<Utc>,
}

pub struct TopicInfo {
    pub length: u64,
    pub consumer_groups: Vec<ConsumerGroupInfo>,
    pub oldest_message: Option<DateTime<Utc>>,
    pub newest_message: Option<DateTime<Utc>>,
}

pub struct ConsumerGroupInfo {
    pub name: String,
    pub consumers: u32,
    pub pending: u64,
    pub last_delivered: Option<MessageId>,
}
```

### 11.2 Topics

| Topic | Publisher | Consumer(s) | Payload |
|-------|-----------|-------------|---------|
| `signals` | Agents (Satoshi, Graham, ...) | Risk Gate | Signal (conviction, direction, sizing) |
| `approved_orders` | Risk Gate | Execution Gateway | CanonicalOrder (risk-approved) |
| `fills` | Execution Gateway | State Store, Agents, Dashboard | Fill |
| `positions` | Reconciler | State Store, Risk Gate, Dashboard | Position update |
| `risk_events` | Risk Gate, Circuit Breaker | Dashboard, Agents | Risk violations, halts |
| `funding` | Adapters (perps) | State Store, Risk Gate, Dashboard | FundingPayment events |
| `system` | All components | Dashboard, Alerting | Heartbeats, connections, errors |

---

## 12. Position Model

### 12.1 Hierarchy

```
Trade (atomic fill)
  └── Strategy Position (agent + venue + market)
       └── Instrument Exposure (unified across venues for same underlying)
            └── Firm Book (aggregate)
```

### 12.2 Types

```rust
/// Lowest level: per-agent, per-venue, per-market.
pub struct StrategyPosition {
    pub agent: AgentId,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub qty: Decimal,
    pub avg_entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub total_fees_paid: Decimal,
    /// Accumulated funding payments (perps). Positive = received, negative = paid.
    pub accumulated_funding: Decimal,
    /// Fill provenance — traces position back to exact fills.
    pub fill_ids: Vec<(OrderId, u64)>,  // (order_id, fill_seq)
    pub opened_at: DateTime<Utc>,
    pub last_fill_at: DateTime<Utc>,
    pub metadata: PositionMetadata,
}

pub struct PositionMetadata {
    pub strategy: Option<String>,
    pub signal_ids: Vec<SignalId>,
    /// Prediction market only
    pub resolution_status: Option<ResolutionStatus>,
}

pub enum ResolutionStatus {
    Open,
    Resolved { outcome: PredictionOutcome, resolved_at: DateTime<Utc> },
    Disputed,
    Voided,
}

/// Mid level: unified across venues for the same underlying.
pub struct InstrumentExposure {
    pub instrument: InstrumentId,
    pub net_qty: Decimal,               // positive = net long
    pub gross_qty: Decimal,             // absolute total
    pub net_notional_usd: Decimal,
    pub gross_notional_usd: Decimal,
    pub by_venue: Vec<VenueExposure>,
    pub by_agent: Vec<AgentExposure>,
}

pub struct VenueExposure {
    pub venue: VenueId,
    pub account: AccountId,
    pub net_qty: Decimal,
    pub notional_usd: Decimal,
}

pub struct AgentExposure {
    pub agent: AgentId,
    pub net_qty: Decimal,
    pub notional_usd: Decimal,
    pub pnl: Decimal,
}

/// Top level: entire firm.
pub struct FirmBook {
    pub total_nav: Decimal,
    pub free_capital: Decimal,
    pub total_unrealized_pnl: Decimal,
    pub total_realized_pnl: Decimal,
    pub total_fees_paid: Decimal,
    pub instruments: Vec<InstrumentExposure>,
    pub agent_allocations: Vec<AgentAllocation>,
    pub prediction_exposure: PredictionExposureSummary,
    pub as_of: DateTime<Utc>,
}

pub struct AgentAllocation {
    pub agent: AgentId,
    pub allocated_capital: Decimal,
    pub used_capital: Decimal,
    pub free_capital: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub current_drawdown: Decimal,
    pub max_drawdown_limit: Decimal,
    pub status: AgentStatus,
}

pub struct PredictionExposureSummary {
    pub total_notional: Decimal,
    pub pct_of_nav: Decimal,
    pub unresolved_markets: u32,
    pub positions: Vec<StrategyPosition>,  // prediction positions only
}
```

### 12.3 Reconciler

```rust
#[async_trait]
pub trait Reconciler: Send + Sync {
    /// Full reconciliation across all venues.
    async fn reconcile_all(&self) -> Result<Vec<ReconciliationResult>>;

    /// Single venue reconciliation.
    async fn reconcile_venue(
        &self,
        venue: &VenueId,
        account: &AccountId,
    ) -> Result<Vec<ReconciliationResult>>;

    /// Check for resolved prediction markets and settle positions.
    async fn check_resolutions(&self) -> Result<Vec<ResolutionResult>>;
}

pub struct ReconciliationResult {
    pub market: MarketId,
    pub internal_qty: Decimal,
    pub venue_qty: Decimal,
    pub divergence: Decimal,            // venue_qty - internal_qty
    pub action: ReconciliationAction,
}

pub enum ReconciliationAction {
    NoAction,                            // quantities match
    CorrectedInternal,                   // adjusted to match venue
    AlertedHuman { reason: String },     // needs manual decision
}

pub struct ResolutionResult {
    pub market: MarketId,
    pub outcome: PredictionOutcome,
    pub settled_pnl: Decimal,
}
```

---

## 13. Observability & Dashboard

### 13.1 Observability Architecture

```
┌────────────────────────────────────────────────────────────┐
│                    RUST CORE ENGINE                         │
│                                                            │
│  ┌────────────────┐  ┌────────────────┐  ┌──────────────┐ │
│  │ Metrics Export │  │  Trace Export  │  │ Event Stream │ │
│  │ (Prometheus)   │  │  (OTLP/Jaeger)│  │ (WebSocket)  │ │
│  └───────┬────────┘  └───────┬────────┘  └──────┬───────┘ │
└──────────┼───────────────────┼──────────────────┼──────────┘
           │                   │                  │
           ▼                   ▼                  ▼
    ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
    │  Prometheus  │   │    Jaeger    │   │  Dashboard   │
    │  (scrape)    │   │   (traces)   │   │  (Web UI)    │
    └──────┬───────┘   └──────────────┘   └──────────────┘
           │
           ▼
    ┌──────────────┐
    │   Grafana    │
    │  (optional)  │
    └──────────────┘
```

### 13.2 Metrics (Prometheus)

```rust
pub trait MetricsExporter: Send + Sync {
    fn record_order_submitted(&self, venue: &str, agent: &str);
    fn record_order_filled(&self, venue: &str, agent: &str, slippage_bps: f64);
    fn record_order_rejected(&self, venue: &str, agent: &str, reason: &str);
    fn record_fill_latency(&self, venue: &str, latency_ms: f64);
    fn record_risk_check_latency(&self, latency_us: f64);
    fn set_position(&self, venue: &str, instrument: &str, agent: &str, qty: f64);
    fn set_pnl(&self, agent: &str, pnl_type: &str, value: f64);
    fn set_nav(&self, value: f64);
    fn record_reconciliation_divergence(&self, venue: &str, instrument: &str);
    fn set_venue_connection(&self, venue: &str, connected: bool);
    fn record_bus_message(&self, topic: &str, direction: &str);
    fn set_bus_pending(&self, topic: &str, group: &str, count: u64);
}
```

**Key metrics exposed:**

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `engine_orders_total` | Counter | venue, agent, status | Orders by outcome |
| `engine_fills_total` | Counter | venue, agent | Fill count |
| `engine_fill_latency_ms` | Histogram | venue | Submission-to-fill latency |
| `engine_risk_check_us` | Histogram | | Risk gate evaluation time |
| `engine_slippage_bps` | Histogram | venue, strategy | Execution slippage |
| `engine_position_qty` | Gauge | venue, instrument, agent | Current position size |
| `engine_pnl_usd` | Gauge | agent, type (realized/unrealized) | P&L |
| `engine_nav_usd` | Gauge | | Firm NAV |
| `engine_drawdown_pct` | Gauge | agent | Current drawdown |
| `engine_venue_connected` | Gauge | venue | Connection status (0/1) |
| `engine_bus_pending` | Gauge | topic, group | Unprocessed messages |
| `engine_reconciliation_divergences_total` | Counter | venue, instrument | Divergence count |
| `engine_circuit_breaker_trips_total` | Counter | breaker | Circuit breaker activations |

### 13.3 Tracing (OpenTelemetry)

Every order gets a trace span from signal to fill:

```
[Signal Received] ──► [Risk Check] ──► [Strategy Selected] ──► [Venue Submitted] ──► [Fill Received]
     span                span               span                    span                  span
     │                                                                                      │
     └──────────────────── trace_id (= order_id) ──────────────────────────────────────────┘
```

### 13.4 Dashboard (Minimal Web UI)

Single-page application served by the Rust engine itself (embedded static files). No external frontend framework dependency — use HTMX + SSE for real-time updates, or a lightweight Rust web framework (axum) serving JSON for a minimal React/Svelte frontend.

**Dashboard Panels:**

```
┌─────────────────────────────────────────────────────────────────────┐
│  FEYNMAN CAPITAL — ENGINE DASHBOARD                    🟢 ALL SYSTEMS│
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─── FIRM OVERVIEW ────────────────────────────────────────────┐   │
│  │  NAV: $125,430    Daily P&L: +$1,230 (+0.98%)               │   │
│  │  Gross Exposure: $89,200    Net Exposure: $34,500 (long)     │   │
│  │  Open Orders: 3    Prediction Exposure: 4.2% NAV             │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌─── AGENT ALLOCATIONS ────────────────────────────────────────┐   │
│  │  Agent     Capital    Used     P&L      Drawdown   Status    │   │
│  │  Satoshi   $50,000    $32,100  +$890    -1.2%      🟢 Active │   │
│  │  Graham    $40,000    $28,400  +$340    -0.5%      🟢 Active │   │
│  │  Soros     $30,000    $18,700  +$0      0.0%       🟡 Paused │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌─── POSITIONS ────────────────────────────────────────────────┐   │
│  │  Instrument  Net Qty   Notional    Venues          Agents    │   │
│  │  BTC         +1.5      $94,500     Bybit, HyperL   Sat,Gra  │   │
│  │  ETH         -5.0      -$16,500    Bybit           Satoshi   │   │
│  │  "Will X?"   200 YES   $340        Polymarket      Graham    │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌─── VENUE STATUS ────────────────────────────────────────────┐    │
│  │  Venue          Status    Latency    Open Orders   Balance   │   │
│  │  Bybit          🟢 OK     23ms       2             $45,200   │   │
│  │  Hyperliquid    🟢 OK     45ms       1             $22,100   │   │
│  │  Polymarket     🟢 OK     120ms      0             $3,400    │   │
│  │  Binance        ⚫ Off    —          —             —         │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌─── RISK LIMITS ─────────────────────────────────────────────┐    │
│  │  Limit                     Value      Used     Utilization  │   │
│  │  Firm Gross Notional       $200,000   $89,200  ████░░ 45%   │   │
│  │  Firm Max Drawdown         -5.0%      -1.2%    ██░░░░ 24%   │   │
│  │  BTC Max Concentration     30% NAV    25.1%    █████░ 84%   │   │
│  │  Prediction Max            10% NAV    4.2%     ████░░ 42%   │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌─── RECENT EVENTS (live stream) ─────────────────────────────┐    │
│  │  14:23:01  FILL    Satoshi  BTC/USDT   +0.1 @ 63,012  Bybit │   │
│  │  14:22:45  SUBMIT  Satoshi  BTC/USDT   Limit Buy 0.1       │   │
│  │  14:22:44  RISK_OK Satoshi  BTC/USDT   All checks passed    │   │
│  │  14:20:00  RECON   All venues reconciled — no divergence     │   │
│  │  14:15:02  SIGNAL  Graham   "Will X?"  Buy YES conviction=72│   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌─── ALERTS ──────────────────────────────────────────────────┐    │
│  │  ⚠ BTC concentration at 84% of limit (approaching cap)      │   │
│  │  ⚠ Soros paused: drawdown breached -3.2% (limit: -3.0%)     │   │
│  └──────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

### 13.5 Dashboard Data Contract

The dashboard consumes data through two channels:

```rust
/// REST API for initial state load and queries.
/// Served by the engine on a configurable port.
pub trait DashboardApi {
    async fn firm_overview(&self) -> FirmBook;
    async fn agent_allocations(&self) -> Vec<AgentAllocation>;
    async fn positions(&self) -> Vec<InstrumentExposure>;
    async fn venue_status(&self) -> Vec<VenueStatus>;
    async fn risk_utilization(&self) -> RiskUtilization;
    async fn recent_events(&self, limit: u32) -> Vec<Event>;
    async fn order_history(&self, filter: &OrderHistoryFilter) -> Vec<OrderRecord>;
    async fn pnl_timeseries(&self, agent: Option<&AgentId>, period: &str) -> Vec<PnLPoint>;
}

/// SSE/WebSocket stream for real-time updates.
/// Dashboard subscribes once, receives updates as they happen.
pub enum DashboardUpdate {
    FirmBookChanged(FirmBook),
    PositionChanged(InstrumentExposure),
    OrderEvent(Event),
    VenueStatusChanged(VenueStatus),
    RiskAlert(RiskViolation),
    AgentStatusChanged { agent: AgentId, status: AgentStatus },
}

pub struct VenueStatus {
    pub venue: VenueId,
    pub connected: bool,
    pub latency_ms: Option<f64>,
    pub open_orders: u32,
    pub balance_usd: Option<Decimal>,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

pub struct RiskUtilization {
    pub limits: Vec<LimitUtilization>,
}

pub struct LimitUtilization {
    pub name: String,
    pub limit_value: Decimal,
    pub current_value: Decimal,
    pub utilization_pct: Decimal,
    pub severity: RiskSeverity,         // Warning if >80%, HardBlock if >=100%
}

pub struct PnLPoint {
    pub timestamp: DateTime<Utc>,
    pub realized: Decimal,
    pub unrealized: Decimal,
    pub net: Decimal,
    pub nav: Decimal,
}
```

### 13.6 Alerting

```rust
pub trait AlertSink: Send + Sync {
    async fn send(&self, alert: Alert) -> Result<()>;
}

pub struct Alert {
    pub severity: AlertSeverity,
    pub title: String,
    pub body: String,
    pub source: String,                 // component that generated it
    pub timestamp: DateTime<Utc>,
}

pub enum AlertSeverity {
    Info,       // logged only
    Warning,    // dashboard + optional notification
    Critical,   // dashboard + push notification (Telegram, Slack, etc.)
    Emergency,  // all channels + halt trading
}
```

Alert sinks (implementations): log file, dashboard panel, Telegram bot, Slack webhook, email. Start with log + dashboard, add notification channels as needed.

---

## 14. Verification Strategy

> **Philosophy:** Every component must be verifiable before it touches real money.
> The verification strategy is not a nice-to-have — it is a prerequisite for each
> phase gate. No component advances to the next phase without passing its
> verification tier. Slow and correct beats fast and buggy.

### 14.1 The Verification Pyramid

```
              ╱╲
             ╱  ╲
            ╱ $$ ╲        Shadow + Small Live
           ╱ Live  ╲      Catches: unknowns
          ╱──────────╲
         ╱  Paper +   ╲    Paper + Testnet
        ╱   Testnet    ╲   Catches: integration bugs
       ╱────────────────╲
      ╱  Journal Replay  ╲  Replay production sequences against new code
     ╱    (regression)     ╲  Catches: behavioral regressions
    ╱──────────────────────╲
   ╱  Continuous Consistency ╲  Runtime invariants + reconciliation loop
  ╱    (production safety)    ╲  Catches: state divergence in prod
 ╱────────────────────────────╲
╱  Property + Fuzz + Unit       ╲  Automated, fast, 1000s of cases
│  (highest ROI for solo dev)    │  Catches: logic bugs, edge cases
└────────────────────────────────┘
```

### 14.2 Property-Based Testing (`proptest`)

Generate random sequences and verify invariants always hold. These are the
**mathematical properties** of the system — if any of these fail, there is a
logic bug regardless of the specific inputs.

| Invariant | Property | Crate |
|-----------|----------|-------|
| Firm book consistency | `sum(agent_allocations.used_capital) == firm.total_used` | `types` |
| Position math | No sequence of fills produces negative qty without explicit short | `types` |
| Fee non-negative | `fee.net >= 0` for all non-rebate fee models | `fees` |
| Order FSM completeness | No random event sequence reaches an illegal transition | `types` |
| Order FSM terminal states | Filled, Cancelled, Rejected, Expired are absorbing | `types` |
| Risk gate determinism | Same order + same state → same approval/rejection | `risk` |
| Risk gate monotonicity | Tighter limits never approve more orders than looser limits | `risk` |
| Reconciliation idempotency | `reconcile(); reconcile();` produces same state | `store` |
| Decimal precision | No operation loses precision below tick size | `types` |
| Snapshot consistency | `restore(snapshot) + replay(tail) == current_state` | `store` |

**Implementation rule:** Every crate that contains financial logic must have a
`tests/properties.rs` file with proptest strategies. PR merge is blocked if
property test coverage decreases.

### 14.3 Journal Replay for Regression Testing

The journal enables replaying **real production decision sequences** against new code.
This is the bridge between "tests pass" and "works in production."

```
┌─────────────────────────────────────────────────────────┐
│                   REGRESSION REPLAY                      │
│                                                          │
│  1. Export journal segment from paper/live               │
│     journal export --from 2026-03-10 --to 2026-03-17    │
│     → events.jsonl (all events for that period)          │
│                                                          │
│  2. Replay against CURRENT code (baseline)               │
│     journal replay events.jsonl --checkpoint-every 100   │
│     → baseline_states.jsonl (state at each checkpoint)   │
│                                                          │
│  3. Make code change (risk gate, sizing, fee model)      │
│                                                          │
│  4. Replay against NEW code                              │
│     journal replay events.jsonl --checkpoint-every 100   │
│     → new_states.jsonl                                   │
│                                                          │
│  5. Diff                                                 │
│     journal diff baseline_states.jsonl new_states.jsonl  │
│     → shows exactly which decisions changed and why      │
│                                                          │
│  SCOPE: This tests deterministic internal logic:         │
│    ✓ Risk gate approve/reject decisions                  │
│    ✓ Position sizing calculations                        │
│    ✓ Fee calculations                                    │
│    ✓ State machine transitions                           │
│    ✗ NOT fill outcomes (those depend on market state)    │
│    ✗ NOT venue interaction (those depend on network)     │
└─────────────────────────────────────────────────────────┘
```

**CI integration:** Maintain a `fixtures/golden-journal/` directory with curated
journal segments covering known edge cases. These replay in CI on every commit.
Any behavioral change must be explicitly acknowledged in the PR.

### 14.4 Continuous State Consistency Checker

**This is the production safety net.** Runs continuously in the live engine,
not just during testing. Catches state divergence before it causes financial harm.

```rust
/// Continuous consistency checker. Runs as a background task in the engine.
/// Validates that internal state matches external reality.
pub struct ConsistencyChecker {
    /// How often to run full reconciliation against venues.
    pub reconciliation_interval: Duration,  // default: 60s

    /// How often to run internal self-checks.
    pub self_check_interval: Duration,      // default: 5s

    /// Maximum tolerated divergence before alerting.
    pub divergence_threshold: Decimal,      // default: 0.001 (0.1%)

    /// Action on consistency failure.
    pub on_failure: ConsistencyFailureAction,
}

pub enum ConsistencyFailureAction {
    /// Log warning, continue trading. For soft divergences.
    AlertOnly,
    /// Pause the divergent agent. For per-agent divergences.
    PauseAgent,
    /// Halt all trading. For firm-level divergences.
    HaltAll,
}
```

**Self-checks (every 5s, no external calls):**

| Check | Invariant | On Failure |
|-------|-----------|------------|
| Position-fill consistency | Position qty == sum of all fill qtys for that position | Alert + reconcile |
| Agent budget accounting | used_capital + free_capital == allocated_capital (± unrealized P&L) | Alert |
| Firm book rollup | sum(agent positions) == firm exposure per instrument | Alert |
| Order state liveness | No order stuck in `Submitted` state > 30s without venue ack | Alert + cancel |
| Journal continuity | No sequence gaps in journal | Panic (data corruption) |
| Snapshot freshness | Last snapshot < 2x snapshot interval old | Force snapshot |

**Venue reconciliation (every 60s, queries venues):**

| Check | How | On Failure |
|-------|-----|------------|
| Position match | Compare internal positions vs `adapter.get_positions()` | PositionDivergence event → correct internal |
| Balance match | Compare tracked balance vs `adapter.get_balance()` | Alert |
| Open order match | Compare tracked orders vs `adapter.get_open_orders()` | Cancel orphans, re-track missing |
| Fill gap detection | Check for fills we missed (venue has fills we didn't process) | Re-fetch and apply missing fills |

### 14.5 Runtime Invariant Assertions (Production)

Compiled into the production binary. These are not debug-only — they run on every
operation in live trading. The cost is <1μs per check; the value is catching
corruption before it compounds.

| Assertion | When | Action on Failure |
|-----------|------|-------------------|
| Position didn't flip sign unexpectedly | After every fill | Alert + flag for review |
| Fill qty > 0 and fill price > 0 | On every fill received | Reject fill, alert |
| Fee is finite and non-NaN | On every fee calculation | Reject, use worst-case estimate |
| `dry_run` flag respected | Before every venue call | **Hard reject** (never bypassable) |
| Event sequence is monotonic | Every journal append | **Panic** (data corruption) |
| Agent budget >= 0 after fill | After position update | Pause agent, alert |
| Order exists in state before fill | On fill received | Alert, create order record, reconcile |
| No duplicate `client_order_id` | On order creation | Return cached ack (idempotent) |

### 14.6 Fuzzing (`cargo-fuzz`)

| Target | Fuzzes | Goal |
|--------|--------|------|
| `order_fsm` | Random event sequences against state machine | No panics, no illegal states |
| `risk_gate` | Random orders + random state combinations | No panics, deterministic decisions |
| `fee_calculator` | Extreme values: zero, MAX, negative, NaN-adjacent | No panics, bounded output |
| `signal_to_order` | Random signals with edge-case convictions (0.0, 1.0, >1.0) | No panics, valid or rejected |
| `journal_replay` | Corrupted event streams, truncated journals | Graceful error, no state corruption |
| `adapter_response` | Random/malformed venue API responses | Graceful error, no state mutation |
| `decimal_arithmetic` | Overflow, underflow, division edge cases | No panics, bounded results |

### 14.7 Shadow Mode

Before cutting over to production:

```
Live orders ──► Current system (executes)
                     │
                     └──► Rust engine (shadow mode: evaluates, does not execute)
                              │
                              ├──► Divergence log (did decisions differ?)
                              ├──► Latency comparison (is Rust faster?)
                              └──► State consistency (does Rust state match?)
```

**Shadow mode exit criteria (all must be true for 7 consecutive days):**
- Zero risk gate disagreements on orders that should have been rejected
- Risk gate approval rate within 2% of current system
- Position state matches current system after every reconciliation cycle
- No panics or unhandled errors in Rust engine logs
- P99 latency of Rust engine < P50 latency of current system

### 14.8 Graduated Deployment Pipeline

| Stage | Gate | Promotes when |
|-------|------|--------------|
| 1. Unit + property tests | CI | All pass, no property regressions |
| 2. Fuzz (30min runs) | CI (nightly) | No crashes in 10M iterations |
| 3. Journal replay | CI | Zero decision regressions vs golden fixtures |
| 4. Paper trading (live data, sim fills) | Manual | 1 week, no state divergences |
| 5. Testnet (real venue APIs, test money) | Manual | 1 week, all venues connect, orders round-trip |
| 6. Shadow mode (live data, no execution) | Manual | 1 week, meets shadow exit criteria above |
| 7. Small live ($100 budget per agent) | Manual | 1 week, fills match, reconciliation clean |
| 8. Scaled live | Manual | Ongoing monitoring via consistency checker |

**Rule: stages are monotonic.** You don't skip stages. You don't compress timelines.
A bug found at stage N sends you back to stage 1 with a new property test or
fuzz target that catches that class of bug, then you re-promote through all stages.

### 14.9 What Each Verification Layer Catches

| Bug class | Unit | Property | Fuzz | Replay | Shadow | Live |
|-----------|:----:|:--------:|:----:|:------:|:------:|:----:|
| Logic errors (wrong math) | **X** | **X** | | | | |
| Edge cases (boundary values) | | **X** | **X** | | | |
| State machine violations | | **X** | **X** | | | |
| Regressions from code changes | | | | **X** | | |
| Integration bugs (venue API) | | | | | **X** | **X** |
| Concurrency bugs (race conditions) | | | **X** | | **X** | **X** |
| State drift over time | | | | | | **X** |
| Unknown unknowns | | | | | | **X** |

Each layer catches bugs the layers below cannot. No single layer is sufficient.

---

## 15. Concurrency & Parallelism

> **Core model:** Single-owner state with message passing. No shared mutable state
> behind locks in business logic. The engine uses the actor pattern — one task owns
> each piece of mutable state, other tasks communicate with it via typed channels.
>
> This is an explicit architectural choice. `Arc<RwLock<FirmBook>>` is banned.
> Not because Rust can't make it safe — but because lock-based shared state creates
> implicit coupling between components, makes deadlocks possible, and makes the
> data flow impossible to trace in production debugging.

### 15.1 Task Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           TOKIO RUNTIME                                      │
│                                                                              │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │                    SEQUENCER TASK (single, owns all mutable state)    │   │
│  │                                                                       │   │
│  │  Owns:                                                                │   │
│  │    - FirmBook (positions, allocations, P&L)                          │   │
│  │    - OrderBook (all in-flight and historical orders)                  │   │
│  │    - RiskGate state (current limits, agent statuses)                  │   │
│  │    - Journal writer (append-only, monotonic seq)                      │   │
│  │    - Idempotency cache (client_order_id → OrderAck)                  │   │
│  │                                                                       │   │
│  │  Processes (in order, one at a time):                                 │   │
│  │    1. Incoming orders (validate → risk check → approve/reject)       │   │
│  │    2. Fill events (update position, update P&L, update firm book)    │   │
│  │    3. Reconciliation results (correct state, emit divergence events) │   │
│  │    4. Risk limit changes (from Taleb L2 or Feynman L3)              │   │
│  │                                                                       │   │
│  │  Emits:                                                               │   │
│  │    - Approved orders → Execution Dispatcher                          │   │
│  │    - Events → Journal, Dashboard, Bus Bridge                         │   │
│  │    - Rejections → back to caller via oneshot channel                  │   │
│  └────────────────────────────▲──────┬───────────────────────────────────┘   │
│                               │      │                                       │
│            ┌──────────────────┘      └──────────────────┐                   │
│            │ (commands in)                (events out)    │                   │
│            │                                              │                   │
│  ┌─────────┴──────────┐                    ┌─────────────▼────────────┐     │
│  │  INGRESS TASKS      │                    │  EXECUTION DISPATCHER    │     │
│  │  (concurrent)       │                    │  (concurrent per-venue)  │     │
│  │                     │                    │                          │     │
│  │  - gRPC handlers    │                    │  Receives approved       │     │
│  │  - Redis bus        │                    │  orders from Sequencer.  │     │
│  │    consumer         │                    │  Dispatches to venue     │     │
│  │  - Dashboard API    │                    │  adapter tasks.          │     │
│  │    (read-only       │                    │  Returns fills to        │     │
│  │     snapshot)       │                    │  Sequencer.              │     │
│  └─────────────────────┘                    └──────────────┬───────────┘     │
│                                                            │                  │
│                                              ┌─────────────▼──────────────┐  │
│                                              │  VENUE ADAPTER TASKS       │  │
│                                              │  (one per venue connection) │  │
│                                              │                            │  │
│                                              │  - Bybit WS + REST        │  │
│                                              │  - Hyperliquid WS + REST  │  │
│                                              │  - Polymarket WS + REST   │  │
│                                              │  - Binance WS + REST      │  │
│                                              │  - dYdX WS + REST         │  │
│                                              │                            │  │
│                                              │  Each task owns its own:   │  │
│                                              │  - WebSocket connection    │  │
│                                              │  - Rate limiter state      │  │
│                                              │  - Reconnection state      │  │
│                                              └────────────────────────────┘  │
│                                                                              │
│  ┌────────────────────────────────────────────────────────────────────────┐  │
│  │  BACKGROUND TASKS (independent, periodic)                              │  │
│  │                                                                        │  │
│  │  - Consistency checker (reads state snapshot, queries venues)          │  │
│  │  - Snapshot writer (periodic state serialization)                      │  │
│  │  - Bus bridge (forwards events from Sequencer → Redis Streams)        │  │
│  │  - Metrics exporter (reads state snapshot → Prometheus)                │  │
│  └────────────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 15.2 The Sequencer: Single Owner of Mutable State

The Sequencer is the heart of the concurrency model. It is a single tokio task
that owns all mutable business state. No other task can read or write this state
directly — they interact via channels.

```rust
/// The Sequencer is the single owner of all mutable engine state.
/// It processes commands and events sequentially, guaranteeing:
///   - No data races (sole ownership, no locks)
///   - Deterministic ordering (events processed in arrival order)
///   - Consistent state (risk checks see the same state as position updates)
///   - Traceable data flow (every state change has a causal event)
pub struct Sequencer {
    // ── Core decision logic (synchronous, runtime-agnostic) ──
    core: EngineCore,

    // ── Runtime concerns (async, live-mode only) ──
    journal: Box<dyn Journal>,
    idempotency_cache: HashMap<String, OrderAck>,

    // ── Input channels (commands from other tasks) ──
    command_rx: mpsc::Receiver<SequencerCommand>,

    // ── Output channels (events to other tasks) ──
    execution_tx: mpsc::Sender<ApprovedOrder>,
    event_broadcast: broadcast::Sender<Event>,

    // ── Monotonic sequence counter ──
    next_seq: u64,
}

/// Core decision logic — synchronous, no async, no channels, no runtime dependency.
///
/// Owned exclusively by the Sequencer task. All modes (live, paper, backtest) use
/// the same Sequencer → same EngineCore. Strategies never import this directly —
/// they interact via gRPC.
///
/// All methods are &mut self (exclusive ownership) — no locking needed.
/// All methods are synchronous — no .await, no channels, no tokio dependency.
/// All financial math uses Decimal — no f64 anywhere inside the engine.
pub struct EngineCore {
    pub state: EngineState,
    pub risk_gate: RiskGate,
    pub sizer: Box<dyn PositionSizer>,
    pub fee_model: Box<dyn FeeModel>,
}

impl EngineCore {
    /// Evaluate a signal: sizing → order construction.
    /// Returns zero or more orders ready for risk evaluation.
    pub fn signal_to_orders(&mut self, signal: &Signal) -> Result<Vec<CanonicalOrder>> {
        // 1. Check agent has budget
        // 2. Size the position (conviction → notional → qty)
        // 3. Construct CanonicalOrder(s)
        // Pure logic, no I/O
    }

    /// Evaluate an order against all risk limits.
    /// Returns approval or rejection with specific violations.
    pub fn evaluate_risk(
        &self,
        order: &CanonicalOrder,
    ) -> Result<RiskApproval, Vec<RiskViolation>> {
        // L0 circuit breakers + L1 agent risk gate
        // Reads self.state (firm book, limits) — consistent because single owner
        // Pure logic, no I/O
    }

    /// Apply a fill to engine state.
    /// Updates: position, agent allocation, firm book, order record.
    pub fn on_fill(&mut self, fill: &Fill) -> Result<Vec<EventKind>> {
        // Returns events that were generated (for journal/broadcast)
        // Pure state mutation, no I/O
    }

    /// Apply a reconciliation correction.
    pub fn on_reconciliation(&mut self, result: &ReconciliationResult) -> Result<Vec<EventKind>> {
        // Pure state mutation, no I/O
    }

    /// Adjust risk limits (from Taleb L2 or Feynman L3).
    pub fn adjust_limits(&mut self, agent: &AgentId, limits: AgentRiskLimits) -> Result<()> {
        // Pure state mutation, no I/O
    }

    /// Snapshot current state (cheap clone for read-only consumers).
    pub fn snapshot(&self) -> EngineStateSnapshot { ... }
}

/// All mutable state lives here.
pub struct EngineState {
    pub firm_book: FirmBook,
    pub orders: HashMap<OrderId, OrderRecord>,
    pub agent_allocations: HashMap<AgentId, AgentAllocation>,
    pub agent_statuses: HashMap<AgentId, AgentStatus>,
    pub risk_limits: RiskLimits,
}

/// Commands sent TO the Sequencer from other tasks.
pub enum SequencerCommand {
    /// New order request (from gRPC or bus). Includes a oneshot for the response.
    SubmitOrder {
        order: CanonicalOrder,
        respond: oneshot::Sender<Result<OrderAck>>,
    },
    /// New signal (from LLM agent). Needs sizing before risk check.
    SubmitSignal {
        signal: Signal,
        respond: oneshot::Sender<Result<SignalAck>>,
    },
    /// Fill received from venue adapter.
    OnFill {
        fill: Fill,
    },
    /// Reconciliation result from consistency checker.
    OnReconciliation {
        results: Vec<ReconciliationResult>,
    },
    /// Risk limit change (from Taleb L2 or Feynman L3).
    AdjustLimits {
        agent: AgentId,
        limits: AgentRiskLimits,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Pause/resume/halt commands.
    PauseAgent { agent: AgentId, reason: String },
    ResumeAgent { agent: AgentId },
    HaltAll { reason: String },
    /// Request a read-only snapshot (for dashboard, metrics, consistency checker).
    Snapshot {
        respond: oneshot::Sender<EngineStateSnapshot>,
    },
}
```

**Why not `Arc<RwLock<EngineState>>`?**

| | Single-owner (Sequencer) | Shared lock (`Arc<RwLock>`) |
|---|---|---|
| Data races | Impossible (sole owner) | Possible if lock discipline breaks |
| Deadlocks | Impossible (no locks) | Possible with multiple locks |
| Read consistency | Guaranteed (sequential processing) | Snapshot may be stale between reads |
| Debugging | Trace channel messages | Trace lock acquisitions (hard) |
| Risk gate sees stale state | Never | Yes, if another task writes between read and check |
| Performance | Bounded by sequencer throughput | Bounded by lock contention |
| Backtest determinism | Guaranteed (single-threaded processing) | Non-deterministic lock ordering |

The Sequencer processes ~10k–100k commands/sec (validate + risk check is <100μs).
This is not the bottleneck. Venue API latency (10ms–500ms) is the bottleneck,
and that happens *outside* the Sequencer in parallel venue tasks.

### 15.3 Channel Topology

Every channel is explicit, typed, and uni-directional. No task reaches into
another task's state.

```
gRPC handlers ──┐
                 ├──► mpsc::channel ──► SEQUENCER ──► mpsc::channel ──► Execution Dispatcher
Redis consumer ──┘         ▲                │                                  │
                           │                │                                  ▼
                   oneshot (response)        │                          Venue Adapter Tasks
                                            │                                  │
                                            ▼                                  │
                                    broadcast::channel                         │
                                            │                          mpsc (fills back)
                                  ┌─────────┼─────────┐                        │
                                  ▼         ▼         ▼                        │
                              Journal   Dashboard   Bus Bridge          ───────┘
                              Writer    (SSE)       (Redis pub)    back to Sequencer
```

**Channel types and why:**

| Channel | Type | Bounded? | Why |
|---------|------|----------|-----|
| Ingress → Sequencer | `mpsc::channel(1024)` | Yes, bounded | Backpressure: if Sequencer is behind, callers wait. Prevents unbounded queue growth. |
| Sequencer → Execution Dispatcher | `mpsc::channel(256)` | Yes, bounded | Backpressure: if venues are slow, Sequencer stops approving new orders. Explicit signal. |
| Sequencer → Event consumers | `broadcast::channel(4096)` | Yes, bounded | Dashboard/journal can lag; oldest events dropped with warning. Lagging consumers don't block Sequencer. |
| Execution Dispatcher → Venue task | `mpsc::channel(64)` per venue | Yes, bounded | Per-venue backpressure. One slow venue doesn't block others. |
| Venue task → Sequencer | `mpsc::channel(1024)` | Yes, bounded | Fills flow back. Same channel as ingress (multiplexed via `SequencerCommand`). |
| gRPC handler → Sequencer | `oneshot::channel` | N/A | One response per request. Caller awaits. Timeout enforced by gRPC layer. |

**Bounded channels are mandatory.** No `mpsc::unbounded_channel()` in production code.
Unbounded channels hide backpressure — the queue grows silently until OOM. Bounded
channels make overload visible and explicit: the caller blocks, the metric spikes,
the alert fires.

### 15.4 Read-Only Snapshots

The dashboard, metrics exporter, and consistency checker need to read engine state
without blocking the Sequencer. They request a snapshot via `SequencerCommand::Snapshot`:

```rust
/// Immutable, cloneable snapshot of engine state.
/// Cheap to produce (Sequencer clones its state periodically, not on every request).
/// Stale by design — consumers accept eventual consistency for reads.
#[derive(Clone)]
pub struct EngineStateSnapshot {
    pub firm_book: FirmBook,
    pub open_orders: Vec<OrderRecord>,
    pub agent_allocations: Vec<AgentAllocation>,
    pub risk_limits: RiskLimits,
    pub as_of_seq: u64,
    pub as_of: DateTime<Utc>,
}
```

The Sequencer maintains a cached snapshot, refreshed every 1s or on significant
state changes (fill, position update). Snapshot requests return the cached copy
without interrupting command processing.

**No live reads.** The dashboard does not see real-time state — it sees state as of
the last snapshot (up to 1s stale). This is an explicit design trade-off:
consistency checker runs on snapshots, not live state. If you need sub-second
reads, you increase snapshot frequency — you don't add a read lock.

### 15.5 Per-Venue Parallelism

Venue adapter tasks run independently. One slow or disconnected venue does not
affect others:

```rust
/// Each venue connection is an independent tokio task.
/// Owns its WebSocket, rate limiter, and reconnection state.
/// Communicates with Execution Dispatcher via channels only.
pub struct VenueTask {
    adapter: Box<dyn VenueAdapter>,
    /// Receives orders to submit from Execution Dispatcher.
    order_rx: mpsc::Receiver<ApprovedOrder>,
    /// Sends fills and events back to Sequencer.
    event_tx: mpsc::Sender<SequencerCommand>,
    /// Rate limiter state (owned, not shared).
    rate_limiter: VenueRateLimiter,
    /// Reconnection state (owned, not shared).
    reconnection: ReconnectionState,
}
```

**Execution Dispatcher** routes approved orders to the correct venue task:

```rust
/// Routes risk-approved orders to venue adapter tasks.
/// Stateless — just reads venue from the order and forwards.
pub struct ExecutionDispatcher {
    /// One sender per venue task.
    venue_txs: HashMap<VenueId, mpsc::Sender<ApprovedOrder>>,
    /// Receives approved orders from Sequencer.
    approved_rx: mpsc::Receiver<ApprovedOrder>,
}
```

**What happens when a venue is slow:**
1. Venue task's `order_rx` channel fills up (bounded at 64)
2. Execution Dispatcher's send to that venue blocks
3. Execution Dispatcher stops consuming from Sequencer's `execution_tx`
4. Sequencer's `execution_tx` fills up (bounded at 256)
5. Sequencer logs a warning: "execution backpressure — venue X is slow"
6. Sequencer continues processing fills and state updates, but new orders for that venue wait
7. Orders for other venues are unaffected (dispatched to their own channels)

This is explicit backpressure. No silent queue growth. No cross-venue interference.

### 15.6 What Is NOT Concurrent

These operations are deliberately serialized through the Sequencer:

| Operation | Why serialized |
|-----------|---------------|
| Risk evaluation | Must see consistent state. Two concurrent risk checks could both approve orders that together exceed the limit. |
| Position updates on fill | Fill A and fill B for the same instrument must update position sequentially. Concurrent updates can lose one. |
| Firm book rollup | Derived from positions. Must be consistent with latest fills. |
| Journal append | Monotonic seq requires serial writes. |
| Idempotency check | Must check-and-insert atomically. |
| Agent pause/resume | Must be visible to the next risk check immediately. |

This is not a performance concern. Risk evaluation is <100μs. Position math is <10μs.
Journal append is <1ms (fsync). The Sequencer can process >10k commands/sec.
The bottleneck is venue API latency (10ms–500ms), which happens outside the Sequencer.

### 15.7 Backtest Mode: Same Engine, External Clock

In backtest mode, the engine runs the same Sequencer, same `EngineCore`, same risk
pipeline — with simulated components swapped in via config.

| Component | Live | Paper | Backtest |
|-----------|------|-------|----------|
| Ingress | gRPC | gRPC | gRPC (same API — backtest harness is a client) |
| Sequencer | Same | Same | Same |
| Core logic | `EngineCore` | `EngineCore` | `EngineCore` |
| Clock | `WallClock` | `WallClock` | `SimulatedClock` (advanced via `AdvanceClock` RPC) |
| Venue adapters | Real (Bybit, HL, ...) | Real data, simulated fills | `SimulatedVenue` (fill model) |
| Financial math | `Decimal` | `Decimal` | `Decimal` throughout |
| Journal | SQLite/Postgres | SQLite/Postgres | In-memory (or disabled) |
| State | Persisted | Persisted | In-memory |

**Key design decision: the engine is always a service.** Even in backtest, strategies
talk to it via gRPC. The backtest harness (HFTBacktest, custom replay, etc.) lives
in the strategy layer and submits orders to the engine like any other client.
See [STRATEGY_ENGINE_BOUNDARY.md](./STRATEGY_ENGINE_BOUNDARY.md) for the full contract.

**Clock synchronization:** The backtest harness advances the engine's simulated clock
via an `AdvanceClock` RPC (rejected in live/paper mode). This keeps the engine's
internal time (used for TIF expiry, funding rate calculations, risk window checks)
in sync with the harness's simulated time.

**SimulatedVenue adapter** implements the same `VenueAdapter` trait as real adapters.
It models fills against an order book snapshot. For basic strategy testing, this is
sufficient. For high-fidelity fill simulation with queue-position modeling, the
strategy layer uses [HFTBacktest](https://github.com/nkaz001/hftbacktest) — but
HFTBacktest runs in the strategy layer, not inside the engine.

Because the Sequencer is single-threaded, backtest is automatically deterministic.
Same events in same order → same state. No lock-ordering non-determinism.

### 15.8 Rules

1. **No `Arc<Mutex<T>>` or `Arc<RwLock<T>>` on business state.** The Sequencer owns it.
2. **No `mpsc::unbounded_channel()` in production.** All channels are bounded with explicit capacity.
3. **No task reads another task's state directly.** Communication is via channels only.
4. **No `.await` while holding mutable business state.** The Sequencer processes commands synchronously within its event loop; async is only for channel recv/send.
5. **Every channel has a name, capacity, and backpressure strategy** documented in this section.
6. **Timeouts are explicit on every channel send.** No infinite waits. If a send blocks for >5s, log error and drop the message (with event).

---

## 16. Crate Structure

```
feynman-engine/
├── Cargo.toml                        # Workspace root
├── crates/
│   ├── types/                        # Core types: OrderId, CanonicalOrder, Fill, etc.
│   │   └── src/lib.rs                # Zero external deps. Used by everything.
│   │
│   ├── fees/                         # FeeModel trait + implementations
│   │   └── src/
│   │       ├── lib.rs                # Trait definition
│   │       ├── maker_taker.rs        # Simple maker/taker model
│   │       ├── tiered.rs             # Volume-tiered model
│   │       └── gas_aware.rs          # On-chain gas estimation
│   │
│   ├── bus/                          # MessageBus trait + Redis Streams impl
│   │   └── src/
│   │       ├── lib.rs                # Trait definition
│   │       ├── redis.rs              # Redis Streams implementation
│   │       └── memory.rs             # In-memory impl (for testing)
│   │
│   ├── store/                        # EventStore + StateStore traits + impls
│   │   └── src/
│   │       ├── lib.rs                # Trait definitions
│   │       ├── sqlite.rs             # SQLite implementation
│   │       ├── memory.rs             # In-memory impl (backtest + testing)
│   │       └── postgres.rs           # Future: Postgres implementation
│   │
│   ├── risk/                         # CircuitBreaker + RiskGate
│   │   └── src/
│   │       ├── lib.rs                # Trait definitions
│   │       ├── circuit_breaker.rs    # Layer 0 hardcoded checks
│   │       └── risk_gate.rs          # Layer 1 configurable checks
│   │
│   ├── adapters/                     # VenueAdapter trait + implementations
│   │   ├── core/                     # Trait definition + shared utilities
│   │   ├── bybit/                    # Bybit adapter
│   │   ├── hyperliquid/              # Hyperliquid adapter (alloy for signing)
│   │   ├── polymarket/               # Polymarket CLOB adapter (alloy for EIP-712)
│   │   ├── binance/                  # Binance adapter
│   │   ├── alpaca/                   # Alpaca adapter
│   │   ├── ibkr/                     # Interactive Brokers adapter
│   │   ├── deribit/                  # Deribit adapter
│   │   └── simulated/                # FillSimulator for backtest/paper
│   │
│   ├── sequencer/                    # EventSequencer trait + impls
│   │   └── src/
│   │       ├── lib.rs                # Trait definition + live FIFO impl
│   │       └── backtest.rs           # Sorted/deterministic impl
│   │
│   ├── gateway/                      # ExecutionGateway + ExecStrategy + Pipeline
│   │   └── src/
│   │       ├── lib.rs                # Gateway orchestration
│   │       ├── pipeline.rs           # PipelineStage trait + composition
│   │       ├── validator.rs          # OrderValidator (stateless pre-checks)
│   │       ├── emulator.rs           # OrderEmulator (complex → simple decomp)
│   │       ├── portfolio.rs          # Signal-to-order generation
│   │       ├── router.rs             # Order routing
│   │       ├── lifecycle.rs          # Order state machine
│   │       ├── shutdown.rs           # Graceful shutdown policy
│   │       └── strategies/
│   │           ├── direct.rs         # Market/Limit — just send
│   │           ├── limit_chase.rs    # Limit → chase if unfilled
│   │           ├── twap.rs           # Time-weighted split
│   │           └── depth_aware.rs    # Orderbook-aware splitting
│   │
│   ├── context/                      # ExecutionContext trait + impls
│   │   └── src/
│   │       ├── lib.rs                # Trait definition
│   │       ├── live.rs               # Live trading context
│   │       ├── paper.rs              # Paper trading context
│   │       └── backtest.rs           # Backtest context
│   │
│   ├── reconciler/                   # Position reconciliation
│   │   └── src/lib.rs
│   │
│   ├── observability/                # Metrics, tracing, dashboard data
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── metrics.rs            # Prometheus metrics
│   │       ├── tracing.rs            # OpenTelemetry spans
│   │       └── dashboard.rs          # Dashboard API types
│   │
│   └── api/                          # gRPC service + dashboard HTTP
│       └── src/
│           ├── grpc.rs               # tonic gRPC for agent communication
│           └── http.rs               # axum HTTP for dashboard
│
├── bins/
│   ├── feynman-engine/               # Main binary: gateway + bus + API + dashboard
│   ├── feynman-backtest/             # Backtest runner CLI
│   └── feynman-paper/                # Paper trading runner
│
├── proto/
│   └── feynman.proto                 # gRPC service definitions
│
└── tests/
    ├── property/                     # proptest suites
    ├── replay/                       # deterministic replay tests
    └── integration/                  # multi-crate integration tests
```

### Key Rust Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `rust_decimal` | All financial math |
| `fred` or `redis` | Redis Streams client |
| `sqlx` | Async SQLite/Postgres (compile-time checked) |
| `tonic` + `prost` | gRPC |
| `axum` | Dashboard HTTP API |
| `alloy` | Ethereum/Polygon signing (Hyperliquid, Polymarket) |
| `tracing` + `tracing-opentelemetry` | Structured logging + OTLP export |
| `prometheus-client` | Metrics export |
| `serde` + `serde_json` | Serialization |
| `proptest` | Property-based testing |
| `criterion` | Benchmarking |
| `chrono` | DateTime handling |

---

## 17. Migration Path

| Phase | Scope | Validates |
|-------|-------|-----------|
| **0** | `types` + `fees` + `store` + `risk` crates. No venue connectivity. | Core types compile. Fee math is correct. State store round-trips. Risk gate evaluates correctly. Property tests pass. |
| **1** | `adapters/bybit` + `gateway` + `api`. gRPC API. | Rust engine replaces Node.js executor for Bybit. MCP bridge calls gRPC instead of direct Bybit. Shadow mode validates parity. |
| **2** | `bus` (Redis Streams) + `reconciler`. | Redis bus replaces SQLite bus. Reconciliation loop catches drift. Bus message acknowledgment works. |
| **3** | `context/backtest` + `adapters/simulated`. | Backtest engine using same strategy code as live. OHLCV replay works. Fill simulation produces reasonable results. |
| **4** | `adapters/hyperliquid` + `adapters/polymarket`. | New venues via adapter pattern. Wallet signing works. Polymarket resolution tracking works. |
| **5** | `observability` + dashboard. | Metrics export, tracing, dashboard serves real-time data. |
| **6** | Additional adapters (Binance, Alpaca, IBKR, Deribit) as needed. L2 exec strategies. | Added only when actively trading on that venue. |

**Rule:** Don't build adapters for venues you're not actively trading on. The adapter pattern means adding one later is isolated work — no gateway, bus, or risk changes.

---

## Appendix A: Venue Order Type Matrix

### Order Types

| Feature | Bybit | Binance | Alpaca | Hyperliquid | Polymarket | IBKR | Deribit |
|---------|-------|---------|--------|-------------|------------|------|---------|
| Market | ✓ | ✓ | ✓ | via IOC limit | ✗ | ✓ | ✓ |
| Limit | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Stop Market | ✓ (trigger) | ✓ | ✓ | ✓ (trigger) | ✗ | ✓ (STP) | ✓ |
| Stop Limit | ✓ (trigger) | ✓ | ✓ | ✓ (trigger) | ✗ | ✓ (STP LMT) | ✓ |
| Take Profit | ✓ (TP/SL) | ✓ | via bracket | ✓ (trigger+tp) | ✗ | via bracket | ✓ |
| Trailing Stop | ✓ | ✓ | ✓ ($/%) | ✗ | ✗ | ✓ ($/%) | ✓ (offset) |
| Market-to-Limit | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (MTL) | ✓ |
| Market-if-Touched | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (MIT) | ✗ |
| Limit-if-Touched | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (LIT) | ✗ |
| Pegged | BBO | ✓ (pegPrice) | ✗ | ✗ | ✗ | ✓ (6+ types) | ✗ |
| Iceberg | ✗ | ✓ (icebergQty) | ✗ | ✗ | ✗ | ✓ (displaySize) | ✓ (display_amount) |
| TWAP | ✗ | ✗ | ✗ | ✓ (native) | ✗ | ✓ (algo) | ✗ |
| VWAP | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (algo) | ✗ |
| Scale | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | ✗ |
| Volatility | orderIv (opts) | ✗ | ✗ | ✗ | ✗ | ✓ (VOL) | ✓ (implv) |
| Conditional | ✓ (triggerPrice) | ✗ | ✗ | ✗ | ✗ | ✓ (conditions) | ✗ |

### Time-in-Force

| TIF | Bybit | Binance | Alpaca | Hyperliquid | Polymarket | IBKR | Deribit |
|-----|-------|---------|--------|-------------|------------|------|---------|
| GTC | ✓ | ✓ | ✓ | ✓ | ✓ (type) | ✓ | ✓ |
| IOC | ✓ | ✓ | ✓ | ✓ | ✗ | ✓ | ✓ |
| FOK | ✓ | ✓ | ✓ | ✗ | ✓ (type) | ✓ | ✓ |
| Day | ✗ | ✗ | ✓ | ✗ | ✗ | ✓ | ✓ (GTD) |
| GTD | ✗ | ✓ (futures) | ✗ | ✗ | ✓ (type) | ✓ | ✗ |
| At Open | ✗ | ✗ | ✓ (opg) | ✗ | ✗ | ✓ (OPG) | ✗ |
| At Close | ✗ | ✗ | ✓ (cls) | ✗ | ✗ | ✓ (MOC/LOC) | ✗ |
| Post-Only | ✓ (TIF) | ✓ (GTX/type) | ✗ | ✓ (Alo) | ✓ (flag) | notHeld | ✓ (flag) |
| FAK | ✗ | ✗ | ✗ | ✗ | ✓ (type) | ✗ | ✗ |

### Composite / Linked Orders

| Feature | Bybit | Binance | Alpaca | Hyperliquid | Polymarket | IBKR | Deribit |
|---------|-------|---------|--------|-------------|------------|------|---------|
| TP/SL attached | ✓ (inline) | ✗ | ✗ | ✓ (grouping) | ✗ | ✗ | ✗ |
| Bracket | TP/SL | ✓ (OTOCO spot) | ✓ (order_class) | ✗ | ✗ | ✓ (parentId) | ✓ (otoco_config) |
| OCO | ✓ | ✓ (spot) | ✓ | ✗ | ✗ | ✓ (OCA group) | ✓ |
| OTO | ✗ | ✓ (spot) | ✓ | ✗ | ✗ | ✓ (parentId) | ✓ |

### Order Flags

| Flag | Bybit | Binance | Alpaca | Hyperliquid | Polymarket | IBKR | Deribit |
|------|-------|---------|--------|-------------|------------|------|---------|
| Reduce-Only | ✓ | ✓ (futures) | ✗ | ✓ | ✗ | ✗ | ✓ |
| Close-on-Trigger | ✓ | ✓ (closePosition) | ✗ | ✗ | ✗ | ✗ | ✗ |
| Hidden/Iceberg | ✗ | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ |
| Self-Trade Prevention | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ |
| Market Maker Protection | ✓ (opts) | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ |
| Extended Hours | ✗ | ✗ | ✓ | ✗ | ✗ | ✓ (outsideRth) | ✗ |

### Auth & Settlement

| Property | Bybit | Binance | Alpaca | Hyperliquid | Polymarket | IBKR | Deribit |
|----------|-------|---------|--------|-------------|------------|------|---------|
| Auth | API key | API key | API key | Wallet (L1) | Wallet (Polygon) | API + session | API key |
| Settlement | Custodial | Custodial | Custodial | On-chain | On-chain | Custodial | Custodial |

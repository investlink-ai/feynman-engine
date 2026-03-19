# AGENTS.md — feynman-engine

Canonical reference for any AI agent working on this codebase. If you are Claude Code, read `CLAUDE.md` first (it has workflow specifics), then this file.

---

## 1. What This Engine Is

A Rust execution engine for Feynman Capital. Receives signals/orders from strategy clients via gRPC, applies layered risk checks, routes orders to crypto exchanges and prediction markets.

**The engine is always a service.** No strategy imports engine internals. The gRPC boundary is the only interface — this guarantees risk enforcement is never bypassed. Mode (live/paper/backtest) is a deployment config (`FEYNMAN_MODE`), not a code change.

**Client types:** LLM agents (`SubmitSignal`), algo strategies (`SubmitOrder`), ML models (`SubmitBatch`), backtest harnesses (`SubmitOrder` + `AdvanceClock`). All go through the same gRPC API, all subject to the same risk gates.

**Phases:**
- **Phase 0:** Custom types, agent risk manager, Redis Streams bus, gRPC API scaffold
- **Phase 1:** Order FSM, venue adapters (per-exchange crates), execution pipeline, multi-client ingress
- **Phase 2:** Backtest engine, fill simulation, journal replay
- **Phase 3+:** Shadow mode, cutover, strategy engine service

---

## 2. Architecture

### System Topology

```
┌─────────────────────────────────┐
│     STRATEGY LAYER              │
│  (external — not this repo)     │
│                                 │
│  LLM agents (Satoshi, Taleb)   │
│  Algo strategies                │
│  ML models                      │
│  Backtest harnesses (HFTBacktest)│
└───────────────┬─────────────────┘
                │ gRPC (port 50051)
                ▼
┌──────────────────────────────────┐
│  feynman-engine (this repo)      │
│                                  │
│  Sequencer ─── EngineCore        │
│    │           (risk, sizing,    │
│    │            state, pipeline) │
│    ▼                             │
│  Execution Dispatcher            │
│    │                             │
│    ├─► Bybit adapter             │
│    ├─► Hyperliquid adapter       │
│    ├─► Polymarket adapter        │
│    ├─► Binance adapter           │
│    └─► SimulatedVenue (backtest) │
│                                  │
│  Redis Streams bus (cross-svc)   │
│  Dashboard (REST :8080, SSE)     │
└──────────────────────────────────┘
```

### Crate Dependency Order

```
types (no deps except std + serde)
  ↓
risk, bus (depend only on types)
  ↓
gateway, engine-core (depend on types + risk/bus)
  ↓
observability, api (depend on engine-core)
  ↓
feynman-engine binary (orchestrates all)
```

No circular dependencies. Never reverse this order.

### Order Pipeline

```
Signal/Order (from gRPC)
  → Grouper (coalesce by instrument)
  → Detector (identify opportunity)
  → Sizing (conviction → notional → qty)
  → Risk (AgentRiskManager — see §6)
  → ExecutionController (route to venue)
  → Fill (from venue)
  → Position update (after fill confirmation only)
```

### Concurrency Model

**Single-owner state with message passing.** The Sequencer task owns all mutable business state (`FirmBook`, positions, risk limits, orders). Other tasks communicate via bounded channels. No `Arc<RwLock<T>>` on business state.

- `EngineCore` — synchronous decision logic (risk, sizing, state) inside the Sequencer
- Venue adapter tasks — one per venue, independent, own their WebSocket/rate limiter
- Background tasks — consistency checker, snapshot writer, bus bridge, metrics
- All channels bounded with explicit capacity. No `mpsc::unbounded_channel()`.
- **Priority queueing:** Two channels — high-priority (fills, HaltAll, reconciliation, `urgency=Immediate`) and normal-priority (orders, batches, snapshots). Biased `tokio::select!` drains high before normal.
- **Poison message handling:** `catch_unwind` around every Sequencer command. Single panic → logged + alerted, command dropped. >3 panics in 60s → `HaltAll`.

See `docs/CORE_ENGINE_DESIGN.md` §15 for full concurrency architecture.

### Execution Modes

| Mode | Venues | Clock | State | Config |
|------|--------|-------|-------|--------|
| **Live** | Real adapters | Wall clock | Persisted | `FEYNMAN_MODE=live` |
| **Paper** | Real WebSocket orderbook, local `FillSimulator` | Wall clock | Persisted | `FEYNMAN_MODE=paper` |
| **Backtest** | `SimulatedVenue` | `SimulatedClock` (via `AdvanceClock` RPC) | In-memory | `FEYNMAN_MODE=backtest` |

Same binary, same EngineCore, same risk pipeline. Mode is deployment config.

### Risk Layers

```
Layer 0 — Circuit breakers (compiled-in, not hot-reloadable)
  11 specific breakers: per-order limits, loss halts, rate limits,
  venue errors, dry-run guard, queue backpressure.
  See CORE_ENGINE_DESIGN.md §7.1 (CB-1 through CB-11).

Layer 1 — AgentRiskManager (programmatic, configurable per-agent)
  Universal + signal-specific checks (see §6 below).

Layer 2 — LLM risk agent (future)
Layer 3 — Human CIO override (future)
```

### Startup & Shutdown

**Startup reconciliation** (§9.2.1): 5-step blocking sequence before accepting commands — restore snapshot → connect venues → reconcile (positions, orders, fills, balances) → snapshot → accept. Health returns `starting` during this phase.

**Graceful shutdown** (§8.7): 8-step ordered — stop ingress → drain sequencer → cancel/leave orders → drain fills → flush journal → snapshot → close connections → exit. Three cancel policies: `LeaveOpen` (restarts), `CancelAll` (emergency), `CancelByAgent` (selective).

### Batch Semantics

`atomic=true` only allowed for **same-venue** batches. Cross-venue batches use best-effort (`atomic=false`). No automatic compensating cancels — strategy handles partial execution. See `CORE_ENGINE_DESIGN.md` §8.8.

### Authentication Boundary

Phase 0-1: shared API token. Phase 2+: per-agent tokens with `AgentPermissions`. Phase 3+: mTLS. Auth checked in gRPC interceptor, never in hot path. See `CORE_ENGINE_DESIGN.md` §14.8.

### Config Hot-Reload

**Hot-reloadable (via RPC):** Agent risk limits, instrument limits, pause/resume, fee tiers.
**Requires restart:** Circuit breaker thresholds, venue adapters, channel capacities, `FEYNMAN_MODE`, auth tokens, journal backend. Restart friction is intentional — forces review before financial harm.

---

## 3. Non-Negotiable Invariants

These rules are never violated. Period.

### 3.1 Financial Math — Decimal Only

All prices, quantities, PnL, fees use `rust_decimal::Decimal`. No `f64`, `f32`, or integer division for money.

```rust
// ✓ Correct
let notional = qty * price;  // both Decimal

// ✗ BANNED
let notional = qty as f64 * price as f64;
```

**Failure mode:** Silent rounding → hidden losses or over-leverage.

### 3.2 Fail-Fast on Invalid Input

Invalid input → `Err(...)` immediately. No silent clamping, no defaulting, no `unwrap_or_default()`.

```rust
// ✓ Correct
anyhow::ensure!(conviction >= Decimal::ZERO && conviction <= Decimal::ONE,
    "conviction must be 0.0–1.0, got {}", conviction);

// ✗ BANNED
let conviction = conviction.clamp(Decimal::ZERO, Decimal::ONE);
```

**Failure mode:** Bad signals accepted without warning → trades based on corrupted data.

### 3.3 Pipeline Integrity

The order pipeline has three gates:

```
Strategy (signal) → Risk (approve/kill) → Executor (submit)
```

Never collapse this. No component can submit orders without risk approval. `execute_order` / venue submission only after `RiskApproval` from `AgentRiskManager`.

### 3.4 dryRun Guard (Default: True)

All venue API calls check `config.dry_run` before submission. `dry_run` defaults to `true`. Real orders require explicit override.

```rust
if self.config.dry_run {
    info!("dry_run: skipping submission for {}", order.id);
    return Ok(DryRunAck { .. });
}
self.venue_adapter.submit(order).await?;
```

**Failure mode:** Accidental real orders during testing.

### 3.5 Idempotent Submission

Retried submissions must not create duplicate orders. Use `client_order_id` as idempotency key. Check existing state before creating.

**Failure mode:** Network retry → duplicate orders → unintended large positions.

### 3.6 Per-Agent Budget Isolation

One agent's loss cannot reduce another agent's allocation. Risk checks enforce per-agent limits, not just firm-level.

```rust
// ✓ Correct — per-agent check
let agent_alloc = self.allocations.get(&order.agent)?;
anyhow::ensure!(order.notional <= agent_alloc.free_capital,
    "order exceeds agent {} free capital", order.agent);

// ✗ BANNED — firm-level only
anyhow::ensure!(order.notional <= self.firm_book.free_capital, "exceeds firm capital");
```

### 3.7 No State Mutation Before Fill Confirmation

Portfolio state updates only after exchange confirms the fill. Two-phase: submit → wait for fill → update state.

**Failure mode:** Premature update → mismatch between local portfolio and exchange.

### 3.8 Error Propagation (No Silent Failures)

Venue errors are returned to the caller. Never log-and-continue. Never swallow with `Ok(())`.

```rust
// ✓ Correct
venue_adapter.submit_order(order).await?;

// ✗ BANNED
if let Err(e) = venue_adapter.submit_order(order).await {
    error!("submission failed: {:?}", e);
    // caller thinks it succeeded!
}
```

### 3.9 Explicit Over Implicit

No silent defaults, fallback chains, or hidden behavior. Every code path must be explicit and traceable.

```rust
// ✗ BANNED — silent default
let timeout = config.timeout.unwrap_or(Duration::from_secs(30));

// ✓ Correct — fail if not configured
let timeout = config.timeout
    .ok_or_else(|| anyhow::anyhow!("timeout not configured"))?;

// ✗ BANNED — silent emulation
if !venue.supports_trailing_stop() {
    self.emulate_trailing_stop(order).await?;
}

// ✓ Correct — require opt-in
if !venue.supports_trailing_stop() && !order.exec_hint.allow_emulation {
    anyhow::bail!("venue {} does not support trailing stops", venue.id());
}
```

### 3.10 Sequencer Owns State

All mutable business state (FirmBook, positions, risk limits, orders) is owned by the Sequencer task. No `Arc<Mutex<T>>` or `Arc<RwLock<T>>` on business state. Communication via bounded channels only.

```rust
// ✗ BANNED
let firm_book = Arc::new(RwLock::new(FirmBook::default()));

// ✓ Correct — request snapshot via channel
let (tx, rx) = oneshot::channel();
cmd_tx.send(SequencerCommand::Snapshot { respond: tx }).await?;
let snapshot = rx.await?;
```

### 3.11 Bounded Channels Only

No `mpsc::unbounded_channel()` in production code. All channels have explicit capacity.

**Failure mode:** Unbounded queue grows silently until OOM.

### 3.12 Clock Injection — No Direct `Utc::now()` in Core Path

All timestamps in the Sequencer, EngineCore, RiskGate, and CircuitBreaker come from `Clock::now()`, not `Utc::now()`. The `Clock` trait is injected at construction time.

```rust
// ✓ Correct — timestamp from injected clock
fn evaluate_order(&mut self, order: &PipelineOrder<Validated>, now: DateTime<Utc>) -> Result<PipelineOrder<RiskChecked>>;

// ✗ BANNED in core path — breaks backtest/replay determinism
fn evaluate_order(&mut self, order: &PipelineOrder<Validated>) -> Result<PipelineOrder<RiskChecked>> {
    let now = Utc::now(); // wrong
}
```

**Failure mode:** Backtest produces different results on different runs; replay timestamps don't match journal.

`WallClock` is used for live and paper mode. `SimulatedClock` is used for backtest (advanced by harness via `AdvanceClock` RPC) and replay (advanced by journal timestamps). Same engine code, different clock instance.

---

## 4. Coding Guidelines

### Error Handling

**Library crates** (`risk`, `bus`, `gateway`, `engine-core`, `observability`, `api`): define a crate-specific error enum with `thiserror`. This gives structured, matchable errors to callers.

```rust
// In crates/risk/src/lib.rs — thiserror for library errors
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error("position {notional} exceeds 5% NAV limit {limit}")]
    PositionLimitExceeded { notional: Decimal, limit: Decimal },
    #[error("agent {agent} has no active allocation")]
    UnknownAgent { agent: AgentId },
    #[error("circuit breaker {breaker} is tripped: {reason}")]
    CircuitBreakerTripped { breaker: String, reason: String },
}
```

**Binary and application code** (`bins/feynman-engine`, integration tests): use `anyhow::Result<T>`, `bail!`, `ensure!`. Never `Box<dyn Error>`.

```rust
// In risk-touching code — guard conditions
anyhow::ensure!(
    conviction >= Decimal::ZERO && conviction <= Decimal::ONE,
    "conviction must be 0.0–1.0, got {}", conviction
);
```

- Never `unwrap()` in non-test code without a doc comment explaining why it is safe
- Prefer exhaustive `match` over `if let` with silent else
- `#[must_use]` on every function returning `RiskApproval`, `OrderAck`, or `Result`

```rust
#[must_use]
pub fn evaluate(&self, order: &PipelineOrder<Validated>) -> Result<RiskApproval, RiskError> { … }
```

### Code Style

- Edition 2021, stable Rust (1.82+)
- `cargo fmt` enforced in CI
- `cargo clippy --workspace -- -D warnings` enforced in CI
- Async: tokio with `#[tokio::main]` / `#[tokio::test]`
- Clippy: enable `clippy::pedantic` for all financial crates. Add to `Cargo.toml`:

```toml
[lints.clippy]
pedantic = "warn"
# silence noisy pedantic lints that don't apply here:
module_name_repetitions = "allow"
```

### Newtype Pattern

Use newtypes everywhere an identifier could be confused with another:

```rust
// ✓ Correct — distinct types prevent mixing OrderId with AgentId
pub struct OrderId(pub String);
pub struct AgentId(pub String);

// Always derive these so newtypes work ergonomically:
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub String);

// Implement Display for human-readable output
impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
```

Never use raw `String` where a domain type exists. No `String` parameters that accept order IDs, agent IDs, or venue IDs.

### Type-State Pipeline + Runtime Venue FSM

The order lifecycle uses a **hybrid design** (see DATA_MODEL.md §3 for full specification):

1. **Type-state pipeline** (pre-submission) — compile-time safety where it matters most
2. **Runtime enum FSM** (post-submission) — flexibility for event-driven venue lifecycle

```rust
// ── Pipeline: type-state markers (zero-cost, sealed) ──
pub struct Draft;        // just created
pub struct Validated;    // passed stateless validation
pub struct RiskChecked;  // passed risk gate, carries RiskProof
pub struct Routed;       // venue selected, ready to submit

pub struct PipelineOrder<S: PipelineStage> {
    pub id: OrderId,
    pub agent: AgentId,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    // ... (see DATA_MODEL.md §3.1)
    _stage: PhantomData<S>,
}

// Each transition consumes self → invalid transitions don't compile
impl PipelineOrder<Draft> {
    pub fn validate(self, caps: &VenueCapabilities) -> Result<PipelineOrder<Validated>> { … }
}
impl PipelineOrder<Validated> {
    pub fn risk_check(self, ...) -> RiskOutcome { … }
    // Returns: Approved(PipelineOrder<RiskChecked>), Resize(PipelineOrder<Draft>), or Rejected
}
impl PipelineOrder<RiskChecked> {
    pub fn route(self, ...) -> Result<PipelineOrder<Routed>> { … }
}
impl PipelineOrder<Routed> {
    /// Boundary: consumes pipeline order, produces LiveOrder with runtime FSM.
    pub async fn submit(self, adapter: &dyn VenueAdapter) -> Result<LiveOrder> { … }
}

// ── Venue lifecycle: runtime FSM (event-driven, exhaustive match) ──
pub struct LiveOrder {
    pub id: OrderId,
    pub venue_order_id: VenueOrderId,
    pub state: VenueState,       // runtime enum
    pub fills: Vec<Fill>,
    pub risk_proof: RiskProof,   // proof carried from pipeline
    // ...
}

pub enum VenueState {
    Submitted,
    Accepted { accepted_at: DateTime<Utc> },
    Triggered { triggered_at: DateTime<Utc> },
    PartiallyFilled { summary: FillSummary },
    Filled { summary: FillSummary, filled_at: DateTime<Utc> },
    VenueRejected { reason: String, rejected_at: DateTime<Utc> },
    Cancelled { reason: CancelReason, summary: FillSummary, cancelled_at: DateTime<Utc> },
    Expired { summary: FillSummary, expired_at: DateTime<Utc> },
}

// Transitions validated at runtime — no wildcard match on VenueState
impl VenueState {
    pub fn on_fill(self, fill: &Fill, original_qty: Decimal) -> Result<Self, InvalidTransition> { … }
    pub fn on_cancel(self, reason: CancelReason, at: DateTime<Utc>) -> Result<Self, InvalidTransition> { … }
    // ... (see DATA_MODEL.md §3.8)
}
```

### Conversion Traits

Use `From`/`TryFrom` for all type conversions. Never standalone `to_foo()` / `from_foo()` functions.

```rust
impl TryFrom<SignalProto> for Signal {
    type Error = anyhow::Error;
    fn try_from(proto: SignalProto) -> Result<Self> {
        Ok(Signal {
            conviction: Decimal::from_str(&proto.conviction)
                .map_err(|e| anyhow::anyhow!("invalid conviction: {e}"))?,
            // …
        })
    }
}
```

### Trait Design

**Fine-grained, single-responsibility traits:**

```rust
pub trait RiskEvaluator: Send + Sync {
    fn evaluate(&self, order: &PipelineOrder<Validated>) -> Result<RiskApproval, RiskError>;
}
```

**Static dispatch (`impl Trait`) in hot paths; dynamic dispatch (`dyn Trait`) at wiring boundaries:**

```rust
// ✓ Hot path — zero-cost, monomorphised
fn check_position<R: RiskEvaluator>(evaluator: &R, order: &PipelineOrder<Validated>) { … }

// ✓ Wiring boundary — runtime polymorphism acceptable
pub struct Sequencer {
    risk: Box<dyn RiskGate>,
    bus: Box<dyn MessageBus>,
}
```

**Sealed traits** for safety-critical interfaces that must not be implemented outside this crate:

```rust
// In crates/risk/src/lib.rs
mod private { pub trait Sealed {} }

/// Safety-critical. External crates cannot implement this.
pub trait CircuitBreaker: private::Sealed + Send + Sync { … }

// Only our implementation gets the Sealed bound
impl private::Sealed for HardcodedCircuitBreaker {}
```

### Module Privacy

- `pub` only for items that are part of the crate's contract
- `pub(crate)` for items shared across modules within a crate
- `pub(super)` for items only needed in the parent module
- No `pub` on internal helpers

```rust
pub struct RiskGateImpl { … }          // public: other crates use this
pub(crate) fn compute_max_loss(…) {}   // internal helper
fn validate_limits_internal(…) {}      // module-private
```

### Builder Pattern for Complex Configs

Any struct with more than 3 optional fields uses a builder:

```rust
#[derive(Debug, Default)]
pub struct RiskLimitsBuilder {
    max_gross_notional: Option<Decimal>,
    max_drawdown_pct: Option<Decimal>,
    per_agent: HashMap<AgentId, AgentRiskLimits>,
}

impl RiskLimitsBuilder {
    pub fn max_gross_notional(mut self, v: Decimal) -> Self { self.max_gross_notional = Some(v); self }
    pub fn build(self) -> Result<RiskLimits> {
        Ok(RiskLimits {
            max_gross_notional: self.max_gross_notional
                .ok_or_else(|| anyhow::anyhow!("max_gross_notional is required"))?,
            // …
        })
    }
}
```

### Avoid Clone in Hot Paths

The Sequencer processes orders synchronously. No `.clone()` inside the main evaluation loop.

```rust
// ✓ Borrow, don't clone
fn evaluate(&self, order: &PipelineOrder<Validated>) -> Result<RiskApproval, RiskError> { … }

// ✗ Avoid in hot path — allocates
fn evaluate(&self, order: PipelineOrder<Validated>) -> Result<RiskApproval, RiskError> { … }
```

For data that must be shared across async tasks (fills, snapshots), clone once at the boundary and move ownership.

### Imports

```rust
// Standard library
use std::collections::HashMap;

// Third-party crates
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::mpsc;

// Local crates (use crate name, not `crate::`)
use types::{AgentId, OrderId, Signal};
use risk::{RiskGate, RiskApproval};
```

### Testing

- Unit tests in the same file (`#[cfg(test)]` module)
- Integration tests in `tests/integration/`
- Property tests with `proptest`, run via `cargo test -- --ignored`
- No production code in `#[cfg(test)]` — test helpers go in `#[cfg(any(test, feature = "test-utils"))]`

---

## 5. Common Mistakes

| Mistake | Fix |
|---------|-----|
| `f64` for prices/quantities | `Decimal` from `rust_decimal` |
| `unwrap()` on fallible code | `?` operator or `anyhow::bail!` |
| `unwrap_or_default()` in financial logic | `ok_or_else` with explicit error |
| Ignoring partial fills | Track fill attribution in `TrackedPosition.fill_ids` |
| Swallowing venue errors | Propagate with `?` |
| `Arc<RwLock<T>>` on business state | Sequencer owns state; communicate via channels |
| `mpsc::unbounded_channel()` | Bounded with explicit capacity |
| Silent fallback on error | Return `Err`, let caller decide |
| `if let` with silent else | Exhaustive `match` |
| `.await` while holding mutable state | Sequencer processes sync; async only for channel I/O |
| Blocking async runtime | `tokio::task::spawn_blocking` for CPU-bound work |
| Circular crate dependencies | Check dependency graph before adding imports |

---

## 6. Risk Checks

The risk gate evaluates differently depending on the ingress path. Universal checks apply to all order paths. Signal-specific checks apply only to `SubmitSignal` (LLM agents), where the engine owns sizing and the agent doesn't manage its own exits.

### Universal checks (all paths: `SubmitSignal`, `SubmitOrder`, `SubmitBatch`)

1. **Position <= 5% NAV?** — `notional <= 0.05 * firm_book.total_nav`
2. **Account risk <= 1% NAV?** — `max_loss <= 0.01 * firm_book.total_nav`. If `stop_loss` is present: `max_loss = qty * |entry - stop_loss|`. If absent: `max_loss = notional` (worst case).
3. **Leverage within limits?** — `gross_notional / free_capital <= max_leverage`
4. **Drawdown < 15%?** — `(realized + unrealized) / allocated >= -0.15`
5. **Cash >= 20% NAV?** — `free_capital / total_nav >= 0.20`

### Signal-specific checks (`SubmitSignal` only)

6. **Stop loss defined?** — Signal must have `stop_loss` field. Required because the engine sizes the position and needs a defined exit to calculate risk.
7. **Risk/reward >= 2:1?** — `(tp - entry) / (entry - sl) >= 2`. Only evaluated when both `stop_loss` and `take_profit` are present.

### Always enforced (all paths, not numbered — these are structural)

- Per-agent budget isolation (agent's order cannot exceed agent's free capital)
- Per-instrument concentration limits
- Per-venue exposure limits
- Agent allowed instruments/venues whitelist
- Circuit breakers (Layer 0, before risk gate)

---

## 7. gRPC API (Port 50051)

Full definition: `proto/feynman/engine/v1/service.proto`

| Category | RPCs |
|----------|------|
| **Signal ingress** | `SubmitSignal` |
| **Order ingress** | `SubmitOrder`, `SubmitBatch` (`atomic=true` same-venue only) |
| **Order management** | `CancelOrder`, `AmendOrder`, `GetOrder`, `ListOpenOrders` |
| **History** | `GetFillHistory`, `GetOrderHistory` (reconnect and audit) |
| **Portfolio** | `GetFirmBook`, `GetAgentPositions`, `GetAgentAllocation` |
| **Risk** | `AdjustAgentLimits`, `PauseAgent`, `ResumeAgent`, `HaltAll`, `GetRiskSnapshot` |
| **Agent registration** | `RegisterAgent` |
| **Streams** | `SubscribeFills` (with `since_fill_seq` for lossless reconnect), `SubscribeEvents` |
| **Backtest control** | `AdvanceClock`, `GetEngineMode` |
| **System** | `GetVenueStatus`, `GetEngineHealth` |

**Fill reconnect protocol:** Fills carry a monotonic `fill_seq`. On reconnect, pass `since_fill_seq=N` to `SubscribeFills` — engine replays all fills with `seq > N` before switching to live stream.

---

## 8. Pre-Merge Checklist (Risk/Execution Code)

Before any change to risk/execution code is merged:

- [ ] All universal risk checks pass for orders; signal-specific checks pass for signals
- [ ] Property tests pass (no panic on valid inputs)
- [ ] No `unwrap()` or `panic!()` without doc comment
- [ ] No `f64` in financial math
- [ ] `dry_run` guard checked before each venue call
- [ ] Taleb gate still exclusive
- [ ] Errors propagated (not swallowed)
- [ ] Per-agent budget isolation verified
- [ ] Partial fills tracked (not assumed full)
- [ ] No silent fallbacks or hidden defaults
- [ ] No `Arc<Mutex<T>>` or `Arc<RwLock<T>>` on business state
- [ ] No `mpsc::unbounded_channel()`
- [ ] No `unwrap_or_default()` in financial logic
- [ ] New channels use bounded capacity with explicit size
- [ ] State mutations run inside Sequencer (not gRPC handler or adapter)
- [ ] Event schema changes bump `schema_version` if modifying existing variants
- [ ] `/hostile-review` passed

```bash
make check
```

---

## 9. Shadow Mode (Phase 2)

Rust engine runs alongside Node.js executor without executing orders (`dry_run=true` always). Signal splitter forwards signals to both systems. Divergence comparator checks: risk gate agreement, approval rate, sizing, routing, state, latency, error rate. Exit criteria (all true for 7 consecutive days) before cutover. See `CORE_ENGINE_DESIGN.md` §14.7.

---

## 10. Alerting (14 Triggers)

14 specific alerts (A-1 through A-14) at Critical/Emergency/Warning severity. Key alerts required before Phase 1 live trading: reconciliation divergence (A-1), venue disconnect (A-5), HaltAll triggered (A-10), daily P&L threshold (A-12). Full list in `CORE_ENGINE_DESIGN.md` §13.6.

---

## 11. Key Documents

| Document | What |
|----------|------|
| `docs/CORE_ENGINE_DESIGN.md` | **Canonical** — full architecture, types, venues, pipeline, risk, concurrency |
| `docs/STRATEGY_ENGINE_BOUNDARY.md` | Strategy ↔ engine contract, client types, deployment modes |
| `proto/feynman/engine/v1/service.proto` | gRPC API definition |
| `.claude/rules/risk-touching.md` | Auto-loaded gate for risk/execution code edits |
| `PHASE_0_CHECKLIST.md` | Current sprint tasks and exit criteria |
| `DEVELOPMENT.md` | Setup, testing, debugging, profiling |

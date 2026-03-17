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
agent-risk, agent-bus (depend only on types)
  ↓
bridge, pipeline (depend on types + risk/bus)
  ↓
dashboard, api (depend on pipeline)
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
  → Risk (AgentRiskManager — 7-point gate)
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

See `docs/CORE_ENGINE_DESIGN.md` §15 for full concurrency architecture.

### Execution Modes

| Mode | Venues | Clock | State | Config |
|------|--------|-------|-------|--------|
| **Live** | Real adapters | Wall clock | Persisted | `FEYNMAN_MODE=live` |
| **Paper** | Real data, simulated fills | Wall clock | Persisted | `FEYNMAN_MODE=paper` |
| **Backtest** | `SimulatedVenue` | `SimulatedClock` (via `AdvanceClock` RPC) | In-memory | `FEYNMAN_MODE=backtest` |

Same binary, same EngineCore, same risk pipeline. Mode is deployment config.

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

---

## 4. Coding Guidelines

### Error Handling

- Return `anyhow::Result<T>` from all fallible functions
- Use `anyhow::bail!()` / `anyhow::ensure!()` for guard conditions
- Never `unwrap()` in non-test code without a doc comment explaining why it's safe
- Prefer exhaustive `match` over `if let` with silent else

### Code Style

- Edition 2021, stable Rust (1.82+)
- `cargo fmt` enforced in CI
- `cargo clippy -- -D warnings` enforced in CI
- Async: tokio with `#[tokio::main]` / `#[tokio::test]`

### Imports

```rust
// Standard library
use std::collections::HashMap;
use std::sync::Arc;

// Third-party crates
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::mpsc;

// Local crates
use feynman_types::{Order, AgentAllocation, Signal};
use crate::risk_manager::AgentRiskManager;
```

### Trait Design

Fine-grained traits for capabilities:

```rust
pub trait RiskEvaluator: Send + Sync {
    fn evaluate(&self, order: &Order) -> Result<RiskApproval>;
}

pub trait MessageBus: Send + Sync {
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId>;
    async fn subscribe(&self, topic: &str) -> Result<Receiver<Message>>;
}
```

### Testing

- Unit tests in the same file (`#[cfg(test)]` module)
- Integration tests in `tests/integration/`
- Property tests marked with `#[ignore]` (run via `cargo test -- --ignored`)

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

## 6. Risk Checks (MVP 7-Point)

1. **Stop loss defined?** — Signal must have `stop_loss` field
2. **Risk/reward >= 2:1?** — `(tp - entry) / (entry - sl) >= 2`
3. **Position <= 5% NAV?** — `notional <= 0.05 * firm_book.total_nav`
4. **Account risk <= 1% NAV?** — `max_loss <= 0.01 * firm_book.total_nav`
5. **Leverage within limits?** — `gross_notional / free_capital <= max_leverage`
6. **Drawdown < 15%?** — `(realized + unrealized) / allocated >= -0.15`
7. **Cash >= 20% NAV?** — `free_capital / total_nav >= 0.20`

Plus per-agent, per-instrument, per-venue isolation.

---

## 7. gRPC API (Port 50051)

Full definition: `proto/feynman/engine/v1/service.proto`

| Category | RPCs |
|----------|------|
| **Signal ingress** | `SubmitSignal` |
| **Order ingress** | `SubmitOrder`, `SubmitBatch` |
| **Order management** | `CancelOrder`, `AmendOrder`, `GetOrder`, `ListOpenOrders` |
| **Portfolio** | `GetFirmBook`, `GetAgentPositions`, `GetAgentAllocation` |
| **Risk** | `AdjustAgentLimits`, `PauseAgent`, `ResumeAgent`, `HaltAll`, `GetRiskSnapshot` |
| **Agent registration** | `RegisterAgent` |
| **Streams** | `SubscribeFills`, `SubscribeEvents` |
| **Backtest control** | `AdvanceClock`, `GetEngineMode` |
| **System** | `GetVenueStatus`, `GetEngineHealth` |

---

## 8. Pre-Merge Checklist

Before any change to risk/execution code is merged:

- [ ] All 7 MVP risk checks pass for orders
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
- [ ] `/hostile-review` passed

```bash
make check
```

---

## 9. Key Documents

| Document | What |
|----------|------|
| `docs/CORE_ENGINE_DESIGN.md` | **Canonical** — full architecture, types, venues, pipeline, risk, concurrency |
| `docs/STRATEGY_ENGINE_BOUNDARY.md` | Strategy ↔ engine contract, client types, deployment modes |
| `proto/feynman/engine/v1/service.proto` | gRPC API definition |
| `.claude/rules/risk-touching.md` | Auto-loaded gate for risk/execution code edits |
| `PHASE_0_CHECKLIST.md` | Current sprint tasks and exit criteria |
| `DEVELOPMENT.md` | Setup, testing, debugging, profiling |

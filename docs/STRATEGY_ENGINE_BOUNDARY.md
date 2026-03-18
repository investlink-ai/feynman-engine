# Strategy Engine ↔ Execution Engine — Boundary Contract

**Version:** 0.1.0-draft
**Status:** Design sketch — boundary contract only (strategy internals out of scope)
**Scope:** How the strategy layer talks to the execution engine. What each side owns.

---

> This document defines the **contract between the strategy layer and the execution engine**.
> It does not design the strategy engine's internals (scheduling, data pipelines, model hosting).
> Its purpose is to ensure the execution engine API is future-proof for all strategy client types.
>
> **Companion docs:** [CORE_ENGINE_DESIGN.md](./CORE_ENGINE_DESIGN.md) (execution engine),
> [service.proto](../proto/feynman/engine/v1/service.proto) (gRPC API).

---

## 1. Ownership Boundary

```
┌─────────────────────────────────────┐         ┌──────────────────────────────────┐
│         STRATEGY LAYER              │  gRPC   │       EXECUTION ENGINE           │
│                                     │◄───────►│                                  │
│  Owns:                              │         │  Owns:                           │
│    - Signal generation (alpha)      │         │    - Risk evaluation             │
│    - When to trade                  │         │    - Position sizing (from hint) │
│    - Which instruments to watch     │         │    - Order lifecycle (FSM)       │
│    - Market data interpretation     │         │    - Venue routing & submission  │
│    - Strategy-level position mgmt   │         │    - Fill tracking               │
│    - Backtest orchestration (clock) │         │    - Portfolio state (FirmBook)  │
│    - Performance analytics          │         │    - Per-agent budget isolation  │
│                                     │         │    - Journal & crash recovery    │
│  Does NOT own:                      │         │                                  │
│    - Risk enforcement               │         │  Does NOT own:                   │
│    - Venue connectivity             │         │    - Signal generation           │
│    - Order execution                │         │    - Strategy scheduling         │
│    - Portfolio state of truth       │         │    - Market data distribution    │
│    - Fill confirmation              │         │    - Backtest clock/replay       │
└─────────────────────────────────────┘         └──────────────────────────────────┘
```

**The gRPC boundary is non-negotiable.** No strategy imports engine crates directly.
This ensures:
- Risk gate cannot be bypassed (same reason Taleb gate exists)
- Engine crash doesn't kill strategies; strategy crash doesn't corrupt engine state
- Mode switching (live/paper/backtest) is deployment config, not code change
- Strategies can be written in any language (Rust, Python, TypeScript)

---

## 2. Client Types

The engine serves multiple strategy client types through the same API.
The client type determines which RPCs are typical, not which are allowed.

| Client Type | Registration | Primary RPC | Sizing | Risk | Examples |
|-------------|-------------|-------------|--------|------|----------|
| `llm_agent` | `RegisterAgent` | `SubmitSignal` | Engine sizes (conviction → qty) | Full pipeline | Satoshi, OpenClaw |
| `algo_strategy` | `RegisterAgent` | `SubmitOrder` / `SubmitBatch` | Strategy sizes (sends exact qty) | Risk gate only | Mean-reversion bot, arb strategy |
| `ml_model` | `RegisterAgent` | `SubmitBatch` | Strategy sizes | Risk gate only | RL agent, portfolio optimizer |
| `backtest_harness` | `RegisterAgent` | `SubmitOrder` / `SubmitBatch` | Strategy sizes | Risk gate only | HFTBacktest runner, custom replay |

All client types:
- Must register via `RegisterAgent` before submitting orders
- Are subject to per-agent risk limits (no exceptions)
- Receive fills via `SubscribeFills`
- Can query portfolio state via `GetAgentPositions`, `GetAgentAllocation`

---

## 3. Interaction Patterns

### 3.1 Signal-Based (LLM Agents)

The strategy has a conviction but doesn't know position sizing or venue details.
The engine does sizing, risk, and execution.

```
Strategy                          Engine
   │                                │
   ├─ RegisterAgent(llm_agent) ───► │
   │                                ├─ allocate budget, set limits
   │ ◄── StatusAck ────────────────┤
   │                                │
   ├─ SubmitSignal(conviction=0.8)─►│
   │                                ├─ size: conviction → notional → qty
   │                                ├─ risk: check all 7 gates
   │                                ├─ execute: route to venue
   │ ◄── SignalAck(accepted) ──────┤
   │                                │
   │ ◄── Fill(partial, 60%) ───────┤  (via SubscribeFills stream)
   │ ◄── Fill(complete, 40%) ──────┤
   │                                │
   ├─ GetAgentPositions ──────────► │
   │ ◄── [position snapshot] ──────┤
```

### 3.2 Order-Based (Algo/ML Strategies)

The strategy knows exactly what it wants. The engine does risk check and execution only.

```
Strategy                          Engine
   │                                │
   ├─ RegisterAgent(algo_strategy)─►│
   │ ◄── StatusAck ────────────────┤
   │                                │
   ├─ SubmitOrder(                  │
   │    symbol=BTCUSDT,             │
   │    side=Buy,                   │
   │    qty=0.5,                    │
   │    price=67000,                │
   │    order_type=Limit) ────────► │
   │                                ├─ risk: check limits (no sizing)
   │                                ├─ execute: route to venue
   │ ◄── OrderAck(accepted) ───────┤
   │                                │
   │ ◄── Fill ─────────────────────┤
```

### 3.3 Batch (Portfolio Rebalance / ML)

All-or-nothing risk evaluation. Execution is best-effort (see constraints below).

```
Strategy                          Engine
   │                                │
   ├─ SubmitBatch(atomic=false,     │
   │    orders=[                    │
   │      Buy 0.5 BTC @ 67000,     │  (Bybit)
   │      Sell 10 ETH @ 3800,      │  (Bybit)
   │      Buy 100 SOL @ 170        │  (Hyperliquid)
   │    ]) ─────────────────────► │
   │                                ├─ risk: check ALL orders as a unit
   │                                │  (net exposure, correlation, budget)
   │                                ├─ execute: route each to venue independently
   │ ◄── BatchAck(partial) ────────┤  (SOL rejected by Hyperliquid, others ok)
   │                                │
   │ ◄── Fill (BTC) ───────────────┤
   │ ◄── Fill (ETH) ───────────────┤
```

**Batch constraints:**

- `atomic=true` is **only allowed for same-venue batches**. Cross-venue atomic execution
  is a distributed transaction without rollback — a filled order cannot be unfilled.
  The engine rejects cross-venue `atomic=true` batches with `INVALID_ARGUMENT`.
- `atomic=false` (default) dispatches each order independently. Per-order status in `BatchAck`.
- In both modes, risk evaluation is all-or-nothing: if the batch as a unit would breach
  any limit, the entire batch is rejected.
- If a leg fails execution (venue error), other legs are NOT auto-cancelled. The strategy
  must handle partial execution (send compensating cancels if needed).

### 3.4 Backtest Harness (HFTBacktest or Custom)

The backtest harness drives the clock externally. The engine runs in backtest mode
(simulated venues, simulated clock). The harness submits orders at the pace it controls.

```
HFTBacktest Strategy fn            Engine (FEYNMAN_MODE=backtest)
   │                                │
   ├─ RegisterAgent(backtest) ────► │
   │                                │
   │  ┌─ hbt.elapse(100ms) ──┐     │
   │  │  read depth           │     │
   │  │  generate signal      │     │
   │  └──────────────────────┘     │
   │                                │
   ├─ SubmitOrder(Buy BTC) ──────► │
   │                                ├─ risk check (same logic as live)
   │                                ├─ SimulatedVenue processes
   │ ◄── OrderAck ─────────────────┤
   │                                │
   │  ┌─ hbt.elapse(50ms) ───┐     │
   │  │  wait for fill        │     │
   │  └──────────────────────┘     │
   │                                │
   ├─ poll SubscribeFills ────────► │
   │ ◄── Fill ─────────────────────┤
   │                                │
   │  (repeat until backtest done)  │
```

**Clock synchronization in backtest:**

The backtest harness advances time. The engine in backtest mode accepts a clock-advance
command so its SimulatedClock stays in sync with the harness.

```protobuf
// Only valid when FEYNMAN_MODE=backtest. Rejected in live/paper.
rpc AdvanceClock(ClockAdvance) returns (StatusAck);

message ClockAdvance {
  google.protobuf.Timestamp advance_to = 1;
}
```

This keeps the engine's internal clock (used for time-in-force expiry, funding rate
calculations, risk window checks) synchronized with the harness's simulated time.

---

## 4. What the Engine API Must Guarantee (for any strategy client)

These are the API-level contracts that make the engine future-proof for strategy clients:

| # | Guarantee | Why |
|---|-----------|-----|
| 1 | **Idempotent submission** — same `client_order_id` returns same `OrderAck`, no duplicate order | Strategies retry on timeout. Engine must be safe to retry. |
| 2 | **Risk enforcement on every path** — no RPC bypasses the risk gate | Strategy bugs don't create uncontrolled risk. |
| 3 | **Per-agent isolation** — one agent's orders/fills/positions never leak to another | Multi-strategy safety. Agent A's loss doesn't eat Agent B's budget. |
| 4 | **Fill delivery with reconnect** — fills have monotonic `fill_seq`. `SubscribeFills(since_fill_seq=N)` replays all fills with seq > N before switching to live. Lossless reconnect. | Strategy needs reliable fill tracking. gRPC streams disconnect — reconnect must not lose fills. |
| 5 | **Mode-transparent** — same RPC, same behavior, same response shape in all modes | Strategy code doesn't change between backtest and live. |
| 6 | **Rejection is explicit** — rejected orders return reason, never silent drop | Strategy can act on rejection (adjust, retry, alert). |
| 7 | **Portfolio reads are consistent** — `GetAgentPositions` reflects all fills up to `as_of` timestamp | Strategy decisions based on stale state are dangerous. |
| 8 | **Bounded latency on rejection** — risk rejection returns in <10ms, not after venue timeout | Fast feedback loop for strategy iteration. |

---

## 5. What the Engine Does NOT Provide to Strategies

These are explicitly out of scope for the engine API. The strategy layer must own them.

| Capability | Why not in engine |
|------------|-------------------|
| Market data distribution | Engine receives orders, not data. Strategies get data from venues/feeds directly. |
| Strategy scheduling / cron | Engine is reactive (processes commands). Strategy decides when to act. |
| Signal generation | Engine doesn't know what alpha is. It enforces risk on whatever the strategy sends. |
| Backtest data replay | The harness (HFTBacktest, custom) owns replay. Engine processes the resulting orders. |
| Performance attribution | Strategy tracks its own P&L using fills from engine. Engine tracks firm-level P&L. |
| Model training / inference | ML models run externally. Engine receives their output as `SubmitOrder`/`SubmitBatch`. |

---

## 6. Strategy Engine — High-Level Shape (Internal Design TBD)

This section sketches what the strategy engine probably looks like, without committing
to internals. Enough to validate the boundary contract.

```
┌──────────────────────────────────────────────────────────────────────┐
│                     STRATEGY ENGINE SERVICE                          │
│                                                                      │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────────────┐ │
│  │  DATA LAYER    │  │  STRATEGY MGR  │  │  BACKTEST ORCHESTRATOR │ │
│  │                │  │                │  │                        │ │
│  │  - Venue WS    │  │  - Load/start  │  │  - HFTBacktest runner │ │
│  │    feeds       │  │    strategies  │  │  - Custom replay       │ │
│  │  - Historical  │  │  - Health      │  │  - Clock control       │ │
│  │    data store  │  │    monitoring  │  │  - Result collection   │ │
│  │  - Feature     │  │  - Config hot  │  │  - Comparison          │ │
│  │    pipeline    │  │    reload      │  │    (Mode A vs B)       │ │
│  └───────┬────────┘  └───────┬────────┘  └───────────┬────────────┘ │
│          │                   │                        │              │
│          ▼                   ▼                        ▼              │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │                    STRATEGY INSTANCES                          │  │
│  │                                                                │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐             │  │
│  │  │ Satoshi     │ │ MeanRev Bot │ │ RL Agent    │  ...        │  │
│  │  │ (LLM)      │ │ (algo)      │ │ (ML)        │             │  │
│  │  └──────┬──────┘ └──────┬──────┘ └──────┬──────┘             │  │
│  │         │               │               │                     │  │
│  └─────────┼───────────────┼───────────────┼─────────────────────┘  │
│            │               │               │                        │
└────────────┼───────────────┼───────────────┼────────────────────────┘
             │               │               │
             ▼               ▼               ▼
      ┌─────────────────────────────────────────┐
      │  gRPC: SubmitSignal / SubmitOrder / ... │
      └────────────────────┬────────────────────┘
                           │
                           ▼
      ┌─────────────────────────────────────────┐
      │         EXECUTION ENGINE SERVICE         │
      │     (feynman-engine, mode = env config)  │
      └─────────────────────────────────────────┘
```

### 6.1 Backtest Orchestration

The strategy engine (not the execution engine) owns backtest orchestration:

```
Strategy Engine                    Execution Engine (backtest mode)
   │                                │
   ├─ start engine in backtest mode │
   │  (FEYNMAN_MODE=backtest)       │
   │                                │
   ├─ register agents ─────────────►│
   │                                │
   │  for each time step:           │
   │    ├─ advance data feeds       │
   │    ├─ run strategy logic       │
   │    ├─ SubmitOrder/Signal ─────►│──► risk check ──► SimulatedVenue
   │    ├─ AdvanceClock(t) ────────►│──► advance internal clock
   │    ├─ poll SubscribeFills ────►│──► return simulated fills
   │    └─ update strategy state    │
   │                                │
   ├─ collect results               │
   ├─ compute analytics             │
   └─ done                          │
```

### 6.2 Multi-Environment Deployment

Same code, different environments. Strategy code doesn't know which mode it's in.

```
                    ┌─────────────────────────────────────────┐
                    │         STRATEGY ENGINE                  │
                    │     (same binary in all envs)            │
                    └──────────────┬──────────────────────────┘
                                   │ gRPC
           ┌───────────────────────┼───────────────────────┐
           ▼                       ▼                       ▼
┌─────────────────────┐ ┌─────────────────────┐ ┌─────────────────────┐
│  EXECUTION ENGINE   │ │  EXECUTION ENGINE   │ │  EXECUTION ENGINE   │
│  FEYNMAN_MODE=live  │ │  FEYNMAN_MODE=paper │ │  FEYNMAN_MODE=bt    │
│                     │ │                     │ │                     │
│  Real venues        │ │  Real data,         │ │  Simulated venues   │
│  Real fills         │ │  simulated fills    │ │  Simulated clock    │
│  Persisted state    │ │  Persisted state    │ │  In-memory state    │
│  dry_run=false      │ │  dry_run=true       │ │  dry_run=true       │
└─────────────────────┘ └─────────────────────┘ └─────────────────────┘
```

---

## 7. Engine API Gaps (Identified from Strategy Boundary Analysis)

RPCs already implemented in `service.proto`: `AdvanceClock`, `GetFillHistory`, `GetOrderHistory`, `GetEngineMode`.

**Remaining candidates (Phase 1-2):**

| RPC | Purpose | Needed by |
|-----|---------|-----------|
| `SubscribeMarketState` | Stream engine's view of market state (funding rates, mark prices) | Strategies that need engine's market view for consistency |

Not blocking Phase 0.

---

## 8. What This Means for Engine Design

Decisions the execution engine should lock in now based on this boundary:

1. **Engine binary has a `--mode` flag / `FEYNMAN_MODE` env var.** This switches between Live/Paper/Backtest without code changes. All internal components check this mode.

2. **SimulatedVenue adapter** exists alongside real adapters. In backtest mode, the venue adapter map is populated with simulated venues instead of real ones. Same VenueAdapter trait.

3. **SimulatedClock** is advanceable via `AdvanceClock` RPC. In live/paper mode, this RPC is rejected. The clock trait is already in §10.1 (`fn clock(&self) -> &dyn Clock`).

4. **Per-agent state is complete enough for strategy recovery.** If a strategy restarts mid-session, it can reconstruct its state from `GetAgentPositions` + `GetFillHistory`. The engine is the source of truth for what actually executed.

5. **EngineCore remains internal.** It is the synchronous decision logic inside the Sequencer. Strategies never see it. The gRPC API is the only interface.

6. **No market data distribution from engine.** Strategies get market data from their own feeds. The engine receives orders. This keeps the engine focused and avoids becoming a data platform.

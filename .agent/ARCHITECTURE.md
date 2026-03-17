# Architecture Quick Reference

For detailed architecture, see `docs/HYBRID_ENGINE_ARCHITECTURE.md`.

## System Topology

```
                        ┌─────────────────────────────────┐
                        │   OpenClaw Agents (Node.js)     │
                        │  Satoshi (Trader) / Taleb       │
                        │  (Risk) / Feynman (CIO)        │
                        └──────────────┬──────────────────┘
                                       │ gRPC + JSON-RPC
                                       ▼
                        ┌──────────────────────────────────┐
                        │  feynman-engine (Rust + tonic)   │
                        │  Port: 50051 (gRPC)              │
                        │  Port: 8080 (REST/dashboard)     │
                        └─┬──────────────┬──────────────────┘
                          │              │
                    ┌─────▼───┐    ┌─────▼───────────┐
                    │ Bybit   │    │ Redis Streams   │
                    │ Binance │    │ (Message Bus)   │
                    │ Hyperliq│    │                 │
                    └─────────┘    └─────────────────┘
```

## Crate Dependency Graph

```
types (no dependencies)
  ├─ OrderId, AgentId, VenueId, Signal, Order, etc.
  ├─ FirmBook (portfolio view)
  └─ RiskViolation, AgentStatus enums

agent-risk (depends: types)
  ├─ AgentRiskManager
  ├─ 7-point MVP checklist evaluation
  ├─ Per-agent / per-instrument / per-venue isolation
  └─ Property-based tests

agent-bus (depends: types)
  ├─ MessageBus trait (async publish/subscribe)
  ├─ RedisBus implementation (Redis Streams)
  ├─ Consumer groups + XPENDING reconciliation
  └─ Integration tests

bridge (depends: types)
  ├─ Signal → Order translation
  ├─ Position sizing under conviction
  ├─ Stop-loss / take-profit computation
  └─ Multi-venue order model (Bybit, Binance, Hyperliquid, Polymarket)

pipeline (depends: types, agent-risk, bridge)
  ├─ Grouper (coalesce signals by instrument)
  ├─ Detector (identify trade opportunities)
  ├─ Sizing (position sizing under risk limits)
  ├─ Risk (call AgentRiskManager)
  └─ ExecutionController (order submission)

dashboard (depends: types, pipeline)
  ├─ REST API (Axum)
  ├─ WebSocket SSE (real-time fills, positions)
  ├─ Prometheus metrics
  └─ HTML frontend stub

api (depends: types, pipeline, dashboard)
  ├─ gRPC service (tonic)
  ├─ Proto definitions (proto/feynman/engine/v1/service.proto)
  └─ RPC handlers for order submission, risk snapshot, etc.

feynman-engine binary (depends: all crates)
  ├─ Initialization (config, Redis, Bybit connection)
  ├─ gRPC server startup (port 50051)
  ├─ Event loop (signal → risk → order)
  └─ Shutdown handler
```

## Key Types

### Order Pipeline
```
Signal (from Satoshi)
  → Order (unsigned, pending risk gate)
  → RiskApproval (from Taleb / AgentRiskManager)
  → ExecutedOrder (submitted to venue)
  → Fill (partial fill from Bybit)
  → StrategyPosition (aggregated per agent + instrument)
```

### Portfolio Hierarchy
```
FirmBook (total NAV, free capital, total PnL)
  ├─ AgentAllocation (per-agent capital, drawdown)
  ├─ InstrumentExposure (net qty, gross notional)
  ├─ VenueExposure (notional per exchange)
  └─ PredictionExposure (options on prediction markets)
```

### Risk Checks (MVP 7-point)
1. **Stop loss defined?** — Signal must have `stop_loss` field
2. **Risk/reward >= 2:1?** — (profit_target - entry) / (entry - stop_loss) >= 2
3. **Position <= 5% NAV?** — notional <= 0.05 * firm_book.total_nav
4. **Account risk <= 1% NAV?** — max_loss_position <= 0.01 * firm_book.total_nav
5. **Leverage within limits?** — gross_notional / free_capital <= agent_limits.max_leverage
6. **Drawdown < 15%?** — (total_realized_pnl + total_unrealized_pnl) / allocated_capital >= -0.15
7. **Cash >= 20% NAV?** — free_capital / firm_book.total_nav >= 0.20

Plus per-agent, per-instrument, per-venue isolation.

## Message Bus (Redis Streams)

Topics:
- `signals:{agent_id}` — signals published by trader agents
- `fills:{order_id}` — fills from Bybit
- `events:risk` — risk violations and alerts
- `events:engine` — lifecycle events (startup, shutdown, errors)

Consumer Groups:
- `execution_service` — service group for Executor
- `analytics` — group for logging/metrics

Pattern: XADD (publish) → XREAD (subscribe) → XACK (acknowledge)

## gRPC API (Port 50051)

**Order Submission:**
```
rpc SubmitSignal(Signal) returns (SignalAck)
rpc CancelOrder(CancelRequest) returns (CancelAck)
```

**Portfolio:**
```
rpc GetFirmBook(Empty) returns (FirmBook)
rpc GetAgentPositions(AgentFilter) returns (stream StrategyPosition)
```

**Risk:**
```
rpc AdjustAgentLimits(AgentLimitsUpdate) returns (StatusAck)
rpc GetRiskSnapshot(Empty) returns (RiskSnapshot)
rpc PauseAgent(PauseRequest) returns (StatusAck)
```

**System:**
```
rpc GetEngineHealth(Empty) returns (HealthReport)
rpc GetVenueStatus(Empty) returns (stream VenueStatus)
```

## REST API (Port 8080)

- `GET /health` — engine health
- `GET /portfolio` — firm book
- `GET /positions/{agent_id}` — agent positions
- `GET /risk/snapshot` — current risk state
- `POST /orders` — submit order (calls gRPC internally)
- `WS /events` — WebSocket for real-time fills and events

## Testing Modes

| Mode | Market Data | Fills | Exchange API | Use Case |
|------|-------------|-------|--------------|----------|
| **Backtest** | Historical | Simulated (fill model) | None | Strategy tuning |
| **Paper** | Live | Simulated | Live | Dry run before live |
| **Live** | Live | Real | Real (Bybit) | Production trading |

Controlled via config:
```toml
[engine]
execution_mode = "paper"  # or "backtest", "live"
dry_run = true            # skip actual order submission
```

## Performance Targets

- **Signal ingestion:** <10ms latency (Satoshi → Engine)
- **Risk evaluation:** <50ms (order → approval/rejection)
- **Order submission:** <100ms (approved → Bybit)
- **Fill settlement:** <5s (fill received → position updated)

## Deployment

**Docker Compose:**
```bash
docker compose -f docker/docker-compose.yml up -d
# Starts: engine (Rust) + Redis + Prometheus
# Accessible: gRPC :50051, REST :8080, metrics :9090
```

**Local Development:**
```bash
# Start Redis
docker run -p 6379:6379 redis:7-alpine

# Start engine
cargo run --bin feynman-engine -- --config config/default.toml

# Call gRPC
grpcurl -plaintext localhost:50051 feynman.engine.v1.ExecutionService/GetEngineHealth
```

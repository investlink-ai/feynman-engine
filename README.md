# Feynman Capital — Execution Engine

[![CI](https://github.com/investlink-ai/feynman-engine/actions/workflows/test.yml/badge.svg)](https://github.com/investlink-ai/feynman-engine/actions/workflows/test.yml)

A unified execution engine and message bus for multi-agent autonomous trading on crypto, stocks, options, and prediction markets.

## Quick Start

```bash
# Clone and setup
git clone https://github.com/feynman-capital/feynman-engine.git
cd feynman-engine

# Verify Rust toolchain
rustc --version  # 1.83+
cargo --version

# Build
cargo build --release

# Run tests
cargo test --workspace

# Start (requires Redis and config)
docker compose up -d
# or
./target/release/feynman-engine --config config/default.toml
```

## Project Structure

```
feynman-engine/
├── Cargo.toml                    # Workspace root
├── README.md                     # This file
├── docker/
│   ├── engine.Dockerfile         # Production image (multi-stage)
│   └── docker-compose.yml        # Engine + Redis stack
│
├── config/
│   ├── default.toml              # Default configuration
│   ├── paper.toml                # Paper trading overrides
│   └── backtest.toml             # Backtest overrides
│
├── proto/
│   └── feynman/engine/v1/
│       └── service.proto         # gRPC service definition
│
├── crates/                       # Individual crates (see details below)
│   ├── types/                    # Feynman-specific types
│   ├── gateway/                  # Signal → Order translation
│   ├── risk/                     # Agent risk isolation
│   ├── bus/                      # Redis Streams cross-process bus
│   ├── engine-core/              # Execution pipeline stages
│   ├── observability/            # Web UI + observability
│   └── api/                      # gRPC service
│
├── bins/
│   └── feynman-engine/           # Main binary (all modes)
│
├── tests/
│   ├── integration/              # Multi-crate integration tests
│   ├── backtest/                 # Backtest scenario tests
│   └── risk/                     # Agent risk isolation tests
│
└── .github/workflows/            # CI/CD (GitHub Actions)
    ├── test.yml                  # cargo test, cargo clippy
    ├── build.yml                 # cargo build --release
    └── docker.yml                # Build and push docker image
```

## Crates Overview

| Crate | Purpose |
|-------|---------|
| **types** | Signal, AgentId, AgentAllocation, FirmBook, custom domain types |
| **gateway** | Signal → Order translation, position sizing, venue routing |
| **risk** | AgentRiskManager, per-agent limits, drawdown checks — L1 risk layer |
| **bus** | AgentBus trait + Redis Streams impl — cross-process agent coordination |
| **engine-core** | Execution pipeline stages: validate → risk → size → route → submit |
| **observability** | REST API (axum), SSE stream, Prometheus metrics — observability |
| **api** | gRPC service (tonic) — network boundary for agents |
| **feynman-engine** (binary) | Main entry point, engine orchestration, CLI args |

## Architecture

**100% custom Rust implementation** (no external trading frameworks). See docs for full design:

- **Type-state order pipeline** — Draft → Validated → RiskChecked → Routed → LiveOrder (compile-time safety)
- **Hybrid FSM** — Type-state for engine pipeline (linear, deterministic), runtime VenueState enum for venue lifecycle (event-driven)
- **Per-agent risk isolation** — Budget limits, leverage caps, drawdown halts (L1 risk)
- **Path-aware validation** — Signal-specific checks (stop loss required), order-specific checks (optional stop loss)
- **Redis Streams bus** — Consumer groups, pending message tracking, circuit breaker coordination
- **Three execution modes** — Backtest (historical), Paper (live data + sim), Live (real)
- **Decimal-only math** — No f64 in financial logic (100% Decimal)

## Development Workflow

### Setup

```bash
# Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
rustup update

# Install proto compiler
brew install protobuf  # macOS
# or
apt-get install protobuf-compiler  # Ubuntu

# Start Redis (for development)
docker run -d --name redis -p 6379:6379 redis:7-alpine
```

### Testing

```bash
# All tests
cargo test --workspace

# Specific crate
cargo test -p types

# Integration tests only
cargo test --test '*' --lib

# With output
cargo test -- --nocapture

# Property tests (slower)
cargo test --test '*' -- --ignored
```

### Building

```bash
# Debug build (fast, unoptimized)
cargo build

# Release build (optimized, for production)
cargo build --release

# Check without building
cargo check

# Linting
cargo clippy --workspace -- -D warnings

# Format check
cargo fmt --check
```

### Docker

```bash
# End-to-end smoke check
./scripts/smoke-test.sh

# Manual bring-up
docker compose -f docker/docker-compose.yml up -d

# View logs
docker compose -f docker/docker-compose.yml logs -f engine

# Health check from host
grpc_health_probe -addr=localhost:50051

# Stop
docker compose -f docker/docker-compose.yml down -v

# Backtest
docker compose -f docker/docker-compose.yml -f docker/docker-compose.backtest.yml run backtest
```

## Configuration

See `config/default.toml` for all options. Key sections:

```toml
[engine]
mode = "live"           # "live", "paper", "backtest"
dry_run = true          # ALWAYS true by default
grpc_port = 50051
dashboard_port = 8080

[risk.firm]
max_gross_notional = 200000
max_drawdown_pct = 5.0

[venues.bybit]
testnet = true
api_key = "${BYBIT_API_KEY}"
api_secret = "${BYBIT_API_SECRET}"
```

Environment variable expansion is supported (e.g., `${HOME}`, `${BYBIT_API_KEY}`).
The bootstrap path also honors runtime overrides for `FEYNMAN_MODE`/`ENGINE_MODE`,
`ENGINE_DRY_RUN`, `ENGINE_GRPC_PORT`, and `REDIS_URL`.

## API Reference

### gRPC Service

See `proto/feynman/engine/v1/service.proto` for full definition. Key methods:

```protobuf
service ExecutionService {
    rpc SubmitSignal(Signal) returns (SignalAck);
    rpc CancelOrder(CancelRequest) returns (CancelAck);
    rpc GetFirmBook(Empty) returns (FirmBook);
    rpc AdjustAgentLimits(AgentLimitsUpdate) returns (Ack);
    rpc SubscribeFills(AgentFilter) returns (stream Fill);
}
```

### Dashboard REST API

- `GET /api/firm` — Firm overview (NAV, P&L, exposure)
- `GET /api/agents` — Agent allocations
- `GET /api/positions` — All positions
- `GET /api/venues` — Venue status
- `GET /api/risk` — Risk utilization
- `GET /api/orders` — Order history
- `GET /api/events` — Event stream (SSE)
- `GET /metrics` — Prometheus metrics

### Message Bus (Redis Streams)

Topics:
- `signals` — Agent submissions
- `approved_orders` — Risk-approved orders
- `fills` — Order fills
- `positions` — Position updates
- `risk_events` — Risk violations / alerts
- `funding` — Funding rate payments (perps)
- `system` — Heartbeats, connections

## Deployment

### Production (Docker Compose)

```bash
export BYBIT_API_KEY="..."
export BYBIT_API_SECRET="..."
docker compose up -d
```

### Local Development

```bash
# Terminal 1: Redis
docker run -p 6379:6379 redis:7-alpine

# Terminal 2: Engine (debug build)
cargo run -p feynman-engine -- --config config/default.toml

# Terminal 3: Test agent (gRPC client)
# Use grpcurl, ghz, or your test script
grpcurl -plaintext localhost:50051 feynman.engine.v1.ExecutionService/GetFirmBook

# Terminal 4: Bybit adapter smoke against testnet
export BYBIT_API_KEY="..."
export BYBIT_API_SECRET="..."
./scripts/bybit-testnet-smoke.sh

# Optional: also place a market order and wait for a fill
BYBIT_TESTNET_RUN_MARKET_FILL=1 ./scripts/bybit-testnet-smoke.sh
```

## Troubleshooting

| Issue | Solution |
|-------|----------|
| `redis connection refused` | Start Redis: `docker run -p 6379:6379 redis:7-alpine` |
| `gRPC connection failed` | Check engine is running: `grpc_health_probe -addr=:50051` |
| `Cargo build fails` | Update Rust: `rustup update` |
| `Proto compilation fails` | Ensure protobuf compiler installed: `brew install protobuf` |
| `Docker image build fails` | Check Rust version: `rustc --version` (need 1.82+) |

## Documentation

**Getting Started:**
- [QUICKSTART.md](./QUICKSTART.md) — 10-minute onboarding (setup + dev loop)
- [docs/DEV_WORKFLOW.md](./docs/DEV_WORKFLOW.md) — Full development workflow (issue → branch → verify → PR → merge)
- [docs/DEVELOPMENT.md](./docs/DEVELOPMENT.md) — Developer reference (commands, debugging, Docker, CI/CD)
- [docs/TESTING.md](./docs/TESTING.md) — Testing strategy (compile-fail, snapshots, property tests, fixtures)
- [docs/CODING_GUIDELINES.md](./docs/CODING_GUIDELINES.md) — Language standards, patterns, architecture rules

**Architecture & Design:**
- [docs/CORE_ENGINE_DESIGN.md](./docs/CORE_ENGINE_DESIGN.md) — Order FSM (type-state + runtime), venue adapters, risk layers
- [docs/SYSTEM_ARCHITECTURE.md](./docs/SYSTEM_ARCHITECTURE.md) — High-level architecture diagram, kill switch hierarchy
- [docs/DATA_MODEL.md](./docs/DATA_MODEL.md) — Domain types, order lifecycle, invariants
- [docs/CONTRACTS.md](./docs/CONTRACTS.md) — Trait contracts, RPC signatures, pipeline stages

**Planning:**
- [PHASE_0_CHECKLIST.md](./PHASE_0_CHECKLIST.md) — Current sprint (scaffold, risk, bus, integration tests)
- [ROADMAP.md](./ROADMAP.md) — 5-phase migration plan with dependency graph

## Contributing

1. Create a branch: `git checkout -b feat/your-feature`
2. Make changes and test: `cargo test --workspace`
3. Format: `cargo fmt`
4. Lint: `cargo clippy --workspace -- -D warnings`
5. Push and open PR

## License

UNLICENSED (internal use only)

## Contact

Feynman Capital team

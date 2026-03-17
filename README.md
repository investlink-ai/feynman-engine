# Feynman Capital — Execution Engine

A unified execution engine and message bus for multi-agent autonomous trading on crypto, stocks, options, and prediction markets.

## Quick Start

```bash
# Clone and setup
git clone https://github.com/feynman-capital/feynman-engine.git
cd feynman-engine

# Verify Rust toolchain
rustc --version  # 1.82+
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
│   ├── bridge/                   # Signal → Order translation
│   ├── agent-risk/               # Agent risk isolation
│   ├── agent-bus/                # Redis Streams cross-process bus
│   ├── pipeline/                 # Execution pipeline stages
│   ├── dashboard/                # Web UI + observability
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
| **types** | Signal, AgentId, AgentAllocation, FirmBook, etc. — types that Nautilus doesn't have |
| **bridge** | PortfolioBridge, PositionSizer, VenueRouter — converts signals to Nautilus orders |
| **agent-risk** | AgentRiskManager, per-agent limits, drawdown checks — L1 risk layer |
| **agent-bus** | AgentBus trait + Redis Streams impl — cross-process agent coordination |
| **pipeline** | PipelineStage trait, execution pipeline composition — signal validation → risk → size → submit |
| **dashboard** | REST API (axum), SSE stream, Prometheus metrics — observability |
| **api** | gRPC service (tonic) — network boundary for agents |
| **feynman-engine** (binary) | Main entry point, FeynmanEngine orchestration, CLI args |

## Key Design Decisions

See [HYBRID_ENGINE_ARCHITECTURE.md](#docs--architecture) for full architecture. TL;DR:

- **NautilusTrader core** — Orders, venue adapters, backtest engine, fee/fill models
- **Custom service layer** — gRPC boundary, agent risk isolation, signal→order bridge, Redis bus
- **Hybrid strategy** — ~70% NautilusTrader (proven), ~30% custom (our edge)
- **Three execution modes** — Backtest (historical), Paper (live data + sim), Live (real)
- **Risk layers** — L0 (compiled circuit breakers), L1 (agent budgets), L2 (LLM Taleb), L3 (human)

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
# Development (paper trading)
docker compose up -d

# View logs
docker compose logs -f engine

# Health check
docker compose exec engine grpc_health_probe -addr=:50051

# Stop
docker compose down

# Backtest
docker compose -f docker-compose.yml -f docker-compose.backtest.yml run backtest
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
testnet = false
api_key = "${BYBIT_API_KEY}"
api_secret = "${BYBIT_API_SECRET}"
```

Environment variable expansion is supported (e.g., `${HOME}`, `${BYBIT_API_KEY}`).

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

- [HYBRID_ENGINE_ARCHITECTURE.md](../feynman_trading_bot/docs/HYBRID_ENGINE_ARCHITECTURE.md) — Full architecture (design, buy-vs-build, type mapping)
- [CORE_ENGINE_DESIGN.md](../feynman_trading_bot/docs/CORE_ENGINE_DESIGN.md) — Order model, venue adapters, risk layers
- [SOLUTION-DESIGN.md](../feynman_trading_bot/docs/SOLUTION-DESIGN.md) — Agent architecture (parent repo)
- [MVP.md](../feynman_trading_bot/docs/MVP.md) — MVP scope and gates (parent repo)

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

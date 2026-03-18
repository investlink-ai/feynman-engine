# Development Guide

## Initial Setup

```bash
cd feynman-engine

# Verify Rust toolchain
rustc --version  # 1.82+
cargo --version

# Install protobuf compiler (required for gRPC)
brew install protobuf      # macOS
# or
sudo apt-get install protobuf-compiler  # Ubuntu

# Build the workspace
cargo build

# Run tests
cargo test --workspace

# Check formatting and linting
cargo fmt --check
cargo clippy --workspace -- -D warnings
```

## Project Layout Overview

| Directory | Purpose |
|-----------|---------|
| `crates/types/` | Feynman-specific domain types (Signal, AgentAllocation, etc.) |
| `crates/gateway/` | Signal → Nautilus order translation, sizing logic |
| `crates/risk/` | L1 agent-aware risk evaluation |
| `crates/bus/` | Redis Streams message bus implementation |
| `crates/engine-core/` | Execution pipeline stage composition |
| `crates/observability/` | REST API, SSE, Prometheus metrics |
| `crates/api/` | gRPC service implementation |
| `bins/feynman-engine/` | Main binary entry point |
| `tests/` | Integration, backtest, and risk tests |
| `proto/` | gRPC service definitions (.proto files) |
| `config/` | Configuration file templates |
| `docker/` | Dockerfile and compose files |

## Crate Development Order

Follow this order to maintain dependency hygiene:

1. **types** — No dependencies except std, serde. Define all domain types here.
2. **risk** — Depends on types. Risk evaluation logic.
3. **bus** — Depends on types. Redis client implementation.
4. **gateway** — Depends on types. Translation and sizing.
5. **engine-core** — Depends on types, risk, gateway. Pipeline stages.
6. **observability** — Depends on types, engine-core. Web UI.
7. **api** — Depends on types, engine-core, observability. gRPC service.
8. **feynman-engine** (binary) — Orchestration, all crates.

## Common Tasks

### Testing

```bash
# All tests
cargo test --workspace --verbose

# Specific crate
cargo test -p types

# Single test
cargo test agent_allocation_arithmetic

# Run with output
cargo test -- --nocapture --test-threads=1

# Property tests (slower, marked #[ignore])
cargo test -- --ignored

# Test coverage (requires `cargo-tarpaulin`)
cargo install cargo-tarpaulin
cargo tarpaulin --workspace --out Html
```

### Code Quality

```bash
# Format (auto-fix)
cargo fmt

# Format check only
cargo fmt --check

# Linting
cargo clippy --workspace

# Strict linting (as CI does)
cargo clippy --workspace -- -D warnings

# Unused dependencies
cargo tree --duplicates
```

### Building

```bash
# Debug (fast to build, slow to run)
cargo build

# Release (slow to build, fast to run)
cargo build --release

# Build specific crate
cargo build -p types

# Build binary
cargo build --bin feynman-engine

# Build and run
cargo run --bin feynman-engine -- --config config/default.toml
```

### Documentation

```bash
# Build and open documentation
cargo doc --workspace --open

# Check doc comments compile
cargo test --doc

# Build docs for dependencies too
cargo doc --workspace --no-deps=false --open
```

## Working with gRPC

The gRPC service is defined in `proto/feynman/engine/v1/service.proto`.

### Proto Changes

If you modify `.proto` files:

```bash
# The Rust code is generated automatically by the build script (build.rs)
# Just rebuild:
cargo build

# The generated code appears in target/debug/feynman/engine/v1/ (or release/)
```

### Testing gRPC Locally

```bash
# Terminal 1: Start the engine
cargo run --bin feynman-engine

# Terminal 2: Call a service method
grpcurl -plaintext localhost:50051 \
  feynman.engine.v1.ExecutionService/GetEngineHealth

# Or with ghz (gRPC benchmarking tool)
ghz --insecure --proto proto/feynman/engine/v1/service.proto \
  --call feynman.engine.v1.ExecutionService/GetEngineHealth \
  localhost:50051
```

## Docker Development

```bash
# Build image locally
docker build -f docker/engine.Dockerfile -t feynman-engine:latest .

# Run with docker-compose (includes Redis)
docker compose -f docker/docker-compose.yml up -d

# View logs
docker compose logs -f engine

# Health check
grpc_health_probe -addr=localhost:50051

# Stop
docker compose down

# Clean up volumes
docker compose down -v
```

## Debugging

### Enable Debug Logging

```bash
# Via environment variable
RUST_LOG=debug cargo run --bin feynman-engine

# Different modules
RUST_LOG=feynman_engine=debug,types=trace cargo run --bin feynman-engine
```

### Debugging in VS Code

Create `.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "lldb",
      "request": "launch",
      "name": "Debug feynman-engine",
      "cargo": {
        "args": [
          "build",
          "--bin=feynman-engine",
          "--package=feynman-engine"
        ],
        "filter": {
          "name": "feynman-engine",
          "kind": "bin"
        }
      },
      "args": ["--config", "config/default.toml"],
      "cwd": "${workspaceFolder}"
    }
  ]
}
```

Then press F5 to debug.

## CI/CD

GitHub Actions workflows are in `.github/workflows/`:

- `test.yml` — `cargo test`, `cargo clippy`, `cargo fmt --check`
- `build.yml` — `cargo build --release`, upload artifacts
- `docker.yml` — Build and push Docker image (future)

Push to `main` or `dev` to trigger CI.

## Performance

### Profiling

```bash
# Compile with debug info but optimized
RUSTFLAGS="-g" cargo build --release

# Run with perf (Linux)
perf record --call-graph=dwarf ./target/release/feynman-engine
perf report

# Benchmark (with criterion.rs)
cargo bench
```

### Optimization

- Use `rust_decimal` for all financial math (never `f64`)
- Avoid allocations in hot path (order submission)
- Cache venue adapter instances
- Use `tokio::task` for CPU-bound work (not blocking threads)

## Troubleshooting

| Issue | Solution |
|-------|----------|
| `error: toolchain 'stable' has no component 'rust-analyzer'` | `rustup component add rust-analyzer` |
| `error: linker 'cc' not found` | Install build tools: `xcode-select --install` (macOS) or `sudo apt-get install build-essential` (Ubuntu) |
| `error: couldn't compile proto` | Install protobuf: `brew install protobuf` |
| `error: no module named 'tonic_build'` | `cargo build` first (generates proto code) |
| Build cache stale | `cargo clean && cargo build` |
| Tests fail but pass locally | Try `cargo test --no-default-features` |

## Git Workflow

```bash
# Create feature branch
git checkout -b feat/signal-validation

# Make changes, test
cargo test --workspace

# Commit
git commit -m "feat: add signal validation"

# Push
git push -u origin feat/signal-validation

# Open PR on GitHub
gh pr create --draft
```

## Adding a New Crate

```bash
# 1. Create the directory structure
mkdir -p crates/newcrate/src

# 2. Create Cargo.toml (template in gateway/Cargo.toml)
# 3. Create src/lib.rs with doc comment
# 4. Add to workspace root Cargo.toml members list
# 5. Test it compiles
cargo build -p newcrate
```

## Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Tokio Tutorial](https://tokio.rs/tokio/tutorial)
- [tonic (gRPC) Examples](https://github.com/hyperium/tonic/tree/master/examples)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)

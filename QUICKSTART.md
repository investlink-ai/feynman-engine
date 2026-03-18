# Quick Start

## One-Time Setup

```bash
cd /Users/feynman/Documents/projects/feynman-engine

# Verify Rust is installed (1.82+)
rustc --version
cargo --version

# Install protobuf compiler (if not already installed)
brew install protobuf

# First build (slow, downloads 194 crates)
cargo build
```

## Development Loop

### 1. Make Changes

Edit code in `crates/` or `bins/`. Example:

```bash
# Edit types
vim crates/types/src/lib.rs

# Edit gateway logic
vim crates/gateway/src/lib.rs
```

### 2. Test Locally

```bash
# Quick check (1-2 sec)
cargo check

# Full test suite (5-10 sec)
cargo test --workspace

# Linting
cargo clippy --workspace -- -D warnings

# Format
cargo fmt
```

### 3. Run the Engine

```bash
# Start Redis first (separate terminal)
docker run -p 6379:6379 redis:7-alpine

# Run engine (debug mode)
cargo run --bin feynman-engine -- --config config/default.toml
```

### 4. Test gRPC API

```bash
# In another terminal
grpcurl -plaintext localhost:50051 \
  feynman.engine.v1.ExecutionService/GetEngineHealth
```

## Next Steps

1. **Initialize Git** (if needed):
   ```bash
   git init
   git add .
   git commit -m "chore: initial engine scaffold"
   ```

2. **Phase 0 Work** (see PHASE_0_CHECKLIST.md):
   - Complete `crates/types/` with all domain types ✓ (done)
   - Implement `crates/risk/` — AgentRiskManager
   - Implement `crates/bus/` — Redis Streams client
   - Write integration tests in `tests/integration/`

3. **Run Tests**:
   ```bash
   cargo test --workspace -- --nocapture
   ```

4. **Build Release**:
   ```bash
   cargo build --release
   # Binary: target/release/feynman-engine
   ```

5. **Docker**:
   ```bash
   docker compose -f docker/docker-compose.yml up -d
   ```

## Common Issues

| Error | Fix |
|-------|-----|
| `error: linker 'cc' not found` | Install build tools: `xcode-select --install` |
| `error: couldn't compile proto` | Install protobuf: `brew install protobuf` |
| `redis connection refused` | Start Redis: `docker run -p 6379:6379 redis:7-alpine` |

See [docs/DEVELOPMENT.md](./docs/DEVELOPMENT.md) for detailed developer reference.

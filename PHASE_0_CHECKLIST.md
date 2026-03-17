# Phase 0: Scaffold — Checklist

**Duration:** 2 weeks
**Gate Alignment:** Post Gate 1 (50 closed trades)
**Risk Level:** Low
**Status:** IN PROGRESS

Scaffolding the Rust engine workspace with custom types, risk checks, and agent bus foundation. No Nautilus dependency yet.

---

## Completed ✓

- [x] Cargo workspace initialized (`Cargo.toml` with 8 crates)
- [x] CI/CD workflows (GitHub Actions: test, build)
- [x] Docker foundation (compose, Dockerfile, config)
- [x] `crates/types/` — domain types with serde + proptest
  - [x] Identifiers (OrderId, AgentId, VenueId, etc.)
  - [x] AgentAllocation, AgentRiskLimits, Agent Status
  - [x] InstrumentRiskLimits, VenueRiskLimits, PredictionMarketLimits
  - [x] Signal (from LLM agents)
  - [x] FirmBook, StrategyPosition, InstrumentExposure
  - [x] TrackedPosition with fill attribution
  - [x] PredictionTracker
  - [x] RiskViolation enums
  - [x] Unit tests (serde round-trip, arithmetic invariants)
- [x] gRPC service definition (`proto/feynman/engine/v1/service.proto`)
- [x] README.md (overview, structure, quick start)
- [x] DEVELOPMENT.md (setup, testing, debugging)
- [x] QUICKSTART.md (one-page quick start)

---

## In Progress 🚧

### 1. Port MVP 7-Point Risk Checklist

**File:** `crates/agent-risk/src/lib.rs`

Implement MVP risk gate rules as `AgentRiskManager::evaluate()`:

```rust
pub struct AgentRiskManager {
    agent_limits: HashMap<AgentId, AgentRiskLimits>,
    instrument_limits: HashMap<InstrumentId, InstrumentRiskLimits>,
    // ...
}

impl AgentRiskManager {
    /// Full risk check: MVP 7-point checklist + per-agent isolation
    pub fn evaluate(
        &self,
        order: &Order,
        firm_book: &FirmBook,
    ) -> Result<RiskApproval, Vec<RiskViolation>> {
        // 1. Stop loss defined?
        // 2. Risk/reward >= 2:1?
        // 3. Position <= 5% NAV?
        // 4. Account risk <= 1% NAV?
        // 5. Leverage within limits?
        // 6. Drawdown < 15%?
        // 7. Cash >= 20% NAV?
        //
        // + Per-agent budget check
        // + Per-instrument concentration
        // + Per-venue exposure
    }
}
```

**Exit criteria:** Property tests: no combination of valid inputs causes panic. All 7 MVP checks present.

**Effort:** 3 days

---

### 2. Agent Bus (Redis Streams) Implementation

**File:** `crates/agent-bus/src/lib.rs`

Implement Redis Streams client:

```rust
#[async_trait]
pub trait AgentBus: Send + Sync {
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId>;
    async fn subscribe(...) -> Result<mpsc::UnboundedReceiver<BusMessage>>;
    async fn ack(...) -> Result<()>;
    async fn pending(...) -> Result<Vec<BusMessage>>;
    async fn claim(...) -> Result<Vec<BusMessage>>;
}

pub struct RedisBus {
    client: fred::RedisClient,
}

impl AgentBus for RedisBus {
    // Use XADD, XREAD with consumer groups
    // Use XPENDING for stuck message detection
    // Use XCLAIM for recovery
}
```

**Exit criteria:** Integration test: publish → subscribe → ack round-trip works. Consumer group redelivery on missed ack.

**Effort:** 3 days

---

### 3. Agent Risk Integration Tests

**File:** `tests/integration/risk.rs`

Test risk gate against MVP rules:

```rust
#[tokio::test]
async fn test_stop_loss_required() {
    // Order without stop loss → REJECT
}

#[tokio::test]
async fn test_risk_reward_check() {
    // Order with R:R < 2:1 → REJECT
}

#[tokio::test]
async fn test_position_sizing_cap() {
    // Order > 5% NAV → RESIZE
}

#[tokio::test]
async fn test_drawdown_halt() {
    // Drawdown > 15% → REJECT ALL
}

// ... 7 tests total, one per MVP rule
```

**Exit criteria:** All 7 tests pass. No panics under stress (property tests).

**Effort:** 2 days

---

## Pending ⏳

### 4. Agent Bus Redis Integration Test

**File:** `tests/integration/bus.rs`

Test Redis Streams end-to-end:

```rust
#[tokio::test]
async fn test_pub_sub_round_trip() {
    let bus = RedisBus::connect("redis://localhost:6379").await?;
    bus.publish("signals", &[1, 2, 3]).await?;
    let mut sub = bus.subscribe("signals", "group1", "consumer1").await?;
    let msg = sub.recv().await?;
    bus.ack("signals", "group1", &msg.id).await?;
}

#[tokio::test]
async fn test_consumer_group_redelivery() {
    // Publish, don't ack, verify XPENDING shows pending
}

#[tokio::test]
async fn test_stuck_message_claim() {
    // Publish, simulate crash, claim via XCLAIM, verify redelivery
}
```

**Exit criteria:** All Redis Streams patterns work. Consumer groups verified.

**Effort:** 2 days

---

### 5. Docker Compose Smoke Test

**File:** `tests/integration/docker.rs` (or bash script)

Test engine starts in Docker:

```bash
docker compose -f docker/docker-compose.yml up -d
sleep 5
grpc_health_probe -addr=localhost:50051
# Should return "serving"
docker compose down
```

**Exit criteria:** `docker compose up` starts engine + Redis. Health check passes.

**Effort:** 1 day

---

### 6. First CI/CD Green Build

**Trigger:** Push to `main` or `dev`

Check `.github/workflows/test.yml` passes:
- `cargo test --workspace`
- `cargo clippy`
- `cargo fmt --check`

**Exit criteria:** GitHub Actions workflow succeeds. No linting errors.

**Effort:** 1 day (async, happens on push)

---

## Validation Targets

| Artifact | Target | Status |
|----------|--------|--------|
| **Types compile** | `cargo build -p types` passes | ✓ |
| **Risk logic correct** | All 7 MVP rules tested | ⏳ |
| **Agent bus works** | Redis Streams round-trip verified | ⏳ |
| **Docker works** | `docker compose up` succeeds | ⏳ |
| **CI green** | GitHub Actions passes | ⏳ |

---

## Exit Criteria

Phase 0 is **complete** when:

1. ✓ Custom types compile (done)
2. ⏳ Agent risk manager evaluates all 7 MVP checks (in progress)
3. ⏳ Redis Streams bus works with consumer groups (pending)
4. ⏳ Integration tests pass (pending)
5. ⏳ Docker compose starts the engine (pending)
6. ⏳ CI/CD green (pending)

**Estimated remaining: ~2 weeks** (1 week coding + 1 week testing + integration)

---

## Unblocked Work

While Phase 0 is in progress, **parallel work** can happen:

- **feynman_trading_bot repo:** Continue MVP trading. No changes needed.
- **Documentation:** Read HYBRID_ENGINE_ARCHITECTURE.md, plan Phase 1 Nautilus integration.
- **Research:** Evaluate Nautilus Rust crates; prepare Phase 1 spike plan.

---

## Next Phase

Once Phase 0 complete:

→ **Phase 1: Nautilus Integration** (3 weeks, HIGH risk)
- Add `nautilus-*` Cargo dependencies
- Implement PortfolioBridge (Signal → Nautilus order)
- Spike: verify Nautilus pure-Rust viability

See [HYBRID_ENGINE_ARCHITECTURE.md §14.3](../feynman_trading_bot/docs/HYBRID_ENGINE_ARCHITECTURE.md#143-phase-1-nautilus-integration) for details.

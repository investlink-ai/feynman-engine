# Safety Rules for feynman-engine

These rules are **non-negotiable** and must be verified before any code change is merged.

## 1. Pipeline Integrity (No Single Point Failure)

The order pipeline has three gates for safety:

```
Satoshi (Trader)  →  Taleb (Risk)  →  Executor (Engine)
publish signal       approve/kill      execute order
```

**Rule:** Never collapse this pipeline. No component can:
- Submit orders directly to Bybit without Taleb approval
- Override Taleb's risk gates
- Bypass conviction/stop-loss checks

**Verification:**
- `execute_order` is **only** callable from Taleb's MCP session
- All signal-to-order conversions go through risk evaluation first
- No direct Bybit API calls from agent workspaces

## 2. Financial Math — Decimal Only

**Rule:** All calculations involving money must use `rust_decimal::Decimal`. **Never** use `f64`, `f32`, or `i64` for prices/quantities.

**Why:** Floating-point math cannot represent decimal prices accurately (0.1 + 0.2 ≠ 0.3 in binary float).

**Verification:**
```rust
// ✓ Correct
let notional = Decimal::from(qty) * price;

// ✗ Wrong — will cause silent rounding errors
let notional = qty as f64 * price as f64;
```

**Failure mode:** Silently incorrect position valuations → hidden losses or over-leverage.

## 3. Fail-Fast on Invalid Input

**Rule:** If input is invalid (NaN, negative where not allowed, missing required field), raise immediately. Never silently clamp, default, or skip.

**Verification:**
```rust
// ✓ Correct
if conviction < Decimal::ZERO || conviction > Decimal::ONE {
    anyhow::bail!("conviction must be 0.0–1.0, got {}", conviction);
}

// ✗ Wrong — silently hides bad data
if conviction < Decimal::ZERO {
    conviction = Decimal::ZERO;
}
```

**Failure mode:** Bad signals are accepted without warning → trades based on corrupted data.

## 4. dryRun Guard (Default: True)

**Rule:** All Bybit API calls must check `dryRun: true` before submission. **dryRun defaults to true.** Real orders require explicit override.

**Verification:**
```rust
pub struct ExecutionConfig {
    pub dry_run: bool,  // defaults to true
}

impl Executor {
    async fn submit_order(&self, order: &Order) -> Result<()> {
        if !self.config.dry_run {
            self.venue_adapter.submit(order).await?;
        } else {
            info!("dry_run: skipping submission");
        }
        Ok(())
    }
}
```

**Failure mode:** Accidental real orders submitted during testing.

## 5. Idempotent Submission

**Rule:** If an order submission is retried (network error, timeout), it must not create duplicate orders.

**Verification:**
```rust
// ✓ Correct — use clientOrderId for idempotency
let response = venue.submit_order_with_client_id(order, client_id).await?;

// Check existing state before creating new order
if let Some(existing) = self.orders.get(&order.id) {
    return Ok(existing.clone());
}
```

**Failure mode:** Network retry → duplicate orders → unintended large positions.

## 6. Partial Fill Handling

**Rule:** Position state must never assume full execution on the first fill. Track all fills with `fill_ids`.

**Verification:**
```rust
pub struct TrackedPosition {
    pub qty: Decimal,
    pub fill_ids: Vec<(OrderId, u64)>,  // Track which fills are included
    pub accumulated_funding: Decimal,
}
```

**Failure mode:** Partial fill ignored → P&L miscalculated → risk limit exceeded.

## 7. Error Propagation (No Silent Failures)

**Rule:** Bybit errors must be surfaced to the caller. Never log and continue.

**Verification:**
```rust
// ✓ Correct — error is propagated
match venue_adapter.submit_order(order).await {
    Ok(response) => Ok(response),
    Err(e) => {
        error!("Bybit submission failed: {:?}", e);
        Err(e)  // caller decides what to do
    }
}

// ✗ Wrong — error is swallowed
match venue_adapter.submit_order(order).await {
    Ok(response) => Ok(response),
    Err(e) => {
        error!("Bybit submission failed: {:?}", e);
        Ok(())  // caller thinks it succeeded!
    }
}
```

**Failure mode:** Ghost positions — caller thinks order is pending, but it failed at exchange.

## 8. Taleb Gate Preserved

**Rule:** Only Taleb can call `execute_order`. No exceptions, no shortcuts.

**Verification:**
- Taleb has exclusive MCP session token
- `execute_order` checks caller identity
- Non-Taleb callers get permission denied
- No env var override, no backdoor

**Failure mode:** Other agents execute orders → unpermissioned risk-taking.

## 9. Per-Agent Budget Isolation

**Rule:** One agent's loss cannot reduce another agent's available capital. Risk limits are per-agent.

**Verification:**
```rust
// ✓ Correct — per-agent check
let agent_allocation = self.allocations.get(&order.agent)?;
anyhow::ensure!(
    order.notional <= agent_allocation.free_capital,
    "order exceeds agent {} free capital", order.agent
);

// ✗ Wrong — firm-level check only
let firm_free = self.firm_book.free_capital;
anyhow::ensure!(order.notional <= firm_free, "order exceeds firm free capital");
```

**Failure mode:** Agent A blows through limits → starves Agent B of capital.

## 10. No State Mutation Before Confirmation

**Rule:** Portfolio state must not be updated until the exchange confirms the order. Use two-phase commit pattern.

**Verification:**
```
Phase 1: Submit to exchange (pending)
Phase 2: Wait for fill confirmation
Phase 3: Update portfolio state (committed)
```

**Failure mode:** Premature state update → mismatch between local portfolio and exchange reality.

## 11. Explicit Over Implicit (No Silent Fallbacks)

**Rule:** No silent defaults, fallback chains, or hidden behavior. Every code path must be explicit and traceable.

**Banned patterns:**
```rust
// ✗ Wrong — silent default hides missing config
let timeout = config.timeout.unwrap_or(Duration::from_secs(30));

// ✓ Correct — fail if not configured
let timeout = config.timeout
    .ok_or_else(|| anyhow::anyhow!("timeout not configured"))?;

// ✗ Wrong — silent venue fallback
if venue_a.submit(order).await.is_err() {
    venue_b.submit(order).await?;  // caller doesn't know venue changed
}

// ✓ Correct — return error, let caller decide
venue_a.submit(order).await?;  // caller sees the failure

// ✗ Wrong — silent emulation
if !venue.supports_trailing_stop() {
    self.emulate_trailing_stop(order).await?;  // caller doesn't know
}

// ✓ Correct — return unsupported, require opt-in
if !venue.supports_trailing_stop() && !order.exec_hint.allow_emulation {
    anyhow::bail!("venue {} does not support trailing stops", venue.id());
}
```

**Failure mode:** Silent fallbacks are invisible in logs and impossible to debug in production. When a trade goes wrong, you cannot reconstruct what actually happened.

## 12. Concurrency Safety (Single-Owner State)

**Rule:** All mutable business state (FirmBook, positions, risk limits, orders) is owned exclusively by the Sequencer task. No `Arc<Mutex<T>>` or `Arc<RwLock<T>>` on business state. Tasks communicate via bounded channels.

**Banned patterns:**
```rust
// ✗ Wrong — shared mutable state behind lock
let firm_book = Arc::new(RwLock::new(FirmBook::default()));

// ✓ Correct — Sequencer owns the state, others get snapshots
enum SequencerCommand {
    Snapshot { respond: oneshot::Sender<EngineStateSnapshot> },
}

// ✗ Wrong — unbounded channel hides backpressure
let (tx, rx) = mpsc::unbounded_channel();

// ✓ Correct — bounded with explicit capacity
let (tx, rx) = mpsc::channel(1024);
```

**Failure mode:** Shared locks → data races under load, deadlocks, non-deterministic backtest, untraceable state corruption.

---

## Pre-Merge Checklist

Before marking any executor/bus change as ready for merge:

- [ ] All risk checks pass (universal for all orders, signal-specific for signals)
- [ ] Property tests pass (no panic on valid inputs)
- [ ] No `unwrap()` or `panic!()` without doc comment
- [ ] No `f64` in financial math
- [ ] dryRun guard checked before each exchange call
- [ ] Taleb gate still exclusive
- [ ] Error handling surfaces (not swallows) exchange errors
- [ ] Per-agent budget isolation verified
- [ ] Partial fills are tracked (not assumed full)
- [ ] No silent fallbacks or hidden defaults (explicit over implicit)
- [ ] No `Arc<Mutex<T>>` or `Arc<RwLock<T>>` on business state
- [ ] No `mpsc::unbounded_channel()` in production code
- [ ] No `unwrap_or_default()` or `unwrap_or()` in financial logic
- [ ] `/hostile-review` passed

Run:
```bash
make check
/hostile-review
```

## Emergency Shutdown

If a safety rule is violated in production:

1. **Halt all agents** — `HaltAll` RPC immediately
2. **Preserve logs** — no cleanup, collect full trace
3. **Notify Feynman (CIO)** — human intervention required
4. **Post-incident review** — add to anti-patterns, update rules if needed

# Coding Guidelines for feynman-engine

## Rust Code Style

### Naming Conventions
- **Types:** `PascalCase` (e.g., `AgentAllocation`, `VenueExposure`)
- **Functions/methods:** `snake_case` (e.g., `evaluate_risk`, `submit_order`)
- **Constants:** `SCREAMING_SNAKE_CASE` (e.g., `MAX_LEVERAGE`, `DEFAULT_TIMEOUT_MS`)
- **Identifiers (newtypes):** `PascalCase` for the type itself (e.g., `struct OrderId(String)`)

### Error Handling
Use `anyhow::Result<T>` for fallible functions:

```rust
pub fn evaluate(&self, order: &Order, book: &FirmBook) -> Result<RiskApproval> {
    let position_size = /* ... */;
    anyhow::ensure!(position_size <= self.max_notional,
        "position size {} exceeds limit {}", position_size, self.max_notional);
    Ok(RiskApproval::Approved)
}
```

### Financial Types
Always use `rust_decimal::Decimal`:

```rust
use rust_decimal::Decimal;

pub struct Order {
    pub qty: Decimal,
    pub price: Decimal,
    pub stop_loss: Option<Decimal>,
}
```

### Async/Await
Use `#[tokio::main]` for entry point, `#[tokio::test]` for async tests:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    // ...
}

#[tokio::test]
async fn test_publish_subscribe() {
    // ...
}
```

## Architecture Guidelines

### Crate Dependencies
Respect the dependency order to maintain layering:

```
types (no deps except std, serde)
  ↓
agent-risk, agent-bus (depend on types)
  ↓
bridge, pipeline (depend on types + risk/bus)
  ↓
dashboard, api (depend on pipeline + types)
  ↓
feynman-engine binary (orchestrates all)
```

No circular dependencies. No jumping layers.

### Concurrency Model

The Sequencer task owns all mutable business state. Other tasks communicate via bounded channels.

```rust
// ✓ Correct — Sequencer owns state, others send commands
let (cmd_tx, cmd_rx) = mpsc::channel(1024);
// gRPC handler sends command, awaits response via oneshot
cmd_tx.send(SequencerCommand::SubmitOrder { order, respond: tx }).await?;
let ack = rx.await?;

// ✗ BANNED — shared mutable state behind locks
let state = Arc::new(RwLock::new(EngineState::default()));

// ✗ BANNED — unbounded channels (hides backpressure)
let (tx, rx) = mpsc::unbounded_channel();

// ✓ Correct — bounded with explicit capacity and timeout
let (tx, rx) = mpsc::channel::<ApprovedOrder>(256);
tx.send_timeout(order, Duration::from_secs(5)).await?;
```

See `docs/CORE_ENGINE_DESIGN.md` §15 for the full concurrency architecture.

### Explicit Over Implicit

No silent defaults, fallback chains, or hidden behavior:

```rust
// ✗ BANNED in financial logic — hides missing values
let price = order.price.unwrap_or_default();
let qty = config.max_qty.unwrap_or(Decimal::from(100));

// ✓ Correct — fail explicitly
let price = order.price
    .ok_or_else(|| anyhow::anyhow!("price is required for limit orders"))?;
let qty = config.max_qty
    .ok_or_else(|| anyhow::anyhow!("max_qty not configured"))?;

// ✗ BANNED — match with silent catch-all
match order.side {
    Side::Buy => { /* ... */ }
    _ => { /* silently does nothing */ }
}

// ✓ Correct — exhaustive match
match order.side {
    Side::Buy => { /* ... */ }
    Side::Sell => { /* ... */ }
}
```

### Trait Design
Prefer fine-grained traits for capabilities:

```rust
pub trait RiskEvaluator: Send + Sync {
    fn evaluate(&self, order: &Order) -> Result<RiskApproval>;
}

pub trait MessageBus: Send + Sync {
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId>;
    async fn subscribe(&self, topic: &str) -> Result<Receiver<Message>>;
}
```

### Testing Strategy
- **Unit tests** in the same file (`#[cfg(test)]` module)
- **Integration tests** in `tests/integration/`
- **Property tests** marked with `#[ignore]` (run via `cargo test -- --ignored`)

Example unit test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_approval_with_valid_order() {
        let order = Order { /* ... */ };
        let book = FirmBook { /* ... */ };
        let manager = AgentRiskManager::new(/* ... */);

        let result = manager.evaluate(&order, &book);
        assert!(result.is_ok());
    }
}
```

### Documentation
Add doc comments to public items:

```rust
/// Evaluates an order against all risk checks.
///
/// Returns `RiskApproval` if all checks pass, or a list of `RiskViolation`s
/// if any check fails.
///
/// # Panics
/// Never panics — returns Result instead.
pub fn evaluate(&self, order: &Order) -> Result<RiskApproval, Vec<RiskViolation>> {
    // ...
}
```

## Money-Touching Code

### Rules
1. **No f64** — only `Decimal`
2. **Fail-fast** — NaN/negative/None → raise immediately
3. **dryRun guard** — check before any exchange call
4. **Idempotency** — use `clientOrderId` or check existing state
5. **Error propagation** — surface Bybit errors, never swallow
6. **Taleb gate** — only Taleb can call `execute_order`

### Before Commit
Run hostile review on any executor/bus changes:
```bash
/hostile-review
```

## Imports Organization
Group imports logically:

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

## Common Mistakes to Avoid

| Mistake | Fix |
|---------|-----|
| Using `f64` for prices/quantities | Use `Decimal` from `rust_decimal` |
| Panicking on invalid input | Return `Result<T>` with `anyhow::bail!` |
| Ignoring partial fills | Track fill attribution in `TrackedPosition.fill_ids` |
| Swallowing venue errors | Propagate errors to caller with `?` operator |
| Storing secrets in code | Use `.env` or config files; gitignore them |
| Blocking async code | Use `tokio::task::spawn_blocking` for CPU-bound work |
| Circular dependencies | Review crate dependency graph before adding imports |
| `Arc<RwLock<T>>` on business state | Sequencer owns state; communicate via channels |
| `mpsc::unbounded_channel()` | Always use bounded channels with explicit capacity |
| `unwrap_or_default()` in financial logic | Handle `None` explicitly with `ok_or_else` |
| Silent fallback on error | Return `Err`, let the caller decide |
| `if let` with silent else | Use exhaustive `match` — no swallowed branches |
| `.await` while holding mutable state | Sequencer processes sync; async only for channel I/O |

## Building & Testing

```bash
# Quick check (1-2 sec)
cargo check

# Full test suite (5-10 sec)
cargo test --workspace

# Linting
cargo clippy --workspace -- -D warnings

# Format
cargo fmt

# Before committing
make check
```

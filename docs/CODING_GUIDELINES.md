# Coding Guidelines for feynman-engine

## Rust Code Style

### Naming Conventions
- **Types:** `PascalCase` (e.g., `AgentAllocation`, `VenueExposure`)
- **Functions/methods:** `snake_case` (e.g., `evaluate_risk`, `submit_order`)
- **Constants:** `SCREAMING_SNAKE_CASE` (e.g., `MAX_LEVERAGE`, `DEFAULT_TIMEOUT_MS`)
- **Identifiers (newtypes):** `PascalCase` for the type itself (e.g., `struct OrderId(String)`)

### Error Handling

**Library crates** (`risk`, `bus`, `gateway`, `engine-core`, etc.) — typed errors with `thiserror`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error("position {notional} exceeds 5% NAV limit {limit}")]
    PositionLimitExceeded { notional: Decimal, limit: Decimal },
    #[error("circuit breaker {breaker} tripped: {reason}")]
    CircuitBreakerTripped { breaker: String, reason: String },
}

// Mark critical return values with #[must_use]
#[must_use]
pub fn evaluate(&self, order: &CanonicalOrder) -> Result<RiskApproval, RiskError> { … }
```

**Binary / application code** (`bins/feynman-engine`, integration tests) — `anyhow`:

```rust
anyhow::ensure!(position_size <= self.max_notional,
    "position {} exceeds limit {}", position_size, self.max_notional);
```

Never use `Box<dyn Error>`. Never `unwrap()` in non-test code without a doc comment explaining why it is safe.

### Financial Types
Always use `rust_decimal::Decimal`. Zero `f64` in financial logic.

```rust
pub struct Order {
    pub qty: Decimal,
    pub price: Decimal,
    pub stop_loss: Option<Decimal>,
}
```

### Async/Await
Use `#[tokio::main]` for entry point, `#[tokio::test]` for async tests. Never block inside an async context — use `tokio::task::spawn_blocking` for CPU-bound work.

```rust
#[tokio::main]
async fn main() -> Result<()> { … }

#[tokio::test]
async fn test_publish_subscribe() { … }
```

### Clippy Configuration

Add to every financial crate's `Cargo.toml`:

```toml
[lints.clippy]
pedantic = "warn"
module_name_repetitions = "allow"   # common in domain code
```

---

## Rust Patterns

### Newtype Pattern

Every domain identifier is a newtype. Never accept raw `String` for `OrderId`, `AgentId`, `VenueId`, etc.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub String);

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
```

Implement `Display` for user-facing output and `Debug` for dev inspection. Always derive `Hash` on ID types so they work as `HashMap` keys.

### Type-State Pattern (Order Lifecycle)

Encode lifecycle phases in the type system. Invalid transitions are compile errors.

```rust
pub struct Draft;
pub struct Validated;
pub struct Approved;
pub struct Submitted;

pub struct Order<S> {
    pub id: OrderId,
    pub qty: Decimal,
    _state: std::marker::PhantomData<S>,
}

impl Order<Draft> {
    pub fn validate(self, caps: &VenueCapabilities) -> Result<Order<Validated>, ValidationError> { … }
}
impl Order<Validated> {
    pub fn approve(self, gate: &dyn RiskGate) -> Result<Order<Approved>, RiskError> { … }
}
impl Order<Approved> {
    pub fn submit(self, adapter: &dyn VenueAdapter) -> Result<Order<Submitted>> { … }
}
```

The compiler prevents calling `submit()` on a `Draft` order. No runtime state-check needed.

### Conversion Traits

Use `From`/`TryFrom` for all type conversions. No standalone `to_foo()`/`from_foo()` functions.

```rust
impl TryFrom<SignalProto> for Signal {
    type Error = anyhow::Error;
    fn try_from(proto: SignalProto) -> Result<Self> {
        Ok(Signal {
            conviction: Decimal::from_str(&proto.conviction)
                .map_err(|e| anyhow::anyhow!("invalid conviction: {e}"))?,
        })
    }
}
```

### Builder Pattern

Any struct with 3+ optional fields uses a builder. Validation happens in `build()`, not in setters.

```rust
#[derive(Default)]
pub struct RiskLimitsBuilder {
    max_gross_notional: Option<Decimal>,
    per_agent: HashMap<AgentId, AgentRiskLimits>,
}

impl RiskLimitsBuilder {
    pub fn max_gross_notional(mut self, v: Decimal) -> Self {
        self.max_gross_notional = Some(v); self
    }
    pub fn build(self) -> Result<RiskLimits> {
        Ok(RiskLimits {
            max_gross_notional: self.max_gross_notional
                .ok_or_else(|| anyhow::anyhow!("max_gross_notional is required"))?,
        })
    }
}
```

### Sealed Traits

Safety-critical traits (`CircuitBreaker`, `VenueAdapter`) must not be implemented outside the crate.

```rust
mod private { pub trait Sealed {} }

/// External crates cannot implement this trait.
pub trait CircuitBreaker: private::Sealed + Send + Sync {
    fn check(&self, order: &CanonicalOrder) -> Result<(), CircuitBreakerTrip>;
}

// Only the authoritative implementation gets Sealed
impl private::Sealed for HardcodedCircuitBreaker {}
```

### Module Privacy

Default to the most restrictive visibility that works:

```rust
pub struct RiskGateImpl { … }          // part of crate's public contract
pub(crate) fn compute_max_loss(…) {}   // shared within crate, not exported
fn validate_internal(…) {}             // module-private
```

---

## Architecture Guidelines

### Crate Dependencies
Respect the dependency order to maintain layering:

```
types (no deps except std, serde)
  ↓
risk, bus (depend on types)
  ↓
gateway, engine-core (depend on types + risk/bus)
  ↓
observability, api (depend on engine-core + types)
  ↓
feynman-engine binary (orchestrates all)
```

No circular dependencies. No jumping layers.

### Concurrency Model

The Sequencer task owns all mutable business state. Other tasks communicate via bounded channels.

```rust
// ✓ Correct — Sequencer owns state, others send commands
let (cmd_tx, cmd_rx) = mpsc::channel(1024);
cmd_tx.send(SequencerCommand::SubmitOrder { order, respond: tx }).await?;
let ack = rx.await?;

// ✗ BANNED — shared mutable state behind locks
let state = Arc::new(RwLock::new(EngineState::default()));

// ✗ BANNED — unbounded channels (hides backpressure)
let (tx, rx) = mpsc::unbounded_channel();

// ✓ Correct — bounded with explicit capacity
let (tx, rx) = mpsc::channel::<ApprovedOrder>(256);
```

Static dispatch (`impl Trait`) in the Sequencer hot path. `Box<dyn Trait>` only at wiring boundaries.

See `docs/CORE_ENGINE_DESIGN.md` §15 for the full concurrency architecture.

### Explicit Over Implicit

No silent defaults, fallback chains, or hidden behavior:

```rust
// ✗ BANNED — hides missing values
let price = order.price.unwrap_or_default();

// ✓ Correct — fail explicitly
let price = order.price
    .ok_or_else(|| anyhow::anyhow!("price is required for limit orders"))?;

// ✗ BANNED — silent catch-all
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

Fine-grained, single-responsibility traits:

```rust
// Hot path — static dispatch, zero-cost
fn check_position<R: RiskEvaluator>(evaluator: &R, order: &CanonicalOrder) { … }

// Wiring boundary — dynamic dispatch acceptable
pub struct Sequencer {
    risk: Box<dyn RiskGate>,
    bus: Box<dyn MessageBus>,
}
```

---

## Testing Strategy

- **Unit tests** in the same file (`#[cfg(test)]` module)
- **Integration tests** in `tests/integration/`
- **Property tests** with `proptest`, run via `cargo test -- --ignored`
- No production code in `#[cfg(test)]` — test helpers go in `#[cfg(any(test, feature = "test-utils"))]`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_approval_with_valid_order() {
        let manager = AgentRiskManager::new(/* ... */);
        let result = manager.evaluate(&order, &book);
        assert!(result.is_ok());
    }
}
```

---

## Documentation

Add doc comments to every public item:

```rust
/// Evaluates an order against all risk checks.
///
/// Returns `RiskApproval` if all checks pass, or `RiskError` if rejected.
///
/// # Errors
/// - `PositionLimitExceeded` — notional exceeds 5% NAV
/// - `CircuitBreakerTripped` — system halted
///
/// # Panics
/// Never panics — returns `Result` instead.
#[must_use]
pub fn evaluate(&self, order: &CanonicalOrder) -> Result<RiskApproval, RiskError> { … }
```

---

## Money-Touching Code

1. **No f64** — only `Decimal`
2. **Fail-fast** — invalid input → `Err(...)` immediately, never clamp or default
3. **dryRun guard** — check before any exchange call
4. **Idempotency** — use `client_order_id` or check existing state first
5. **Error propagation** — surface venue errors, never swallow
6. **#[must_use]** — on all functions returning risk approvals or order acks

Run `/hostile-review` before finalizing any code in `risk`, `gateway`, `engine-core`, `api`, `bins/feynman-engine`.

---

## Common Mistakes

| Mistake | Fix |
|---------|-----|
| `f64` for prices/quantities | `Decimal` from `rust_decimal` |
| `anyhow` in library crates | `thiserror` enum for structured errors |
| `unwrap_or_default()` in financial logic | `ok_or_else` with explicit error |
| No `#[must_use]` on approval functions | Add attribute — caller must handle result |
| Raw `String` for domain IDs | Newtype (`OrderId`, `AgentId`, `VenueId`) |
| Standalone `to_foo()` conversions | `From`/`TryFrom` traits |
| Struct with 5 optional fields | Builder pattern with `build()` validation |
| `dyn Trait` in hot path | `impl Trait` or generic `<T: Trait>` |
| External impl of `CircuitBreaker` | Sealed trait pattern |
| `pub` on internal helpers | `pub(crate)` or module-private |
| `Arc<RwLock<T>>` on business state | Sequencer owns state; channels |
| `mpsc::unbounded_channel()` | Bounded with explicit capacity |
| Silent fallback on error | Return `Err`, let caller decide |
| `if let` with silent else | Exhaustive `match` |
| `.clone()` in Sequencer loop | Borrow `&T`, clone at task boundaries |
| Blocking in async context | `tokio::task::spawn_blocking` |
| Circular crate dependencies | Check dependency graph first |

---

## Building & Testing

```bash
cargo check                              # quick syntax check (1-2s)
cargo test --workspace                   # full test suite
cargo clippy --workspace -- -D warnings # linting
cargo fmt                                # format
make check                               # full gate before commit
```

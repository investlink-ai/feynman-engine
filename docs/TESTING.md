# Testing Strategy — feynman-engine

**Last Updated:** 2026-03-18

Testing strategy for a custom Rust trading engine where correctness carries real financial consequences. Optimized for a solo developer: every test must earn its place.

---

## Testing Budget

Solo dev means limited testing time. Spend it where the blast radius is highest.

**High value (always test):**
- Risk evaluation logic — wrong decision = real money lost
- Type-state transitions — the architecture's safety guarantee
- Financial math — Decimal arithmetic, sizing, PnL
- State transitions — VenueState, pipeline stages

**Low value (don't test):**
- Derived trait impls (serde, Debug, Clone) — the derive macro is correct
- Trivial getters/setters — the compiler already checks these
- Generated gRPC code — tonic codegen is tested upstream
- Boilerplate constructors — unless they contain validation logic

**Rule of thumb:** if the compiler or a derive macro guarantees it, don't write a test for it. Spend that time on another risk rule test instead.

---

## Test Types

### 1. Unit Tests

In-module tests for individual functions. The bread and butter.

**Location:** `crates/{name}/src/*.rs` inside `#[cfg(test)]` modules
**Runs in CI:** Yes
**When to write:** Every function that computes, decides, or transforms

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_position_exceeding_nav_resizes() {
        let rm = TestRiskManager::with_nav(dec!(100_000));
        let order = TestOrder::buy(dec!(10_000)); // 10% NAV, over 5% limit

        let result = rm.evaluate(&order, &book, EvaluationPath::SubmitOrder);

        match result {
            RiskOutcome::Resize { new_qty, .. } => {
                assert_eq!(new_qty, dec!(5_000)); // capped to 5% NAV
            }
            other => panic!("expected Resize, got {:?}", other),
        }
    }
}
```

### 2. Compile-Fail Tests

Verify that the type system rejects invalid operations. **This is the highest-value test category for this project** — the entire architecture bet is that invalid pipeline transitions won't compile.

**Location:** `crates/types/tests/compile_fail/` (using `trybuild`)
**Runs in CI:** Yes
**When to write:** Every type-state transition boundary

```rust
// crates/types/tests/compile_fail/invalid_pipeline_skip.rs
use types::*;

fn main() {
    let order = PipelineOrder::<Draft>::new(/* ... */);
    // ERROR: Draft has no method `route()` — must validate and risk-check first
    let routed = order.route();
}
```

```rust
// crates/types/tests/compile_tests.rs
#[test]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
```

**What to verify won't compile:**
- `PipelineOrder<Draft>` calling `.route()` (skipping validate + risk_check)
- `PipelineOrder<Draft>` calling `.risk_check()` (skipping validate)
- `PipelineOrder<Validated>` calling `.submit()` (skipping risk_check + route)
- `PipelineOrder<RiskChecked>` calling `.submit()` (skipping route)
- Constructing `LiveOrder` directly without going through `submit()`

### 3. Snapshot Tests (Golden Tests)

Canonical input/output pairs for risk evaluation. When a risk rule changes, the snapshot diff shows exactly what changed. Catches regressions instantly.

**Location:** `crates/risk/tests/snapshots/` (using `insta`)
**Runs in CI:** Yes
**When to write:** Every risk rule, every `RiskOutcome` path

```rust
#[test]
fn snapshot_signal_without_stop_loss() {
    let rm = TestRiskManager::default();
    let order = TestOrder::signal_without_stop_loss();

    let result = rm.evaluate(&order, &book, EvaluationPath::SubmitSignal);

    insta::assert_debug_snapshot!(result);
    // Saved to: snapshots/snapshot_signal_without_stop_loss.snap
    // Contains: RiskOutcome::Rejected { violations: [MissingStopLoss] }
}
```

**Golden test set (one per risk rule):**

| Test name | Input | Expected RiskOutcome |
|-----------|-------|---------------------|
| `signal_no_stop_loss` | SubmitSignal, stop_loss=None | Rejected(MissingStopLoss) |
| `signal_bad_risk_reward` | SubmitSignal, R:R=1.5 | Rejected(InsufficientRiskReward) |
| `order_no_stop_loss_ok` | SubmitOrder, stop_loss=None | Approved (uses full notional for worst-case) |
| `position_over_5pct_nav` | qty=10% NAV | Resize(new_qty=5% NAV) |
| `drawdown_over_15pct` | firm drawdown=16% | Rejected(DrawdownHalt) |
| `cash_below_20pct` | cash=15% NAV | Rejected(InsufficientCash) |
| `agent_budget_exhausted` | agent spent > allocation | Rejected(AgentBudgetExceeded) |
| `all_checks_pass` | valid order within all limits | Approved |

When a snapshot changes, `cargo insta review` shows the diff. Accept intentional changes, reject regressions.

### 4. Property Tests

Fuzz invariants with randomly generated inputs. Catches edge cases that hand-written tests miss.

**Location:** Same file as unit tests, marked `#[ignore]` (slow)
**Runs in CI:** On demand (`cargo test -- --ignored`)
**When to write:** Risk evaluation, financial math, type conversions

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    #[ignore] // slow — run explicitly
    fn risk_evaluate_never_panics(
        qty in 1u64..1_000_000u64,
        price in 1u64..100_000u64,
        nav in 10_000u64..10_000_000u64,
    ) {
        let qty = Decimal::from(qty);
        let price = Decimal::from(price);
        let nav = Decimal::from(nav);

        let rm = TestRiskManager::with_nav(nav);
        let order = TestOrder::buy_at(qty, price);
        let book = TestFirmBook::with_nav(nav);

        // Must not panic — any valid input produces a valid RiskOutcome
        let result = rm.evaluate(&order, &book, EvaluationPath::SubmitOrder);
        match result {
            RiskOutcome::Approved => {}
            RiskOutcome::Resize { new_qty, .. } => {
                prop_assert!(new_qty > Decimal::ZERO, "resize qty must be positive");
                prop_assert!(new_qty <= qty, "resize qty must not exceed original");
            }
            RiskOutcome::Rejected { violations } => {
                prop_assert!(!violations.is_empty(), "rejected must have violations");
            }
        }
    }
}
```

**Invariants to fuzz:**
- `evaluate()` never panics on any valid input
- `Resize.new_qty` is always positive and less than original qty
- `Rejected.violations` is never empty
- `FillSummary.filled_qty + remaining_qty == original_qty`
- Decimal arithmetic: no overflow, no precision loss in common ranges

### 5. Integration Tests

Cross-crate behavior tests. Require external infrastructure (Redis, Docker).

**Location:** `tests/integration/{name}.rs` registered from the owning crate's `Cargo.toml`
**Runs in CI:** When infrastructure is available (or gated behind `#[ignore]`)
**When to write:** Bus round-trips, pipeline end-to-end, Docker smoke

```rust
// tests/integration/risk.rs
#[tokio::test]
async fn test_signal_without_stop_loss_rejected() {
    let rm = AgentRiskManager::new(/* production-like config */);
    let order = make_validated_order(/* signal without stop_loss */);
    let book = make_firm_book(dec!(100_000));

    let result = rm.evaluate(&order, &book, EvaluationPath::SubmitSignal);
    assert!(matches!(result, RiskOutcome::Rejected { .. }));
}
```

Run a workspace integration suite through its owning crate:

```bash
cargo test -p {crate} --test {name}
```

Each integration test uses its own isolated state — no shared mutable fixtures between tests.

### 6. Regression Tests

When a bug is found, the fix **must** include a test that would have caught it. This is non-optional.

**Protocol:**
1. Bug discovered — reproduce with a failing test first
2. Fix the bug — test goes green
3. Run `/evolve` — extract the pattern to `memory/anti-patterns.md`
4. The regression test stays forever — it is never deleted

```rust
#[test]
fn regression_risk_eval_zero_nav_division() {
    // BUG: evaluate() panicked on division by zero when NAV was 0
    // FIX: fail-fast with Err(InvalidNav) when NAV <= 0
    // See: memory/anti-patterns.md entry 2026-XX-XX
    let rm = TestRiskManager::with_nav(Decimal::ZERO);
    let result = rm.evaluate(&order, &book, EvaluationPath::SubmitOrder);
    assert!(matches!(result, RiskOutcome::Rejected { .. }));
}
```

---

## Test Fixtures

Building test objects (`FirmBook`, `PipelineOrder`, `AgentRiskLimits`) is the most repeated setup in the codebase. Use builders, not ad-hoc construction.

### Fixture module

```rust
// crates/types/src/test_fixtures.rs
#[cfg(any(test, feature = "test-utils"))]
pub mod fixtures {
    use super::*;

    /// Builder for test risk managers with sensible defaults.
    /// Override only what the specific test cares about.
    pub struct TestRiskManager { /* ... */ }

    impl TestRiskManager {
        pub fn default() -> AgentRiskManager {
            Self::with_nav(dec!(100_000))
        }

        pub fn with_nav(nav: Decimal) -> AgentRiskManager {
            AgentRiskManager {
                nav,
                agent_limits: default_agent_limits(),
                instrument_limits: default_instrument_limits(),
                venue_limits: default_venue_limits(),
                firm_drawdown_pct: dec!(0),
                cash_pct: dec!(50), // 50% cash — well above 20% minimum
            }
        }

        pub fn with_drawdown(nav: Decimal, drawdown_pct: Decimal) -> AgentRiskManager {
            let mut rm = Self::with_nav(nav);
            rm.firm_drawdown_pct = drawdown_pct;
            rm
        }
    }

    pub struct TestOrder { /* ... */ }

    impl TestOrder {
        /// Buy order at 1% NAV — well within limits
        pub fn small_buy() -> PipelineOrder<Validated> { /* ... */ }

        /// Buy order at 10% NAV — triggers 5% position resize
        pub fn large_buy() -> PipelineOrder<Validated> { /* ... */ }

        /// Signal without stop_loss — should be rejected on SubmitSignal path
        pub fn signal_without_stop_loss() -> PipelineOrder<Validated> { /* ... */ }

        /// Signal with R:R = 1.5 — below 2:1 threshold
        pub fn signal_bad_risk_reward() -> PipelineOrder<Validated> { /* ... */ }

        /// Custom order with explicit qty and stop_loss.
        /// Note: stop_loss is Option<Decimal> here because PipelineOrder uses OrderCore,
        /// where stop_loss is optional (SubmitOrder path). For Signal-path tests, stop_loss
        /// is always required — Signal.stop_loss is Decimal (non-optional). Use
        /// signal_without_stop_loss() to test the rejection case.
        pub fn buy_at(qty: Decimal, stop_loss: Option<Decimal>) -> PipelineOrder<Validated> {
            /* ... */
        }
    }

    pub struct TestFirmBook { /* ... */ }

    impl TestFirmBook {
        pub fn with_nav(nav: Decimal) -> FirmBook { /* ... */ }
        pub fn with_drawdown(nav: Decimal, drawdown_pct: Decimal) -> FirmBook { /* ... */ }
    }
}
```

### Design rules for fixtures

- **Sensible defaults** — `TestRiskManager::default()` produces a risk manager where a normal small order passes all checks. Tests override only what they're testing.
- **Named constructors for scenarios** — `TestOrder::signal_without_stop_loss()` is self-documenting. Don't make every test build orders from scratch.
- **No production code in test modules** — fixtures live in `#[cfg(any(test, feature = "test-utils"))]`
- **Fixtures don't assert** — they build objects. The test function asserts.

---

## Test Naming

```
test_{function}_{scenario}_{expected_outcome}
```

Examples:
```rust
test_evaluate_signal_without_stop_loss_rejected
test_evaluate_order_exceeding_nav_resized
test_evaluate_all_checks_pass_approved
test_venue_state_submitted_to_filled_valid
test_venue_state_filled_to_submitted_invalid
```

The name should tell you what failed without reading the test body.

---

## What Not to Test

Explicit guidance on where testing time is wasted:

| Don't test | Why |
|-----------|-----|
| `#[derive(Serialize, Deserialize)]` | serde derives are correct — test your custom `impl` if you have one |
| `#[derive(Debug, Clone, PartialEq)]` | Standard derives don't break |
| Trivial constructors (`OrderId::new(s)`) | It's a newtype wrapping a String |
| Generated gRPC stubs | tonic codegen is tested by tonic |
| `Display` impls | Unless the format is contractual (parsed elsewhere) |
| Private helper functions directly | Test through the public API that calls them |
| Config file parsing | Unless you wrote a custom parser — `toml::from_str` is tested upstream |

**Exception:** If a derive has a custom `#[serde(rename = ...)]` or `#[serde(default)]` that's contractual (e.g., wire format compatibility), test that specific attribute behavior.

---

## Verification Layers

Tests are one part of verification. The full layered approach:

| Layer | When | Command | Catches |
|-------|------|---------|---------|
| **Compiler** | Every edit | `cargo check` | Type errors, borrow violations, missing type-state transitions |
| **Targeted tests** | Every change | `cargo test -p {crate}` | Logic errors in changed code |
| **Clippy** | Before commit | `cargo clippy -p {crate} -- -D warnings` | Missing `#[must_use]`, non-exhaustive match, pedantic issues |
| **Full workspace** | Before commit | `make check` | Cross-crate breakage, formatting |
| **Property tests** | Risk/math changes | `cargo test -- --ignored` | Edge cases, panic paths |
| **Integration tests** | Bus/infra changes | `cargo test -p {crate} --test {suite} -- --ignored` | Cross-process behavior |
| **Hostile review** | Money-touching PRs | `/hostile-review` | Safety invariant violations |

Lower layers run in seconds. Only escalate to higher layers when the lower ones pass.

---

## Dependencies

| Crate | Purpose | Cargo.toml |
|-------|---------|------------|
| `proptest` | Property-based testing | `[dev-dependencies]` in `risk`, `types` |
| `trybuild` | Compile-fail tests | `[dev-dependencies]` in `types` |
| `insta` | Snapshot testing | `[dev-dependencies]` in `risk` |
| `tokio-test` | Async test utilities | `[dev-dependencies]` in `bus`, `engine-core` |

---

## Adding a New Test

Checklist for adding tests to a new feature:

1. **Unit tests** — cover the happy path and each error variant
2. **Compile-fail test** — if the feature adds a type-state boundary, verify the compiler rejects invalid transitions
3. **Snapshot test** — if the feature produces a `RiskOutcome` or other structured output, add a golden test
4. **Property test** — if the feature does financial math or risk evaluation, fuzz it
5. **Integration test** — if the feature crosses crate boundaries or needs external infra
6. **Regression test** — added retroactively when a bug is found (never proactively)

Not every feature needs all six. Use the testing budget table at the top to decide.

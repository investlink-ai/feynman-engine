# CLAUDE.md — feynman-engine

## Identity

You are a senior Rust systems engineer building the Feynman Capital execution engine — production trading infrastructure where correctness carries real financial consequences.

The feynman-engine is the Rust core that receives signals/orders from strategy clients via gRPC, evaluates them through a layered risk system, and submits orders to venues. It is always a **service** (never imported as a library). Strategies talk to it via gRPC in all modes. See `docs/STRATEGY_ENGINE_BOUNDARY.md`.

**Engine is 100% custom Rust** using per-exchange client crates. NautilusTrader was evaluated and rejected (2026-03-17). See `docs/CORE_ENGINE_DESIGN.md` (canonical architecture).

Read `AGENTS.md` for architecture, invariants, safety rules, and coding guidelines. Everything below is Claude Code workflow-specific.

---

## Development Commands

```bash
# Quick check (1-2 sec)
cargo check

# TDD loop (fast)
make quick

# Full test suite
cargo test --workspace

# Linting
cargo clippy --workspace -- -D warnings

# Format
cargo fmt

# Full gate (before commit)
make check

# Run engine
cargo run --bin feynman-engine -- --config config/default.toml
FEYNMAN_MODE=paper cargo run --bin feynman-engine   # paper mode
FEYNMAN_MODE=backtest cargo run --bin feynman-engine # backtest mode

# Debug logging
RUST_LOG=debug cargo run --bin feynman-engine
```

---

## Git Workflow

```bash
# Start work on an issue
git fetch origin main
git worktree add ../feynman-engine-{slug} -b feat/{slug} origin/main
cd ../feynman-engine-{slug}

# Before committing
make check

# Push and PR
git push -u origin feat/{slug}
gh pr create --draft --base main
```

**Branch naming:** `feat/`, `fix/`, `ops/`, `research/`

### PR Communication

- Put verification evidence in the PR body, commit message context, or direct user handoff summary.
- Do **not** post raw testing logs or verification summaries as PR comments unless the user explicitly asks for a comment.

---

## Rust Standards (non-negotiable)

| Rule | What it means |
|------|---------------|
| **Decimal only** | `rust_decimal::Decimal` for all prices, quantities, PnL. Zero `f64` in financial logic. |
| **thiserror in libs** | `risk`, `bus`, `gateway`, `engine-core`, `observability`, `api` define typed errors with `thiserror`. |
| **anyhow in bins** | `bins/feynman-engine` and integration tests use `anyhow::Result`. No `Box<dyn Error>`. |
| **`#[must_use]`** | On every fn returning `RiskApproval`, `OrderAck`, `Result`. Caller must handle it. |
| **Type-state for pipeline** | Pipeline stages (`Draft → Validated → RiskChecked → Routed → LiveOrder`) encoded in the type system. Venue lifecycle uses runtime `VenueState` enum with exhaustive match. See DATA_MODEL.md §3. |
| **Sealed traits** | `CircuitBreaker`, `VenueAdapter` are sealed — external crates cannot implement them. |
| **`impl Trait` in hot paths** | Static dispatch for Sequencer inner loop. `Box<dyn Trait>` only at wiring boundaries. |
| **`pub(crate)` default** | Internal APIs use `pub(crate)`. `pub` only for crate contract surface. |
| **Newtypes everywhere** | `OrderId`, `AgentId`, `VenueId` — never raw `String` parameters for domain IDs. |
| **`From`/`TryFrom` only** | No standalone `to_foo()`/`from_foo()`. Use standard conversion traits. |
| **Builder for complex configs** | Any struct with 3+ optional fields uses a builder with validation in `build()`. |
| **No `.clone()` in hot path** | Sequencer loop: borrow, don't clone. Clone once at async task boundaries. |
| **Clippy pedantic** | `[lints.clippy] pedantic = "warn"` in all financial crates. |
| **Exhaustive match** | No `_` wildcard on enums you own. Every new variant is a compile error. |

See `AGENTS.md §4` for code examples for each rule.

---

## Operating Principles

- **Architecture first.** Before writing code, check if the change touches a safety boundary (risk evaluation, order submission, state mutation). If it does, understand the failure modes first.
- **Explicit over implicit.** No silent fallbacks, no hidden defaults. `Err(...)` not defaults. See Design Principle #11 in `docs/CORE_ENGINE_DESIGN.md`.
- **Learn automatically.** When a bug is fixed or an assumption was wrong, extract the pattern to memory files.

| Auto-learning trigger | Target file |
|---|---|
| Bug fixed → root cause | memory `anti-patterns.md` |
| Design flaw / wrong assumption | memory `evolution-log.md` |

- **Money-touching code** (risk, engine-core, gateway, api, bins/feynman-engine) → run `/hostile-review` before finalizing.

---

## Key Documents

| Document | Status | What |
|----------|--------|------|
| `AGENTS.md` | **READ FIRST** | Architecture, invariants, safety rules, coding guidelines |
| `docs/DEV_WORKFLOW.md` | Canonical | Full dev loop: issue → branch → verify → PR → merge |
| `docs/TESTING.md` | Canonical | Testing strategy: compile-fail, snapshots, property, fixtures |
| `docs/CORE_ENGINE_DESIGN.md` | Canonical | Full architecture: types, venues, pipeline, risk, concurrency |
| `docs/STRATEGY_ENGINE_BOUNDARY.md` | Canonical | Strategy ↔ engine contract, client types, deployment modes |
| `.claude/rules/risk-touching.md` | Active | Auto-loaded gate for risk/execution code edits |
| `PHASE_0_CHECKLIST.md` | Current sprint | Tasks and exit criteria |
| `docs/HYBRID_ENGINE_ARCHITECTURE.md` | DEPRECATED | NautilusTrader hybrid (rejected, kept for history) |

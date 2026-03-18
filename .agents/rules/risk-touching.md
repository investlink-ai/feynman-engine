---
description: Pre-implementation gate for risk evaluation and order execution code
globs:
  - "crates/risk/src/**"
  - "crates/engine-core/src/**"
  - "crates/gateway/src/**"
  - "crates/api/src/**"
  - "bins/feynman-engine/src/**"
---

# Risk / Execution Code Gate

You are editing code that evaluates trades or submits orders. Answer all questions before writing a single line:

1. **Decimal math** — Is every price, quantity, and PnL calculation using `rust_decimal::Decimal`? No `f64` anywhere in financial logic.
2. **Fail-fast** — Does invalid input (conviction out of range, negative notional, missing required fields) immediately return `Err(...)`? No silent clamping or defaulting. Note: `stop_loss` is required for `SubmitSignal` but optional for `SubmitOrder`/`SubmitBatch`.
3. **dryRun guard** — Is `config.dry_run` checked before any exchange call? Every venue submission path must check this.
4. **Idempotency** — If this code runs twice (restart, retry), does it produce a duplicate order? Use `clientOrderId` or check existing state first.
5. **Error propagation** — Are venue errors returned to the caller with `?` or `Err(...)`? No `unwrap()` that swallows failures silently.
6. **Per-agent isolation** — Do risk checks enforce per-agent budget limits, not just firm-level limits? One agent's loss must not reduce another's allocation.
7. **Pipeline order preserved** — Does this change maintain Grouper → Sizing → Risk → ExecutionController sequence? Risk gate must remain before submission.
8. **Explicit over implicit** — Are there any silent defaults, fallback chains, or `unwrap_or_default()`? Every code path must be explicit. No silent emulation of unsupported order types.
9. **Sequencer ownership** — Does this code mutate business state (positions, firm book, risk limits)? If so, it must run inside the Sequencer task — not in a gRPC handler or adapter task behind a lock.
10. **Bounded channels** — Are all new channels bounded with explicit capacity? No `mpsc::unbounded_channel()`.

## Invariants (never violate)

- `Decimal` only for all financial math — no `f64`/`f32`
- Fail-fast: `anyhow::bail!()` on invalid input, never return default values silently
- No state mutation before fill confirmation from venue
- `execute_order` / venue submission only after `RiskApproval` from `AgentRiskManager`
- Per-agent budget isolation verified before every order
- No `Arc<Mutex<T>>` or `Arc<RwLock<T>>` on business state — Sequencer owns it
- No `mpsc::unbounded_channel()` — all channels bounded
- No silent fallbacks — `Err(...)` not default substitution
- Emulation is opt-in only (`allow_emulation: true`), never automatic

## Before finalizing

Run `/hostile-review` on this file before marking the task done.

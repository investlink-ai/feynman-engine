---
description: Hostile review for risk evaluation and execution code in the feynman-engine
argument-hint: "[crate or file to review]"
allowed-tools:
  - Read
  - Grep
  - Glob
---

# /hostile-review

Review the specified crate or file as a hostile senior Rust engineer who has seen real money lost to subtle bugs. Report every issue that could cause an unintended trade, silent failure, incorrect position state, or panic in production.

## Review Checklist (10 points)

1. **Decimal math** — Every price, quantity, PnL, and percentage uses `rust_decimal::Decimal`. No `f64`, `f32`, or integer division for money.
2. **Fail-fast on invalid input** — Missing stop loss, conviction out of range, negative notional → `anyhow::bail!()` immediately. No silent defaults or clamping.
3. **dryRun guard** — Every venue submission call site checks `config.dry_run` before sending to exchange.
4. **Idempotency** — Order submissions use `clientOrderId` or check existing state. Restarts/retries cannot produce duplicate orders.
5. **Error propagation** — Venue errors surface to the caller via `?` or explicit `Err(...)`. No `unwrap()` without a doc comment. No silently returning `Ok(())` on failure.
6. **No state mutation before confirmation** — Portfolio/position state is updated only after exchange ACK, never optimistically.
7. **Per-agent isolation** — Risk checks use per-agent limits (`AgentRiskLimits`), not just firm-level limits. Budget checks are per-agent.
8. **Pipeline order** — Grouper → Detector → Sizing → Risk (AgentRiskManager) → ExecutionController. Risk gate before submission, always.
9. **No panics in hot path** — No `unwrap()`, `expect()`, or `panic!()` in order submission or risk evaluation without explicit justification.
10. **Logging completeness** — Every order attempt (approved, rejected, dry_run skip, venue error) is logged with `tracing` at appropriate level with enough context to reconstruct the event.

## Output Format

For each issue found:

```
[P0|P1|P2] [crate/file:line] Issue title
Root cause: ...
Production risk: ...
Fix: ...
```

**P0** = blocks deploy (unintended trade or silent data corruption possible)
**P1** = fix before merging (incorrect position state or error swallowed)
**P2** = log and defer (quality issue, not immediately dangerous)

End with: `X issues (Y P0, Z P1, W P2)`. If zero issues: `LGTM — risk pipeline integrity preserved.`

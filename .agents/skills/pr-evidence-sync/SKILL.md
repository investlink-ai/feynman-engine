---
description: Sync acceptance criteria and evidence between GitHub issue and PR body
argument-hint: "[PR number or issue number]"
allowed-tools:
  - Read
  - Grep
  - Glob
  - Bash
---

# /pr-evidence-sync

Use before marking a PR ready for review. Ensures the PR body reflects completed acceptance criteria with evidence.

## Workflow

1. Read the linked GitHub issue body (`gh issue view <N>`).
2. Read the current PR body (`gh pr view`).
3. For each acceptance criterion in the issue:
   - Find the corresponding evidence (test output, file:line, `cargo test` result).
   - Mark it checked in the PR body with the evidence attached.
4. If any AC is not yet completed, leave it unchecked and note what's missing.
5. Update the PR body with `gh pr edit --body "..."`.

## Evidence Format

```markdown
## Acceptance Criteria

- [x] `AgentRiskManager::evaluate` returns `Err` when signal missing stop loss
  — `crates/risk/src/lib.rs:42`, test: `test_signal_without_stop_loss_rejected` ✓
- [x] All risk checks present and tested (universal + signal-specific)
  — `cargo test -p risk` → tests pass
- [ ] Property tests pass under stress (pending)
```

## Rules

- PR body ACs must mirror the issue ACs exactly.
- Never mark an AC checked without citing the file:line or test name.
- If scope changed during implementation, update both issue and PR body.
- Comments are for brief status only — put evidence in the PR body.

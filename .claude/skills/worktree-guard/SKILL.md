---
description: Guard against editing the main dev checkout — verify issue worktree before any code changes
argument-hint: "[issue number and slug if known]"
allowed-tools:
  - Read
  - Grep
  - Glob
  - Bash
---

# /worktree-guard

Run before any implementation task in feynman-engine.

## Hard Rule

If the current session started in the main checkout on branch `dev`, do not edit files. Start a fresh session from the issue worktree instead.

Shell-level `cd` is not a sufficient safety guarantee for later edits.

## Workflow

1. Check `pwd`.
2. Check `git branch --show-current`.
3. If the path ends in `feynman-engine` (main checkout) and branch is `dev`, **stop**.
4. If a worktree does not exist yet, create it from the main checkout:
   ```bash
   git fetch origin dev
   git worktree add ../feynman-engine-{slug} -b feat/{slug} origin/dev
   ```
5. Open or resume the session from the worktree path.
6. Re-check `pwd` and `git branch --show-current` in the new session before editing anything.

## Output

Return one of:

- `SAFE: session is in issue worktree at ../feynman-engine-{slug} on branch feat/{slug}`
- `STOP: session started in main checkout on dev; restart from worktree before editing`

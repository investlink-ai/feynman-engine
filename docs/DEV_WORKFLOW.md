# Dev Workflow — feynman-engine

**Version:** 1.0.0
**Last Updated:** 2026-03-18

One dev, AI-assisted. GitHub Issues hold status and task memory. Every implementation is verified before commit, and docs stay in sync with code at all times.

**Companion docs:**
- **[DEVELOPMENT.md](./DEVELOPMENT.md)** — Command reference cheat sheet (when you need to know *how* to run `cargo test`)
- **[TESTING.md](./TESTING.md)** — Testing strategy (compile-fail tests, snapshots, property tests, fixtures)

---

## The Loop

```
Pick issue → branch → implement → verify → commit → PR → CI green → review → merge
```

Every issue follows this loop. The only thing that changes is what fills the issues: scaffold, pipeline, venue adapters, API, shadow mode, cutover.

---

## 1. Issue Pickup

### Fetch and understand

```bash
# Read the issue
gh issue view {N}

# Check dependencies — are blockers closed?
gh issue view {N} --json body | jq -r '.body' | grep -i "blocked by"

# Check milestone context
gh issue list --milestone "Phase 0: Scaffold" --state open
```

### Before writing code

1. **Read the issue body** — understand scope, acceptance criteria, and out-of-scope items
2. **Read referenced docs** — every issue links to canonical specs. Read them.
3. **Read existing code** — understand what's already there before changing anything
4. **Check if money-touching** — if the issue touches `risk`, `engine-core`, `gateway`, `api`, or `bins/feynman-engine`, the risk-touching gate applies (`.claude/rules/risk-touching.md`)

---

## 2. Branch

```bash
# Create branch from main
git fetch origin main
git checkout -b feat/{slug} origin/main

# Verify
git branch --show-current  # Should print feat/{slug}
```

**Branch naming:** `feat/`, `fix/`, `ops/`, `research/` — match the slug to the issue title.

---

## 3. Implementation Plan

Before writing code, produce a short implementation plan. This is not optional for issues touching more than one file.

### Plan structure

```markdown
## Plan for #{N}: {title}

### Changes
1. {file_path} — {what changes and why}
2. {file_path} — {what changes and why}

### Type-state / Safety impact
- Does this touch pipeline order types? {yes/no → which stages}
- Does this touch risk evaluation? {yes/no → which checks}
- Does this mutate business state? {yes/no → must be in Sequencer}

### Test strategy
- Unit tests: {what to test, in which module}
- Integration tests: {what to test, in which file}
- Property tests: {what invariants to fuzz}

### Docs to update
- {doc_path} — {what section and why}
```

For trivial changes (single-file, no safety boundary), skip the plan — just implement and verify.

---

## 4. Implementation

### Principles

- **Smallest correct change** — satisfy the acceptance criteria, nothing more
- **Architecture first** — if the change touches a safety boundary (risk, order submission, state mutation), understand failure modes before writing code
- **Explicit over implicit** — no silent fallbacks, no hidden defaults
- **Read before write** — never modify code you haven't read in this session

### Rust standards (non-negotiable)

These are enforced by clippy, tests, and review. Violations block merge.

| Rule | Enforcement |
|------|-------------|
| `Decimal` only for financial math | clippy + `/hostile-review` |
| `thiserror` in library crates | code review |
| `anyhow` in bins and integration tests | code review |
| `#[must_use]` on approval/ack returns | clippy pedantic |
| Type-state pipeline stages | compiler (invalid transitions won't compile) |
| Sealed traits for safety-critical interfaces | code review |
| `pub(crate)` by default | clippy pedantic |
| Newtypes for domain IDs | code review |
| Bounded channels only | `/hostile-review` |
| Exhaustive match (no `_` wildcard on owned enums) | clippy + code review |

Full reference: `docs/CODING_GUIDELINES.md`

### Money-touching code gate

Before writing a **single line** in `risk/`, `engine-core/`, `gateway/`, `api/`, or `bins/feynman-engine/`, answer all 11 questions in `.claude/rules/risk-touching.md`. This gate auto-loads when editing those paths.

---

## 5. Verification

Verification is the core of this workflow. Every change goes through a layered verification process before it can be committed.

### Layer 1: Compile check (seconds)

Run `cargo check` after every meaningful edit. Catches type errors, missing imports, borrow violations.

### Layer 2: Targeted tests (seconds)

Run `cargo test -p {crate}` after each logical change. This is your TDD inner loop.

### Layer 3: Clippy (seconds)

Run `cargo clippy -p {crate} -- -D warnings` before committing. Catches pedantic issues, unused variables, missing `#[must_use]`, non-exhaustive matches.

### Layer 4: Full workspace (10-30 seconds)

```bash
make check
```

Run before every commit. This is the commit gate. If `make check` fails, the commit does not happen.

### Layer 5: Property tests (minutes, on demand)

Run `cargo test -- --ignored` when changing risk evaluation logic, financial math, or type conversions. Property tests verify that no combination of valid inputs causes panics or invariant violations.

### Layer 6: Integration tests (requires infrastructure)

Run when changing bus or infrastructure code:

```bash
# Current bus verification (the bus crate currently exposes unit tests only)
cargo test -p bus

# If a Redis-backed bus integration suite is added under tests/integration/,
# register it from crates/bus/Cargo.toml and then run:
docker run -d --name redis-test -p 6379:6379 redis:7-alpine
cargo test -p bus --test {suite} -- --ignored
docker rm -f redis-test

# Docker smoke test
./scripts/smoke-test.sh
```

### Layer 7: Hostile review (money-touching only)

Run `/hostile-review` in Claude Code before marking any money-touching PR as ready. Checks all 11 invariants from risk-touching.md.

### Verification summary

| Layer | When | Blocks |
|-------|------|--------|
| Compile | Every edit | Next edit |
| Targeted test | Every change | Next change |
| Clippy | Before commit | Commit |
| Full workspace (`make check`) | Before commit | Commit |
| Property tests | Risk/math changes | Commit |
| Integration tests | Bus/infra changes | PR |
| Hostile review (`/hostile-review`) | Money-touching PRs | Merge |

See [DEVELOPMENT.md](./DEVELOPMENT.md) for exact command syntax and [TESTING.md](./TESTING.md) for detailed guidance on each test type.

---

## 6. Testing Strategy

See **[TESTING.md](./TESTING.md)** for the full testing strategy: test types (unit, compile-fail, snapshot, property, integration, regression), fixtures, naming conventions, what not to test, and verification layers.

**Key points:**
- **Compile-fail tests** (`trybuild`) verify the type-state pipeline rejects invalid transitions at compile time
- **Snapshot tests** (`insta`) capture canonical risk evaluation outputs — diffs catch regressions instantly
- **Property tests** (`proptest`) fuzz risk evaluation and financial math — no valid input should panic
- **Regression tests** are mandatory with every bug fix — the test stays forever
- **Test fixtures** use builders with sensible defaults — override only what the test cares about
- **Don't test derives, generated code, or trivial constructors** — spend the budget on risk logic

---

## 7. Doc Sync

Documentation must stay in sync with code. Drift between docs and implementation is treated as a bug.

### When to update docs

| Change type | Docs to check |
|-------------|---------------|
| New type or enum variant | `docs/DATA_MODEL.md` |
| New or changed trait signature | `docs/CONTRACTS.md` |
| Risk rule added/changed | `docs/CORE_ENGINE_DESIGN.md §6`, `PHASE_0_CHECKLIST.md` |
| Pipeline stage added/changed | `docs/CORE_ENGINE_DESIGN.md §4.10`, `docs/DATA_MODEL.md §3` |
| gRPC RPC added/changed | `proto/service.proto`, `docs/CONTRACTS.md §8` |
| New crate or module | `docs/DEVELOPMENT.md` (project layout), `CLAUDE.md` |
| Architecture decision | `docs/CORE_ENGINE_DESIGN.md`, memory files |

### How to verify doc sync

Before PR, ask: "If someone reads only the docs, will they have an accurate picture of the system?" If not, update the docs.

Use `/docs-sync` in Claude Code to auto-check which docs need updating after a code change.

### Doc hierarchy

| Document | Authority | When to update |
|----------|-----------|----------------|
| `docs/CORE_ENGINE_DESIGN.md` | Canonical architecture | Architecture changes |
| `docs/DATA_MODEL.md` | Canonical types | Type changes |
| `docs/CONTRACTS.md` | Canonical interfaces | Trait/RPC changes |
| `CLAUDE.md` | AI assistant instructions | Workflow/rule changes |
| `AGENTS.md` | Architecture + coding rules | Pattern/invariant changes |
| `PHASE_0_CHECKLIST.md` | Current sprint status | Task completion |

---

## 8. Commit

### Pre-commit gate

```bash
# Must pass before every commit
make check
```

If `make check` fails, fix the issue first. Never `--no-verify`.

### Commit message format

```
{type}: {short description}

{optional body — what and why, not how}
```

Types: `feat`, `fix`, `refactor`, `test`, `docs`, `ops`, `chore`

Examples:
```
feat: add PipelineOrder<S> type-state markers

Implement Draft, Validated, RiskChecked, Routed stage types
with consuming transition methods. Invalid stage transitions
are now compile-time errors.
```

```
fix: risk evaluation uses full notional when stop_loss is None

Previously the worst-case loss calculation silently defaulted
to zero when stop_loss was missing on SubmitOrder path. Now
uses full notional as worst-case, matching the spec in
CORE_ENGINE_DESIGN.md §6.
```

### What to commit

- Commit logical units of work, not "end of day" dumps
- Each commit should compile and pass `cargo check` at minimum
- Tests and implementation go in the same commit when they're for the same feature

---

## 9. Pull Request

### Creating

```bash
git push -u origin feat/{slug}
gh pr create --draft --base main --title "{title}" --body "$(cat <<'EOF'
## Summary
{1-3 bullet points}

Closes #{N}

## Verification
- [ ] `make check` passes
- [ ] {specific test command and output}
- [ ] {money-touching: `/hostile-review` run — PASS}

## Doc sync
- [ ] {docs updated, or "no doc changes needed"}

## Test evidence
```
{paste relevant test output}
```
EOF
)"

# When ready for review
gh pr ready
```

### PR body requirements

Every PR must include:
1. **Summary** — what changed and why (link to issue)
2. **Verification** — what commands were run and passed
3. **Doc sync** — which docs were updated (or why none needed updating)
4. **Test evidence** — paste actual test output showing the change works

Use `/pr-evidence-sync` to keep issue ACs and PR verification in sync.

### Review checklist (self-review before marking ready)

- [ ] `make check` passes locally
- [ ] Acceptance criteria from the issue are all met
- [ ] No unrelated changes included
- [ ] Docs updated if behavior changed
- [ ] Money-touching code: `/hostile-review` run and PASS

---

## 10. CI/CD

CI runs on every push to `main` or `dev` and on every PR.

### What CI checks

```yaml
- cargo check --workspace
- cargo fmt --check
- cargo clippy --workspace -- -D warnings
- cargo test --workspace
```

### CI failure protocol

1. Read the failure log — identify the **first** real failure
2. Fix locally — don't guess-and-push
3. Run the same check locally to confirm the fix
4. Push the fix as a new commit (don't amend unless the PR is still draft)

---

## 11. Human Gates

### Money-touching code

Before merging any PR that touches `risk/`, `engine-core/`, `gateway/`, `api/`, or `bins/feynman-engine/`:

- [ ] All 11 risk-touching gate questions answered
- [ ] `/hostile-review` returns PASS
- [ ] No `f64` in financial math
- [ ] No `unwrap()` outside tests
- [ ] No `Arc<Mutex<T>>` on business state
- [ ] No `mpsc::unbounded_channel()`
- [ ] Fail-fast on invalid input — no silent defaults

### Live ops changes (post-cutover)

Any change to a running system with real capital:

- [ ] What's the worst case if this is wrong?
- [ ] Is there a rollback path?
- [ ] Test on paper mode first

---

## 12. Post-Implementation

### After merge

```bash
# Switch back and update
git checkout main
git pull origin main

# Clean up branch
git branch -d feat/{slug}
```

### Learn from the work

After non-trivial bug fixes or wrong assumptions, run `/evolve` to extract patterns:

| Trigger | Target |
|---------|--------|
| Bug fixed → root cause | `memory/anti-patterns.md` |
| Design flaw / wrong assumption | `memory/evolution-log.md` |
| New review gap discovered | `.claude/rules/risk-touching.md` or relevant skill |

### Update issue

Close the issue if the PR body includes `Closes #N`. Otherwise:

```bash
gh issue close {N} --reason completed
```

---

## 13. Working with AI Assistants

### Session discipline

- One issue per session. Don't mix tasks.
- Start every session by reading the issue: `gh issue view {N}`
- Re-read referenced docs before implementing
- If a session is interrupted, re-read the issue and PR before continuing

### Delegation

| Task | Tool |
|------|------|
| Implementation | Claude Code (primary) |
| Architecture review | Claude Code with `/hostile-review` |
| Code search, exploration | Subagents for broad searches |
| Quick checks | Direct tool calls (Grep, Glob, Read) |

### What persists across sessions

- **GitHub Issue body** — task contract, ACs, blockers
- **PR body** — delivery record, test evidence
- **Memory files** — durable patterns, anti-patterns, decisions
- **Docs** — canonical specs

If the next session must know it, write it to one of these surfaces.

### Skills reference

| Skill | When to use |
|-------|-------------|
| `/hostile-review` | Before finalizing money-touching code |
| `/docs-sync` | After behavior or architecture changes |
| `/pr-evidence-sync` | Before marking PR ready |
| `/evolve` | After non-trivial bug fixes or wrong assumptions |

---

## Quick Reference

```bash
# === Issue pickup ===
gh issue view {N}

# === Branch ===
git fetch origin main
git checkout -b feat/{slug} origin/main

# === Verify (layered) ===
make check                               # Full gate: compile, lint, test, format

# === Commit ===
git add {files}
git commit -m "feat: ..."

# === PR ===
git push -u origin feat/{slug}
gh pr create --draft --base main --title "..." --body "Closes #N"
gh pr ready

# === After merge ===
git checkout main && git pull origin main
git branch -d feat/{slug}
```

See [DEVELOPMENT.md](./DEVELOPMENT.md) for all command details (testing, building, debugging, Docker, etc.).

**Money-touching PR:** `/hostile-review` + 11-point gate, then approve.
**Ops PR:** verify worst case + rollback path, then approve.
**Everything else:** `make check` green + review clean = merge.

# Codex Configuration

This directory stores Codex-specific configuration for the feynman-engine workspace. **Kept in parity with `.claude/`** (Claude Code configuration).

## Structure

**Configuration:**
- `settings.json` — Codex hooks and permissions (based on Claude Code format — **may need Codex-specific adjustments**)
- `rules/risk-touching.md` — Auto-loaded rule when editing risk/execution code

**Custom Skills (in parity with Claude Code):**
- `skills/hostile-review/` — Critical review of money-touching code before finalization
- `skills/evolve/` — Structured evolution of design patterns and improvements
- `skills/docs-sync/` — Synchronize architecture docs with code changes
- `skills/pr-evidence-sync/` — Sync PR acceptance criteria with evidence
- `skills/worktree-guard/` — Isolate work in worktrees

## Documentation (in docs/ directory)

See `docs/` for reference documentation (shared between Codex and Claude Code):
- `docs/CODING_GUIDELINES.md` — Rust patterns, style, error handling, testing
- `docs/SAFETY_RULES.md` — 12 non-negotiable safety rules with verification examples

## Parity with Claude Code

Both `.agents/` (Codex) and `.claude/` (Claude Code) should have:
- Equivalent configuration files (settings, rules)
- Access to same reference documentation in `docs/`
- Equivalent settings/permissions configuration

This allows both AI systems to work with the same codebase using their native configuration formats.

## Key Project Files

**Project Root:**
- `AGENTS.md` — Canonical architecture, invariants, safety rules, coding guidelines (all AI agents)
- `CLAUDE.md` — Claude Code workflow

**Documentation (canonical):**
- `docs/CORE_ENGINE_DESIGN.md` — Full system architecture
- `docs/SYSTEM_ARCHITECTURE.md` — System topology and deployment
- `docs/CONTRACTS.md` — Trait interfaces and gRPC API

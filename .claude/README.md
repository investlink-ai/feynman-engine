# Claude Code Configuration

This directory stores Claude Code-specific configuration and custom skills.

## Structure

**Configuration (this directory):**
- `settings.json` — Hooks (risk code warning, test failure extraction) and permissions
- `settings.local.json` — Local permissions (web search, crates.io, GitHub)

**Rules (auto-loaded by Claude Code):**
- `rules/risk-touching.md` — Auto-loaded gate when editing risk/execution code

**Custom Skills:**
- `skills/` — Claude Code custom skills:
  - `/hostile-review` — Critical review of money-touching code before finalization
  - `/evolve` — Structured evolution of design patterns and improvements
  - `/docs-sync` — Synchronize architecture docs with code changes
  - `/pr-evidence-sync` — Sync PR acceptance criteria with evidence
  - `/worktree-guard` — Isolate work in git worktrees

## Documentation (in docs/ directory)

See `docs/` for reference documentation:
- `docs/CODING_GUIDELINES.md` — Rust patterns, style, error handling, testing
- `docs/SAFETY_RULES.md` — 12 non-negotiable safety rules with verification examples

## Key Project Files

**Project Root:**
- `CLAUDE.md` — Claude Code workflow: development commands, git, operating principles, Rust standards
- `AGENTS.md` — Canonical architecture, invariants, safety rules, coding guidelines (for all AI agents)

**Documentation (canonical):**
- `docs/CORE_ENGINE_DESIGN.md` — Full system architecture (canonical)
- `docs/SYSTEM_ARCHITECTURE.md` — System topology, deployment, component architecture
- `docs/STRATEGY_ENGINE_BOUNDARY.md` — Strategy ↔ engine contract
- `docs/CONTRACTS.md` — Trait interfaces and gRPC API contracts
- `docs/DATA_MODEL.md` — Type definitions and state machines
- `docs/MIGRATION_PLAN.md` — Phased migration plan with gates

# Claude Code Configuration

This directory stores Claude Code-specific configuration for the feynman-engine workspace.

## Structure

- `rules/risk-touching.md` — Auto-loaded gate when editing risk/execution code
- `skills/` — Custom skills (`/hostile-review`, `/worktree-guard`, `/evolve`, `/docs-sync`, `/pr-evidence-sync`)
- `settings.json` — Hooks (risk code warning, test failure extraction) and permissions
- `settings.local.json` — Local permissions (web search, crates.io, GitHub)

## Key Files (outside this directory)

- `CLAUDE.md` (project root) — Claude Code workflow: commands, git, auto-learning
- `AGENTS.md` (project root) — Architecture, invariants, safety rules, coding guidelines
- `docs/CORE_ENGINE_DESIGN.md` — Canonical architecture
- `docs/STRATEGY_ENGINE_BOUNDARY.md` — Strategy ↔ engine contract

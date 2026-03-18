---
description: Update the correct engine docs when behavior, architecture, or workflow changes
argument-hint: "[short description of the code or design change]"
allowed-tools:
  - Read
  - Grep
  - Glob
  - Edit
  - Write
---

# /docs-sync

Use when code or design changes make existing docs stale. Update only what changed.

## Doc Routing

| Doc | Update when... |
|-----|---------------|
| `docs/HYBRID_ENGINE_ARCHITECTURE.md` | Architecture decisions, NautilusTrader integration, migration phases |
| `docs/CORE_ENGINE_DESIGN.md` | Type system, crate interfaces, risk layers, pipeline stages |
| `docs/DEV_WORKFLOW.md` | Development process, issue workflow, verification steps |
| `docs/TESTING.md` | Testing strategy, test types, fixtures |
| `PHASE_0_CHECKLIST.md` | Phase 0 tasks completed, exit criteria met |
| `DEVELOPMENT.md` | Setup steps, build/test commands, debugging procedures |
| `QUICKSTART.md` | First-run experience changes |
| `README.md` | Public-facing project overview, API endpoints, quick start |
| `.claude/rules/risk-touching.md` | New invariants discovered, safety rules refined for money-touching code |
| `AGENTS.md` | Architecture patterns, safety rules, coding guidelines |
| `CLAUDE.md` | AI assistant workflow, development standards |

## Workflow

1. Identify what changed in behavior, design, or workflow.
2. Find the affected doc(s) using the routing table above.
3. Update only the stale sections — do not rewrite unchanged content.
4. Keep wording aligned with actual code behavior (not aspirational).

## Rules

- Never duplicate the same policy across multiple docs — add a pointer instead.
- If a phase milestone is completed, check off the item in `PHASE_0_CHECKLIST.md`.
- Architecture docs are time-invariant descriptions — remove outdated rationale rather than appending.

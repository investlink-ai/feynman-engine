---
description: Post-task reflection — extract learnings to memory files
argument-hint: "[optional: brief description of the task just completed]"
allowed-tools:
  - Read
  - Write
  - Edit
  - Glob
---

# /evolve

Run after completing a non-trivial task. Extract learnings before closing the conversation.

## Trigger Rules

| Event | Action | Target file |
|---|---|---|
| Bug fixed | Extract root cause pattern | auto-memory `anti-patterns.md` |
| Design flaw discovered | Log observation + what was missed | auto-memory `evolution-log.md` |
| Assumption was wrong | Log what was assumed vs. reality | auto-memory `evolution-log.md` |
| Recurring pattern emerged | Document the pattern | auto-memory `evolution-log.md` |
| Review gap found | Add check to checklist | auto-memory `review-checklist.md` |

## Workflow

1. **Reflect** — What was harder than expected? What assumption was wrong? What pattern emerged?
2. **Classify** — Which trigger(s) apply?
3. **Write** — Update the relevant memory file(s). One entry = one insight.
4. **No duplicates** — Check the file first; update an existing entry if it's related.

## Entry Format

### anti-patterns.md entry
```
## [Short title] (YYYY-MM-DD)
**Symptom:** What went wrong
**Root cause:** Why it happened
**Fix:** What prevents recurrence
**Crates:** Where this matters (e.g., `agent-risk`, `pipeline`)
```

### evolution-log.md entry
```
## [Short title] (YYYY-MM-DD)
**Context:** What was being built
**Finding:** What was discovered
**Impact:** What changes as a result
```

## Memory File Location

Auto-memory directory for this project (managed by Claude Code):
`/Users/feynman/.claude/projects/-Users-feynman-Documents-projects-feynman-engine/memory/`

Create the file if it doesn't exist. Keep each file under 300 lines.

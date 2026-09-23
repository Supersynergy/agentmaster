---
name: am
description: Ponytail master router for the agentmaster fleet — one entry point that routes to per-runtime control skills, the agentmaster CLI, or runtime passthrough. Use when the user says "am <cmd>", "fleet", "agentmaster", "orchestrate agents", "what's running", "peek agent", "assign task to worker".
---

# `am` — Ponytail Master Router

One bash script, ~80 lines, zero deps. Routes every fleet command to the right
place: `agentmaster` CLI, runtime CLI passthrough, or per-runtime control skill.

## Commands

```bash
am                                   # = am ls (fleet snapshot)
am ls [--json]                       # all live agents (cmux + tmux), status glyphs
am peek <ref> [--tail N]             # zero-tax read of an agent's transcript
am message <ref> <msg> [--goal N] [--dod D]   # alias: am send
am handoff <ref> <msg> [--wait 120]           # send + wait for next assistant reply
am assign <runtime> <task> [--name N]         # async fan-out, fresh cmux workspace
am broadcast "<msg>" [--tmux] [--needs-input]
am goal <name> <ziel> [:: dod]
am goals [--json]
am events [-n 50]
am doctor [--verbose] [--costs]
am skill <runtime>                           # print control-skill SKILL.md path + first heading
am <runtime> <args...>                       # passthrough: am codex --model gpt-5 …
```

## Runtime → control-skill map

| Runtime | Skill folder |
|---|---|
| `claude` | `~/superskills/universal/skills/claude-code` |
| `codex` | `~/superskills/universal/skills/codex` |
| `hermes` | `~/superskills/universal/skills/hermes-agent` |
| `opencode` | `~/superskills/universal/skills/opencode` |
| `gemini` | (no skill folder yet) |
| `aider` | (no skill folder yet) |
| `cline` | (no skill folder yet) |
| `ggcoder` | (no skill folder yet) |
| `shell` | (raw shell, no skill) |

`am skill <runtime>` prints the path dynamically — no hardcoded doc snippets
to drift. Missing skill → "no control skill registered", exit 1.

## Failsafe behavior

- Missing `am` binary → tell user to run `make install` in agentmaster.
- Unknown command → runtime passthrough if the name is on PATH, else exit 127.
- Missing skill folder → "no control skill registered for runtime '<rt>'".
- Missing SKILL.md → path + "(no SKILL.md — skill folder exists but is empty)".

## Install

```bash
ln -sf ~/superskills/universal/skills/agentmaster/am.sh ~/.local/bin/am
```

Or from the agentmaster repo:

```bash
cd agentmaster && make install-am
```

## Source

`~/superskills/universal/skills/agentmaster/am.sh` — ~80 lines of bash, zero deps.
The runtime→skill map is one `case` block; add a runtime = one line.

## See also

- `agentmaster --help` — the underlying CLI
- `.cursor/agents/agentmaster.md` — the supervisor subagent that uses `am`
- `~/BASE/projects/agentmaster/cursor-sdk/` — TypeScript SDK + MCP server

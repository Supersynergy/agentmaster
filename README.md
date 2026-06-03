# agentmaster

**One session to see and steer every agent — Kanban TUI, zero orchestration tax.**

agentmaster runs your coding agents (Claude Code, Codex, or any CLI/shell) behind
native PTYs and shows them on a live Kanban board: which agent, which project,
which process, what state — at a glance. No agent pays LLM tokens to coordinate;
all coordination is on-disk (SQLite) and all state is *observed*, not *reported*.

```
 agentmaster  ⛁ 9 agents   ● 3 working  ▲ 1 blocked  ◍ 1 review  ✓ 4 done    cpu 41%  mem 12.3/128.0G
┌ QUEUED 1 ┐┌ WORKING 3 ───────────┐┌ BLOCKED 1 ──────────┐┌ REVIEW 1 ┐┌ DONE 4 ──┐
│ ● codex  ││ ● claude   synapse    ││ ● codex   leads-eng  ││ ● claude ││ ● claude │
│ shell    ││ ⎇ synapse  pid 8821   ││ ⎇ leads   pid 9001   ││ events   ││ siteaudit│
│ ⏱ 2s     ││ ⏱ 4m  idle 1s working ││ ⏱ 12m  blocked       ││ review   ││ done     │
│ ▸ queued ││ ▸ running cargo nextes││ ▸ approve rm -rf? [y/N│└──────────┘└──────────┘
└──────────┘└──────────────────────┘└──────────────────────┘
 [1]kanban [2]tree [3]logs   lane:h/l/Tab  card:j/k  ↵inspect   n)ew s)end K)ill /filter ?help q)uit
```

## Why

Most multi-agent setups burn tokens on an LLM orchestrator that routes and
coordinates. agentmaster removes that tax three ways:

- **You are the router.** The board makes *where/what/which* obvious, so no LLM
  needs to narrate status.
- **Coordination is on-disk.** SQLite audit log + mail bus — queryable, free.
- **State is observed.** Status (working / blocked / review / done) is inferred
  from each agent's terminal output, exactly what a human sees — no self-report.

## Install / run

```bash
cargo build --release
./target/release/agentmaster        # launch the TUI (default)
agentmaster doctor                  # health check (pty, sqlite, runtimes)
agentmaster events -n 100           # headless: tail the audit log
```

Requires Rust 1.95+. Works on any terminal or bare Linux/macOS — agentmaster owns
the PTYs itself, so it needs no tmux/zellij/kitty cooperation.

## Keys

| Key | Action |
|-----|--------|
| `1` `2` `3` | kanban / tree / logs view |
| `h` `l` `Tab` | switch lane |
| `j` `k` | select card |
| `↵` | inspect agent (live tail + command + cwd) |
| `n` | new agent — `<runtime> [task]`, e.g. `shell` or `claude fix the bug` |
| `s` | send a line to the selected agent |
| `K` | kill selected agent |
| `/` | filter |
| `?` | help · `q` quit |

## Observability

Every state change and action is written to **two** sinks:

- `~/.agentmaster/agentmaster.db` — SQLite `events` table (queryable audit log)
- `~/.agentmaster/logs/agentmaster.jsonl` — structured JSON traces (`tracing`)

Filter the JSONL verbosity with `AGENTMASTER_LOG=debug`.

## Status

v0.1 — native-PTY backend, Kanban/Tree/Logs views, state detection, send/kill,
full audit + JSONL observability. See `docs/SPEC.md` for the roadmap (adapters for
tmux/cmux/rmux/kitty, SQLite mail bus, daemon + detach/reattach).

Built by Maxim Supersynergy.

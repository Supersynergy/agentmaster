# agentmaster — SPEC

## Goal

One session sees and steers every agent. The operator gets unambiguous,
at-a-glance answers to *where / what / which agents / which processes*, so no LLM
orchestrator is needed to route or narrate — that removes the orchestration tax.

## Principles

1. **Observed, not reported.** Agent state is inferred from terminal output (what
   a human reads), never from a token-costing self-report.
2. **On-disk coordination.** Audit log + mail bus live in SQLite — queryable,
   durable, free. Files are the bus, not API calls.
3. **Render is a pure function of state.** All mutation in `app.rs`; `ui.rs` only
   draws. Easy to reason about, easy to test.
4. **Universal floor.** A native PTY backend works on any terminal / bare Linux.
   Multiplexer adapters (tmux/cmux/rmux/kitty) are interop on top, never required.
5. **Visibility = the product.** The board IS the state. If you can see it
   clearly, you don't need to pay an agent to tell you.

## Architecture (v0.1, implemented)

```
main.rs        CLI: tui | doctor | events
 └ app.rs      App state + single-thread event loop + state machine
    ├ fleet.rs Agent, Status, Lane, Fleet
    ├ pty.rs   native PTY spawn + ANSI-stripping reader thread  ─┐ AppEvent
    ├ state.rs output → status heuristics (block/review/done)    │ over mpsc
    ├ runtime.rs runtime name → (program, args) adapter          │
    ├ store.rs SQLite events + mail (audit / coordination)       │
    ├ obs.rs   JSONL tracing + host metrics                      │
    └ ui.rs    Kanban / Tree / Logs / Inspect / Help (pure draw)─┘
```

Threading: one reader thread per agent streams clean lines over a channel; the
main thread drains the channel each tick, polls crossterm for keys, and redraws.

## Backend matrix (target)

| Class | Targets | Mechanism | Control |
|-------|---------|-----------|---------|
| Native PTY (**done**) | bare Linux, Ghostty, Alacritty, foot | `portable-pty` | full (owns pid) |
| Multiplexer | tmux · zellij · rmux · herdr · cmux | send-keys / SDK / socket + capture | full (attach + persist) |
| Terminal IPC | kitty · wezterm · iTerm2 | `kitten @` / `wezterm cli` / py-API | send + list |
| Spawn-only | Windows Terminal | `wt.exe` | start only |

## Roadmap

- **S2** — multiplexer adapters (rmux SDK first, then cmux/tmux); `↵` true attach;
  SQLite **mail bus** surfaced as a view; per-agent CPU/mem via `sysinfo`.
- **S3** — daemon + socket API (detach/reattach, agents survive the client);
  runtime adapters for codex/copilot/gemini; integrate `grepgod` (search across
  panes) and `sr find` (recall/restore distilled sessions).
- **S4** — lead→worker hierarchy (optional), git worktree isolation per worker,
  tiered merge.

## Non-goals (v0.1)

Full TTY passthrough/raw byte mirroring (send-line covers steering); cloud /
sandboxed agents; web UI. All deferred.

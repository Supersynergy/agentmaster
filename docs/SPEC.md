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

## Architecture (v0.17, implemented)

```
main.rs        CLI: tui | doctor | ls | send | broadcast | focus | goals | peek | batch
 └ app.rs      App state + event loop + cmux/tmux/native orchestration
    ├ fleet.rs Agent, Status, Lane, Fleet
    ├ backend.rs tmux + cmux discovery/control/capture
    ├ peek.rs    transcript resolution + zero-tax digest/title extraction
    ├ pty.rs     native PTY spawn + ANSI-stripping reader thread ─┐ AppEvent
    ├ state.rs   output → status/progress heuristics              │ over mpsc
    ├ runtime.rs runtime name → (program, args) adapter           │
    ├ store.rs   SQLite events + goals + seen-state               │
    ├ obs.rs     JSONL tracing + host metrics                     │
    └ ui.rs      List / Board / Tree / Logs / Inspect (pure draw)─┘
```

Threading: one reader thread per native agent streams clean lines over a channel;
backend refresh and `cmux events` run off-thread; the main thread drains events,
polls crossterm for keys, and redraws.

## Backend matrix (target)

| Class | Targets | Mechanism | Control |
|-------|---------|-----------|---------|
| Native PTY (**done**) | bare Linux, Ghostty, Alacritty, foot | `portable-pty` | full (owns pid) |
| Multiplexer (**tmux/cmux done**) | tmux · cmux | send-keys / RPC / capture / events | full for discovered tabs |
| Multiplexer (target) | zellij · rmux · herdr | SDK / socket + capture | full (attach + persist) |
| Terminal IPC | kitty · wezterm · iTerm2 | `kitten @` / `wezterm cli` / py-API | send + list |
| Spawn-only | Windows Terminal | `wt.exe` | start only |

## Roadmap

- **S2** — cost/token tracking from transcripts; subagent-tree parsing; worktree
  isolation where the backend provides per-agent repositories.
- **S3** — daemon + socket API (detach/reattach, agents survive the client);
  additional runtime adapters; richer mail-bus UI.
- **S4** — lead→worker hierarchy (optional), tiered merge, richer activity feed.

## Non-goals

Full TTY passthrough/raw byte mirroring (send-line + focus covers steering);
cloud-hosted agents; web UI; token-costing status self-reports.

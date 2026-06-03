# ADR 0001 — Single-binary, daemon-less, native-PTY TUI

- Status: accepted
- Date: 2026-06-03

## Context

We need to see and steer many agents from one session without paying an LLM
orchestrator to coordinate. Prior art: herdr/rmux (Rust multiplexers, server +
client), multi-agent-shogun (tmux hierarchy, on-disk coordination), overstory
(pluggable runtime adapter, SQLite mail bus). cmux (the existing local tool) is
fragile and has lost sessions on update.

## Decision

Build agentmaster as a **single Rust binary** that **owns the PTYs itself**
(`portable-pty`) and renders a Kanban TUI (`ratatui`). v0.1 is **daemon-less**:
one process, one thread for the event loop, one reader thread per agent.

- Transport floor = native PTY → works on any terminal / bare Linux. Multiplexer
  adapters (rmux/cmux/tmux) come later as interop, not as a dependency.
- Coordination + audit = **SQLite** (`store.rs`). Status = **observed** from
  output (`state.rs`), never self-reported.
- Render is a **pure function of `App`**; all mutation in `app.rs`.

## Why not a daemon now

Detach/reattach (the daemon's payoff) is real but not needed to prove the core
value — *seeing and steering* agents with zero token tax. A daemon adds a socket
protocol, lifecycle, and IPC surface. Deferred to S3 (SPEC), where adapters and
session-restore make it worthwhile. v0.1 stays small and obviously correct.

## Why steal concepts, not whole repos

- rmux → typed SDK transport (S2 adapter).
- herdr → agent-state awareness (implemented as output-based detection).
- shogun → on-disk, token-free coordination (SQLite, implemented).
- overstory → pluggable runtime adapter (`runtime.rs`, implemented) + mail bus
  (table present, view in S2). overstory is archived → learn, don't depend.

## Consequences

- Works everywhere immediately; no multiplexer setup.
- No cross-session persistence yet (client exit stops the agents' PTYs). Accepted
  for v0.1; daemon in S3.
- Linear scans over a `Vec<Agent>` — fine at the small N a human can watch.

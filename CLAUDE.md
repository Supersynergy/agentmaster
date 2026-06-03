# agentmaster — agent guide

Rust 1.95, edition 2024, single binary. Kanban TUI that owns native PTYs to see +
steer agents. Zero orchestration tax: state is **observed** from output, coordination
is **on-disk** (SQLite), never paid in tokens.

## Layout
- `src/main.rs` — CLI (`tui`/`doctor`/`events`/`find`/`dash`/`start`/`peek`/`batch`)
- `src/app.rs` — state + event loop + state machine (ALL mutation here)
- `src/ui.rs` — ratatui rendering (PURE function of `App`, no mutation)
- `src/fleet.rs` — Agent / Status / Lane / Fleet (+ goal/progress/transcript)
- `src/backend.rs` — tmux + cmux adapters (discover/capture/send/kill)
- `src/pty.rs` — native PTY spawn + ANSI-stripping reader thread
- `src/state.rs` — output → status heuristics + goal progress inference
- `src/runtime.rs` — runtime name → (program, args) adapter
- `src/store.rs` — SQLite audit log + mail bus + goals table
- `src/peek.rs` — zero-tax transcript digest (last user/assistant/next)
- `src/orch.rs` — orchestrator bridge: sr passthrough + batch fan-out
- `src/obs.rs` — JSONL tracing + host metrics

## Invariants (do not break)
- `ui.rs` never mutates state. Render = `f(App)`.
- Colors only via `status_color`/`kind_color` + the `C_*` constants. One meaning per color.
- Every state change + action → `store.log(...)` AND a `tracing::` event.
- Status is inferred from output (`state.rs`); never add a token-costing self-report path.
- Every dynamic string is `trunc()`-ed to the real cell width.
- See `docs/adr/0002-tui-best-practices.md` before touching the UI.

## Gates (run before "done")
`just check` (fmt + clippy -D warnings + build) for small edits; `just ci` for normal.
Keep clippy at 0 warnings. Update `CHANGELOG.md` for any user-visible change.

## Verify the TUI
Interactive, so drive it under a PTY (see the smoke test): spawn an agent, switch
views, quit; assert exit 0 + audit trail in `agentmaster events`.

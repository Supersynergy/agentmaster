# agentmaster — Agent Guide

agentmaster is a Rust 1.95+ TUI/CLI command center for local coding-agent
fleets: native PTYs, tmux panes and cmux workspaces. It optimizes for one human
seeing and steering many agents with zero orchestration-token tax.

## Commands

```bash
just check      # fmt check + clippy -D warnings + build
just ci         # doctor + check + tests + release build
cargo nextest run
cargo run -- doctor
cargo run       # launch TUI
```

## Hard Rules

- `src/ui.rs` renders only. Do not mutate application state from rendering code.
- `src/app.rs` owns event handling, state transitions, discovery and orchestration.
- Status must be observed from output, transcript state, cmux/tmux state, or local
  process state. Do not add token-costing self-report paths.
- Every user-visible behavior change needs focused tests and a `CHANGELOG.md`
  entry.
- Dynamic text in the TUI must be truncated or wrapped to the real cell width.
- Do not commit local runtime data: `target/`, `.agentmaster/`, transcripts,
  logs, generated scratch files.

## Important Files

- `src/app.rs` — event loop, filters, sorting, status transitions, cmux events.
- `src/ui.rs` — ratatui list/board/tree/log/detail rendering.
- `src/fleet.rs` — `Agent`, `Status`, `Lane`, persistence-facing fields.
- `src/backend.rs` — tmux/cmux discovery and control.
- `src/peek.rs` — transcript resolution and zero-token digest/title extraction.
- `src/store.rs` — SQLite audit log, goals and seen-state.
- `docs/adr/0002-tui-best-practices.md` — read before UI changes.

## Release Checklist

```bash
cargo fmt --check
cargo nextest run
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
./target/release/agentmaster --version
```

For TUI behavior, verify under a real PTY when possible. `cmux events` children
must not survive after quitting the TUI.

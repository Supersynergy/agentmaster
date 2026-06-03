# Changelog

All notable changes to agentmaster are documented here. Newest first.

## [0.1.0] — 2026-06-03

First working cut: a single-binary Kanban TUI that sees and steers agents over
native PTYs, with observability wired from the start.

### Added
- **Native PTY backend** (`portable-pty`): spawn any runtime (Claude Code, Codex,
  shell, or arbitrary program) under a pseudo-terminal — works on any terminal or
  bare Linux/macOS, no multiplexer required.
- **Kanban board**: five lanes = agent state (queued · working · blocked · review
  · done). Cards show runtime, project, pid, age, idle, last line.
- **Tree view**: agents grouped by project with status + pid + age.
- **Logs view**: live tail of the SQLite audit log (every state change + action).
- **Inspect mode**: full-screen live output tail for one agent, plus its exact
  command and cwd.
- **State detection** (`state.rs`): infers working / blocked / review / done from
  terminal output — zero token cost, sticky terminal states, idle sweep.
- **Actions**: new agent (`n`), send line (`s`), kill (`K`), filter (`/`).
- **Observability (dual sink)**: SQLite `events` audit log + structured JSONL
  traces (`tracing` + `tracing-appender`, `AGENTMASTER_LOG` filter).
- **CLI**: `tui` (default), `doctor` (pty/sqlite/runtime health), `events`
  (headless audit tail).
- TUI best practices baked in (see `docs/adr/0002-tui-best-practices.md`):
  consistent color semantics, single focus highlight, empty states, contextual
  footer, overflow-safe truncation, responsive layout, clean terminal restore.

### Notes
- Verified this session: `cargo build` clean, `cargo clippy --all-targets` 0
  warnings, 6/6 unit tests pass, PTY-driven smoke (spawn shell agent → render →
  quit) exits 0 with full audit + JSONL trail.

[0.1.0]: #010--2026-06-03

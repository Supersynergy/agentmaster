# Changelog

All notable changes to agentmaster are documented here. Newest first.

## [0.2.0] — 2026-06-03

Mouse control + motion-as-feedback. Informed by Universal UI principles (effects
must carry meaning — never decoration, never status-by-color-alone).

### Added
- **Mouse**: click a lane/card to focus + select, click the selected card again
  to inspect, scroll wheel to move the card cursor. Enabled by default.
- **`m` toggle**: drop mouse capture to hand the terminal back native text
  selection / copy, then re-enable — respects "selection wins by default".
- **Working spinner**: a braille spinner animates on working agents, so "this
  agent is alive and running" reads at a glance (motion mapped to meaning).
- **Status glyphs**: distinct shapes per state (`◔ ⠋ ◌ ▲ ◍ ✓ ✗`) so status never
  depends on color alone (accessibility).
- **Selected-card highlight**: subtle background on the focused card — clearer
  focus feedback than a border change alone.
- Shared layout geometry (`HEADER_H`/`FOOTER_H`/`CARD_H`) used by both the
  renderer and mouse hit-testing, with unit tests for the click→lane/card map.

### Notes
- Verified: fmt clean, clippy `-D warnings` 0, 9/9 tests, tmux render confirms
  spinner animation + glyphs + mouse footer.

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
- **Live per-process metrics**: targeted `sysinfo` refresh fills each agent's
  CPU% and resident memory, shown on cards and in the tree view.
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

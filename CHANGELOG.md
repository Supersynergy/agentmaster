# Changelog

All notable changes to agentmaster are documented here. Newest first.

## [0.5.0] — 2026-06-04

CLI full-ready. Everything the TUI does to *external* agents is now also a
headless subcommand, so agentmaster is a complete orchestrator from scripts and
other agents — no TUI required, no Python. `--json` on every read command (the
cli-anything contract) so other agents can consume it.

### Added
- **`ls [--json]`** — every discoverable live agent across backends (tmux panes +
  cmux workspaces) with herdr-style glyphs (`⏸ ▶ ✓ ·`), status, pid, title.
- **`send <ref> <msg…>`** — steer one agent. `workspace:NN` routes to cmux,
  `session:win.pane` to tmux. Logged to the audit trail.
- **`broadcast <msg…> [--tmux] [--needs-input]`** — line to every live agent;
  `--needs-input` narrows to cmux agents waiting on a human; never hits our pane.
- **`goal <name> <text…> [:: dod]`** — pin a goal headlessly (same `::` DoD split
  as the TUI `g` key); persisted to the SQLite `goals` table, rehydrated on import.
- **`goals [--json]`** — list every stored goal + progress.
- New `cli.rs` module; the full surface is now: `tui · events · doctor · ls ·
  send · broadcast · goal · goals · peek · find · dash · start · batch`.

### Notes
- Verified live: `ls` listed real cmux workspaces (incl. this session's own),
  `ls --json` emitted valid JSON, `goal …:: dod` split correctly, `goals` showed
  persisted progress. fmt clean, clippy `-D warnings` 0, **21/21 tests** (+2 cli:
  glyph-map, json-escape).
- grepgod (ghmax) survey found no established single-binary Rust TUI+CLI doing
  tmux+cmux fleet orchestration — the niche is real; design mirrors the proven
  `cmux-meta-orchestrator` subcommand set, now native + headless.

## [0.4.0] — 2026-06-03

Orchestrator parity + goals + dynamic indicators. agentmaster becomes the single
control surface: discover, steer, and goal-track every agent — tmux panes, cmux
workspaces, and native PTYs — from one session, with zero token tax (state is
observed off output/transcripts, coordination stays on-disk).

### Added
- **cmux backend wired in** (`backend.rs` + `app.rs`): `d` / `[* find]` now
  discovers BOTH tmux panes and cmux workspaces (`cmux top --all`). Imported as
  `Source::Cmux`, status mapped from the agent tag, refreshed each tick, steered
  via `cmux send`. `kill` untracks an external workspace (never destroys it).
  Verified live: imported 17 real Claude/Codex cmux workspaces into the board.
- **Orchestrate (`o talk`)** — the in-TUI port of `cmux-meta-orchestrator send`:
  `#N <msg>` steers agent N, `#* <msg>` broadcasts to every live agent. Routed as
  a plain transport write; logged to an orchestrator chat strip (Logs view).
- **Goals** (`g`): pin `<goal> :: <definition-of-done>` on any agent. Persisted in
  a SQLite `goals` table keyed by name, so a goal survives restart/re-import
  (`rehydrate_goal`). Goal-aware state: a DoD match flips the agent to DONE.
- **Dynamic progress indicators**: `infer_progress` ratchets a 0-100% bar from
  output milestones (`step 3/7`, build/test signals) — monotonic, never asked
  for. Shown as a `🎯 ▰▰▰▱▱ NN%` bar on goal cards + the inspect panel.
- **Needs-you alert**: a blinking `⏸ N NEED YOU` badge in the header whenever any
  agent is blocked — the one thing that wants the operator now.
- **Zero-tax peek** (`p` / `peek <id>`): read a session's last user/assistant/next
  action straight off its transcript JSONL (`peek.rs`), no LLM round-trip.
- **Headless orchestrator parity** (`orch.rs`): `find <q>`, `dash [--all]`,
  `start <id>` (passthrough to `session-restore`), and `batch <file>` fan-out
  (spawn one seeded cmux workspace per task; `.json`/`.md` task files, dry-run by
  default). No Python dependency.

### Fixed
- Finished the broken WIP from the prior session (8 compile errors): the
  half-added cmux adapter + orchestrator chat were non-exhaustive and referenced
  undefined `cmux_status`/`cmux_snapshot`. Collapsed an identical-branch bug in
  the cmux `Needs input` parser.

### Notes
- Verified end-to-end this session: fmt clean, clippy `-D warnings` 0, **19/19
  tests** (+6: dod-detect, progress-ratchet, 2× peek, 2× task-parse). Live:
  `peek` digested a real session, `batch` dry-run parsed a Markdown task file,
  PTY-driven TUI imported 17 cmux workspaces and persisted spawn(11)/send(6)/
  goal(1) rows to SQLite — including a goal set on an imported cmux agent.

## [0.3.0] — 2026-06-03

Multi-backend + a clickable toolbar. agentmaster now sees and steers agents that
already run in tmux, not only the PTYs it spawns.

### Added
- **tmux backend adapter** (`backend.rs`): discover panes (`d`), import them as
  `Source::Tmux` agents, derive their state from periodic `capture-pane`, and
  steer them — `send` routes to `tmux send-keys`, `kill` to `tmux kill-pane`.
  Cards show a `·tmux` source tag. Pure tmux CLI, no extra dependency.
- **Backend abstraction**: `Agent.source` (`Native` | `Tmux`); send/kill/poll
  dispatch on it, so more backends (rmux/cmux/kitty) slot in the same way.
- **Clickable toolbar footer** (Normal mode): `[1 kanban] [2 tree] [3 logs]
  [+ new] [* tmux] [m mouse] [? help] [q quit]` — every button is a real click
  target, active view highlighted. One `TOOLBAR` source of truth shared by the
  renderer and the hit-test.
- `doctor` now reports tmux availability + discoverable pane count.
- Unit tests for pane parsing and toolbar hit-mapping.

### Notes
- Verified end-to-end: a worker tmux session was discovered, imported, and its
  state (REVIEW, from its own output) rendered live with the `·tmux` tag.
- fmt clean, clippy `-D warnings` 0, 12/12 tests.

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

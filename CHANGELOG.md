# Changelog

All notable changes to agentmaster are documented here. Newest first.

## [0.11.0] — 2026-06-04

Real response times for the whole *live* fleet — found the exact, universal link
between a workspace and its transcript, replacing the partial snapshot guess.

### Changed
- **Transcript resolution is now PID-based** (`peek::pid_transcripts`): one `ps`
  scan finds every running agent's own session id on its command line —
  `claude … --resume <id>` → `~/.claude/projects/**.jsonl`. The cmux/tmux workspace
  reports the agent's pid; the pid carries the id. Exact, live, no snapshot, covers
  just-spawned agents. Verified: the cmux-reported pid *is* the `--resume` process.
- **Codex too**: codex CLI carries no id on argv, so its pid is resolved via its
  working directory (`lsof` cwd) → the newest matching rollout in
  `~/.codex/sessions/**` (whose `session_meta` records the cwd).
- Result: ~87% of **live** agents now show a ground-truth `↩ last response` time
  (up from ~35% via the partial cmux snapshot). Dead/hibernated cmux tabs (no live
  process) honestly fall back to time-in-state.

### Notes
- Dropped the snapshot/title fuzzy match (`cmux_transcripts`/`norm_title`) — the
  pid path is exact where the old one was a guess.
- fmt clean, clippy `-D warnings` 0, **29/29 tests** (+session-id guard).

## [0.10.0] — 2026-06-04

Honest times + full mouse. The board now knows *when each agent last actually
responded* — read off its transcript, not guessed from polling — and everything
is operable with the mouse and the wheel.

### Added
- **Real "last response" time** (`fleet::last_seen` / `last_response_secs`): each
  cmux/Claude agent is linked to its session transcript (the cmux snapshot's
  `session_id`, refreshed on discovery, resolved to `~/.claude/projects/**.jsonl`),
  and its mtime is the ground-truth last-activity clock. The list shows `↩ 14m`;
  the detail pane shows `↩ last response 14m ago (14:32)`. Verified: 28/28
  snapshot sessions resolved to real transcripts with accurate ages.
- **Cache TTL now anchored on the real last response** (transcript mtime) instead
  of a status guess — so `🧊` countdowns are trustworthy.
- **Mouse everywhere**: click the list **column header** → cycle sort; click the
  **detail pane** → open the agent inside; **wheel scrolls** the list, the board,
  and now the **inspect output** (also `j`/`k` in inspect); click a row → select,
  again → jump to its tab. Toolbar + chat pane already clickable.

### Notes
- Agents not present in the cmux snapshot (e.g. not-yet-indexed) fall back to the
  observed time-in-state — no worse than before, real times where available.
- fmt clean, clippy `-D warnings` 0, **29/29 tests** (+norm_title match key).
  tmux capture confirmed the `↩` column + correct "(no transcript linked)"
  fallback for plain shell panes.

## [0.9.0] — 2026-06-04

A genuinely usable overview for a big fleet — every agent reachable, dense and
calm, with a golden-ratio master-detail layout. Designed around how a human reads
it: "who needs me", scan, read, act.

### Added
- **List view (`1`, now the default)** — a dense, full-width, scrollable table of
  EVERY agent beside a detail pane (golden-ratio 62/38 split). Columns: glyph ·
  task · kind · state · time-in-state · 🧊cache-ttl · cpu. Scales to hundreds where
  the board's cards can't. The detail pane shows the selected agent's title,
  source, status, cache, goal+progress, and recent output, with the actions.
- **Sort (`S` / `[S sort]`)** — cycle smart / stuck / cache / status / name.
  `smart` floats what needs you (blocked, longest-waiting) to the top.
- **Everything reachable** — List scrolls through the whole fleet (j/k, wheel,
  h/l page); the board's lanes now scroll too (`↑N above · ↓N below` replaces the
  dead-end `+N more…`). No more agents you can see but can't open.

### Changed
- **Killed the flicker**: removed the per-tick selection pulse and the NEED-YOU
  strobe (both now stable), and slowed the working spinner to ~3×/s — a board of
  77 agents no longer shimmers.
- Views renumbered: `1` list · `2` board · `3` tree · `4` logs.
- Cards/rows de-cluttered: titles strip cmux decoration AND the trailing
  ` [ref]`; dropped the redundant prompt/last-line column; `cmux cmux` → a single
  `claude`/`codex` kind badge.

### Verified (tmux capture)
- List rendered 4 agents with aligned columns + wide titles (no `[ref]` noise),
  `▸` selection, live detail pane; `j` moved selection and the detail followed;
  no flicker. fmt clean, clippy `-D warnings` 0, 28/28 tests, release reinstalled.

## [0.8.0] — 2026-06-04

Two views of every agent — steer it inside the board, or **jump to its real tab**
and watch it live in its own session. Plus a more clickable board.

### Added
- **Jump to the live tab** (`f`, click a selected card, `[f tab]`, or
  `agentmaster focus <ref>`): switches the cmux UI (`workspace.select` RPC, accepts
  the short `workspace:NN` ref) or the tmux client (`select-window`/`select-pane`)
  straight to the session where the agent runs. Native agents have no external tab.
  Runs off-thread so the board never blocks.
- **Two clear views**: `↵` opens the agent **inside** agentmaster (output + send a
  line); `f`/click opens it **in its own tab**. The selected card spells both out.
- **Headless** `agentmaster focus <ref>` for scripts.

### Changed
- Clicking an already-selected card now **jumps to its tab** (was: inspect inside).
  Inspect-inside moved to `↵`/double-nothing — the two views are now distinct.
- The selected card **pulses** (bright ↔ accent, thick border) so focus visibly
  blinks, and its border shows the live action hint `↵ inspect · f→tab`.

### Verified (tmux capture)
- Footer renders `[f tab]`; selected card shows the pulsing thick border titled
  `↵ inspect · f→tab` over `claude fix the auth bug` / `working for 4s 🧊1:00:00`.
  `agentmaster focus workspace:3` switched the cmux UI live (then back). fmt clean,
  clippy `-D warnings` 0, 28/28 tests, release reinstalled.

## [0.7.0] — 2026-06-04

Open it and your fleet is just *there* — plus a live prompt-cache countdown so you
can see which Claude/Codex sessions are still cheap to resume.

### Added
- **Auto-discovery on boot**: the TUI scans tmux panes + cmux workspaces the moment
  it starts (off-thread, non-blocking) — no need to press `d`/`[* find]` first.
  Your agents are on the board immediately.
- **1h prompt-cache countdown** (`fleet::cache_remaining_secs`): every Claude/Codex
  card shows a `🧊` timer counting **down from the last generation** toward the 1h
  cache TTL — full/hot while the agent is working, ticking down once it goes
  idle/blocked, green → amber (<15m) → red (<5m) → `🧊cold` at expiry. At a glance
  you see which sessions still have a warm cache (cheap to continue) vs cold (the
  next turn pays full, uncached input cost — ping them now).

### Verified (tmux capture)
- Booted in tmux, **auto-discovered** a `Claude · build the parser` pane with no
  keypress; card rendered `claude build the parser` (cmux decoration stripped),
  `working for 3s`, and `🧊1:00:00` (hot). fmt clean, clippy `-D warnings` 0,
  **28/28 tests** (+2: cache_remaining, fmt_countdown). Release reinstalled.
- Note: cmux discovery needs the cmux env (`CMUX_SOCKET_PATH`) the app injects into
  its workspaces — present when agentmaster runs inside a cmux tab (its normal home),
  absent in a bare tmux session.

## [0.6.0] — 2026-06-04

Observability you can act on, plus a faster, quieter board. Built by two parallel
sessions that converged (voice + chat pane + async I/O from one, the observability
model + card polish from the other) — integrated, not clobbered.

### Added
- **Time-in-state observability** (`fleet::last_change` / `in_status_secs` /
  `note_status`): cards now show **how long an agent has held its current state**
  ("blocked for 18m"), the honest signal for *imported* agents whose true start we
  never owned. Fixes the old "everything shows the same idle time" problem (all
  cmux agents imported together read identical age/idle — useless).
- **Stuck surfacing**: a blocked agent past 5 min gets a loud `⏰ stuck` marker;
  the header shows `⏰ N stuck>5m` next to NEED YOU; BLOCKED + REVIEW lanes sort by
  time-in-state so the longest-waiting agent is the top card.
- **Cleaner cards**: cmux's own title decoration (`🟣 Claude ✓ ·`) is stripped to
  the task; a `claude`/`codex` kind badge replaces the redundant `cmux cmux`
  labels; line 2 shows the backend ref (`workspace:96`) instead of a blank dir;
  memory is hidden when unknown.
- **Voice push-to-talk** (`v`): mic → whisper.cpp → the orchestrator bar (parallel
  session).
- **Permanent orchestrator chat pane**, pinned bottom, always-on (parallel session).

### Performance
- All blocking backend I/O (`cmux top`, tmux `capture-pane`) moved **off the render
  thread** to one-shot workers; results flow back over the event channel — the UI
  never stalls on a 107-workspace scan.
- Off-thread refresh **throttled to ~1.5s** (cmux top is ~0.2s on 107 ws and
  external status doesn't change faster); native agents still update instantly via
  PTY events. `note_status` only restamps on real transitions, so the in-state
  clock is cheap and correct.

### Notes
- debugmaster `hunt`: production code clean (the 6 findings are idiomatic
  `unwrap()` in test modules). fmt clean, clippy `-D warnings` 0, **26/26 tests**
  (+5: note_status, in_status_secs, clean_title, agent_kind, progress_bar). Release
  PTY smoke booted, imported 107 cmux workspaces, navigated, quit 0.

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

# Changelog

All notable changes to agentmaster are documented here. Newest first.

## [Unreleased]

### Deferred — Cursor SDK adapter
- `cursor-sdk/` remains an experimental, unshipped adapter. It is deliberately
  excluded from v0.19.0 until its pinned TypeScript dependencies are installed
  from a clean lockfile and the adapter has passed its own type-check and MCP
  smoke test. The native AgentMaster CLI remains the release surface.

## [0.19.0] — 2026-07-27

### Fixed — bounded ATS swarm bridge
- **Hard execution boundaries:** `swarm` now defaults to at most three lanes;
  `--n > 3` requires explicit `--fanout`. Every parallel and legacy capsule is
  UTF-8-safe and capped at 2,800 bytes, and asks for a result of at most 500
  tokens. On PASS, deadline, or a measured overrun, AgentMaster sends a
  targeted message plus Ctrl-C only to the workspace it created; it never
  calls cmux `close-workspace`.
- **No pretend budget gates:** token budgets run only with the canonical
  `agent-token-ledger` available and complete provider usage for every chosen
  lane. A budget request routes to the metered `kimi-worker` subset (currently
  one lane); unsupported fan-out, sequential mode, and non-invoice static
  `--budget-cost` fail closed with an actionable error instead of silently
  measuring zero tokens. Only a budgeted `kimi-worker` atomically refreshes
  its cumulative wire ledger every two seconds, so the controller can poll an
  in-flight lane; unbudgeted workers retain final-only ledger I/O. A total
  token budget is divided conservatively across the selected lanes.
- **Private by default:** Synapse prime/recall and result persistence are now
  off unless the operator passes `--recall`; this prevents unrelated prior
  swarm context from being injected into normal project work.
- **Model binding is real:** `auto`, batch, sequential swarm and parallel
  swarm put a locally verified runtime `--model` selector before the prompt;
  they no longer leak `--model <name>` into natural-language task text.
- Swarm lanes now receive an explicit bounded-capsule path instead of only the
  bare task, so the generated oracle, evidence contract and one deterministic
  tool route are actually available to each runtime. Results use compact
  `STATUS/EVIDENCE/HANDOFF` fields; the controller owns any targeted follow-up.
- The one verified `si route` skill is now also passed by path inside each
  lane capsule. Workers read only that relevant skill instead of receiving a
  copied global catalog or an inert controller-only routing result.
- `si route` now runs once against the real task with `--strict --json`, accepts
  at most one existing skill path and never prints router JSON per lane. Synapse
  recall preview is single-line and capped at 360 characters.
- Updated existing `anyhow` from `1.0.102` to `1.0.104` after the RustSec scan;
  the release was nine days old at the update check and clears RUSTSEC-2026-0190.

### Added — New lane runtimes: kimi, cursor-agent, devin
- `kimi` (positional prompt), `cursor-agent` with aliases `cursor`/`composer`
  (positional prompt), and `devin` (prompt after `--`; in-session `/model`
  switches across 35 model families incl. Gemini Flash, Kimi K2.7, GLM-5.2,
  Grok 4.5, GPT-5.6, Claude). Picked up by `doctor` and the TUI new-agent
  completer via `KNOWN_RUNTIMES`; focused tests in `runtime.rs`.

### Added — Auto-router + oracle-gated swarm (cheapest-passing model selection)
- **`models [--tier T] [--json]`** — static model registry (2026-07 pricing,
  9 models across 3 tiers: frontier_closed, mid_tier, open_weight). Each row
  carries vendor, input/output prices, context window, strengths, weaknesses,
  and `best_for` task types. `--json` for machine consumption (agent-token-saver
  integration). Single source of truth for cost-ladder decisions — no remote
  API calls, no runtime discovery.
- **`auto "<task>" [--name N] [--dry-run]`** — classify the task by keywords,
  pick the cheapest model that's strong at that task type, then `assign` it.
  Classifier maps to 9 task types (code_generation, code_review, planning,
  research, shell_projection, long_context, verification, creative, general).
  Ponytail: one keyword matcher, one cost-sorted pick, reuse `assign`. The
  model flag is injected as `--model <flag>` for runtimes that accept it;
  aider/opencode/shell get the bare task (fail-open).
- **`swarm <name> --oracle "<cmd>" [-n 3] [--budget B] [--dry-run] "<task>"`**
  — oracle-gated swarm. Spawns N diverse agents on the same goal, first
  oracle PASS wins. Diversifies across runtimes + tiers (cheapest + mid-tier
  + frontier, never duplicate runtimes) so a single model's failure mode
  doesn't block convergence. Each lane gets a bounded capsule (300-700
  tokens) written to `~/.agentmaster/capsules/<name>-lane<i>-<model>.md`
  — the agent-token-saver contract. The swarm auto-inits an omnigoal so
  `goal-check <name>` gates all lanes. First PASS wins; the controller signals
  only the known losing lane processes and retains their workspaces for
  inspection.
- **`src/router.rs`** — new module: `ModelSpec`, `TaskType`, `Tier`, `REGISTRY`,
  `classify()`, `pick()`, `pick_swarm()`. 11 unit tests covering classification,
  cheapest-passing pick, swarm diversity (no duplicate runtimes), cost sorting.
- **Integration with agent-token-saver**: capsules are bounded (300-700 tokens,
  paths/hashes, constraints, PASS/FAIL oracle) — never transcripts. The
  `models --json` output is machine-readable for `si route` or token-cfo
  pricing pipelines. Swarm lanes are independent workers (no transcript
  sharing) — the token-saver contract for subagent teams.
- Verified: `cargo test` 55/55 green (44 existing + 11 new router tests);
  `models`, `auto --dry-run`, `swarm --dry-run` smoke-tested across 4 task
  types (shell/code-gen/long-context/planning).

### Added — Omnigoal lifecycle (closed-loop goal controller, ponytail-style)
- **`goal <name> <text> --oracle <cmd> [--budget N] [--deadline ISO]`** —
  omnigoal init. Stores a machine-checkable oracle (shell command that exits 0
  iff the goal is done), a token budget cap, and an ISO deadline alongside the
  goal text + DoD. The TUI and `goal check` rehydrate these. Flags must come
  before the trailing goal text (clap `trailing_var_arg`).
- **`goal-check <name>`** — runs the oracle in a subshell, classifies the
  result, and records a try. On failure, scans stdout+stderr for the
  bottleneck (first line matching `error|fail|missing|panic|not found|undefined`)
  and persists it to the goal row. Three tries without oracle passing → goal
  auto-abandoned (no infinite loops). Budget cap (`AM_GOAL_SPEND_<NAME>` env
  var) and deadline also trigger abandonment. Exit codes: 0=pass, 1=fail,
  2=abandoned, 3=no oracle.
- **`goal-close <name> [--summary ...] [--abandon]`** — marks a goal done (or
  abandoned) with a closing summary. The summary is persisted to the goal row
  + the event stream, so the next session can rehydrate the outcome without
  replaying the transcript. This is the "persistent outcome" primitive.
- **`goal-spawn <name> [--capsule path] [--skill name]`** — registers a
  subagent against this goal with a bounded capsule + skill. Ponytail: we log
  the event only — the capsule file lives on disk and is referenced by path,
  never inlined into the goal row. Cross-agent coordination happens via the
  goal JSON (the row), not by sharing transcripts.
- **Schema migration**: `goals` table extended with `oracle`, `budget_tokens`,
  `deadline`, `tries`, `status`, `bottleneck`, `summary`, `closed_ts` columns.
  Tolerant migration via `PRAGMA table_info` — fresh DBs get the full schema,
  legacy DBs get `ALTER TABLE` only for missing columns.
- Verified: `cargo test` 44/44 green; live smoke test exercises init → check
  (try 1/3 fail with bottleneck) → check (try 2/3 fail) → spawn → close.

### Added — Advanced orchestration levers (CAO-class primitives, ponytail-style)
- **Runtime-agnostic `batch()`**: tasks can now carry a `runtime` field
  (`- runtime: codex` in MD, `"runtime":"hermes"` in JSON) and `batch` spawns
  each via `runtime::resolve()` instead of hardcoding `claude`. Default
  runtime remains `claude` (legacy). `--model` is only forwarded to runtimes
  that accept it (claude/codex/hermes/gemini/cline); aider/opencode/shell
  launch with their native flags. Dry-run prints `runtime=` per task.
- **`send --wait <secs>`** (handoff primitive): sends the line, then polls the
  target's transcript every 2s until a NEW assistant message lands (baseline
  captured pre-send) or the timeout expires. Zero token tax — reads the JSONL
  directly via `peek::digest`, never asks the agent to report. Timeout returns
  a "still running — peek later" notice, the agent keeps working (same
  semantics as CAO's `handoff`).
- **`assign <runtime> <task> [--name N]`** (async fan-out primitive): spawns
  one fresh cmux workspace seeded with the task and returns immediately.
  Auto-generates a slug from the task text as workspace name. Logs the
  callback-ref (`workspace:NN`) to the audit log so the supervisor can
  `peek` it later. Fire-and-forget like CAO's `assign` — the worker is
  expected to work independently.
- **`doctor --costs`**: reads the audit log and prints per-target send/assign
  counts. No token spend (agentmaster never sees provider billing — use
  `agent-token-ledger` for that). Ponytail: reuses the existing log, no new
  accounting state.
- **`am` master router** rewritten with new commands: `am handoff <ref> <msg>
  [--wait 120]` → `agentmaster send --wait`; `am assign <runtime> <task>
  [--name N]` → `agentmaster assign`; `am message` as alias of `am send`;
  `am skill <runtime>` dynamically reads the control-skill's `SKILL.md` path
  and prints its first non-frontmatter heading (no hardcoded doc snippets to
  drift). Failsafe: missing skill → "no control skill registered", missing
  binary → "unknown command", unknown subcommand → runtime passthrough if the
  name is on PATH, else 127. ~80 lines of bash, zero deps.
- **`am skill <runtime>`** bridges to the per-agent control skills
  dynamically — prints the `SKILL.md` path and first heading so a supervisor
  agent can discover what a runtime's skill does without a second round-trip.

### Added (earlier this session)
- **`am` master router** (first version): one entry point for the whole fleet.
- **`doctor --verbose`** probes each found runtime's `--version` (falls back to
  `-V`) so you see not just presence but what's on PATH — e.g.
  `runtime claude : found  2.1.218 (Claude Code)`.
- **`peek --tail N`** prints the last N raw transcript events (tagged 🧑/🤖)
  instead of the structural digest — stream-of-thought view for live debugging.
- **`send --goal <name> [--dod <text>]`** combines steer + pin: sends the line
  to the agent AND persists a goal on that name in one command, so you don't
  have to chain `send` + `goal`.
- **Tab completion in the TUI's new-agent input**: typing `cl<Tab>` expands to
  `claude `; bare Tab on empty input cycles through `KNOWN_RUNTIMES`. Discovers
  the registry without leaving the keyboard.

### Added (earlier this session)
- **Eight agent runtimes** now first-class: `claude`, `codex`, `hermes`, `ggcoder`,
  `aider`, `opencode`, `gemini`, `cline`, plus the existing `shell`. `runtime::resolve`
  maps each to its CLI; `KNOWN_RUNTIMES` is the single source of truth. The TUI's
  new-agent prompt and help line list every runtime. `agent_kind` / `agent_tag`
  give each a distinct colored badge (Claude amber, Codex blue, Hermes cyan,
  GGcoder green, Aider magenta, OpenCode violet, Gemini orange, Cline rose) so
  the fleet reads at a glance. `doctor` probes every runtime's presence on PATH.
  Aider and OpenCode launch bare (their positionals are files/paths, not a
  prompt) — steer them via `agentmaster send` after spawn.

### Changed
- **cmux discovery now reads `cmux top --all --json`** instead of scraping the
  human-readable tree. The structured surface gives stable `ref`/`title`/`tags`
  fields, so workspace titles, status, git/phase tags and the agent pid no longer
  depend on tree-drawing characters or column layout. The text parser
  (`parse_cmux_top`) is kept as an automatic fallback for older cmux builds that
  lack `--json`. New `parse_cmux_top_json` mirrors the text-tree status semantics
  exactly (agent-type tag = base status, `*_code` tag wins on `Needs input`) and
  is order-independent. Covered by `parses_cmux_top_json` +
  `cmux_top_json_rejects_garbage`.

## [0.18.0] — 2026-06-04

Symbol-led list rows — read the fleet at a glance: *what is happening · where ·
who · what it needs*, no identity emoji, no rigid table.

### Added
- **Project icons** (`project_icon`). A stable per-repo emoji (🤖 agentmaster,
  🔮 synapse, 🎫 events-hub, ⚡ supermax, 🌀 supersyn, 🧩 cmux, 🧲 lead, 📇 crm,
  🦀 zeroclaw, 🏆 achiever, 📁 other) so the eye groups the fleet by project.
- **Per-agent signal flags** (`agent_flags`): `🔔`/`⏰<dur>` waiting-on-you,
  `🎯N%` goal, `⎇dirty`/`⇡ahead`/`⎇clean` repo state, `◇<phase>` workflow phase,
  `💤<dur>` long idle — only the ones that apply, most-urgent first, capped to a
  fixed width so the right-edge time block always aligns.
- **Cell-accurate column alignment** (`cells`/`pad_cells`, new `unicode-width`
  dep) — emoji count as 2 cells, so mixed emoji/ASCII rows line up.

### Changed
- List row is now `status · 📁proj · agent · task — what it's doing · signals ·
  ↩last · 🧊ttl`. The dynamic status glyph leads on the far left ("was passiert").
- **Claude vs Codex** is a short colored word tag (`Claude` amber / `Codex` blue)
  instead of the old `🟣`/`🧠` emoji — readable in screenshots and logs.
- **Responsive layout**: a narrow terminal renders the list full-width as one
  compact column; a wide one (≥100 cols) keeps the detail pane on the right.
- The column header is replaced by a one-line legend (the row is symbol-led, not
  tabular). The `kind`/`cpu` columns are dropped from the row.

### Notes
- fmt clean, clippy `-D warnings` 0, tests green (incl. new `cells`, `pad_cells`,
  `agent_tag`, `project_icon` cases).
- Next: load a project→icon override map from `~/.config/agentmaster/icons.toml`.

## [0.17.0] — 2026-06-04

Signal filter pass — the list can now collapse stale/non-agent noise without
changing discovery or status truth.

### Added
- **Hide-noise list mode** (`H`). Hides obvious imported shell tabs (`⏱ Last
  login…`, URL tabs, starter-tip tabs without a real task label) and `Idle` rows
  whose last real response is older than 24h. Blocked, review, working, queued,
  and labelled real agents remain visible.
- The List title shows the current mode (`H hide:all` / `H hide:noise`) so the
  operator can tell whether the fleet is raw or signal-filtered.

### Fixed
- `/` filter now searches the visible transcript-derived `task_label`, not only
  the raw multiplexer title. This keeps v0.16's human task titles searchable.

### Notes
- `h` remains List page-up; `H` is the noise toggle to preserve `h/j/k/l`
  navigation muscle memory.
- fmt clean, clippy `-D warnings` 0, 38/38 tests.

## [0.16.0] — 2026-06-04

Usability pass — "what a human actually wants to see". The list was technically
rich but read like noise: a column of `</task-notification>` and `## ▸ Last user
request` where the task should be, plus a `↑0` artifact on the selected row.

### Fixed
- **Real task titles.** When a multiplexer title is a system fragment
  (`</task-notification>`, `## ▸ …`, a bare URL, `Last login…`, a `⏱` shell tab),
  the row now shows the agent's actual task — pulled from the transcript's first
  user prompt (`peek::first_prompt`, a cheap head-only read) and cached on the
  agent (`task_label`). The list went from a wall of `</task-notification>` to
  legible asks like "suche bitte alles über Human Design…". `peek::is_fragment_title`
  decides; `ui::display_title` applies it.
- **Scroll position moved to the block's bottom border** (`12–34 of 83  j/k
  scroll`) instead of a right-edge overlay that printed a dead `↑0` on top of the
  first row's cpu column. No more data overlap; the indicator only shows when the
  list overflows.
- `clean_title` strips a leading `⏱` so non-agent shell tabs read cleanly.

### Notes
- `task_label` is set only when the title is a fragment AND a transcript is
  linked, so native/tmux agents and already-meaningful titles are untouched.
- fmt clean, clippy `-D warnings` 0, 36/36 tests. Verified live: the
  `</task-notification>` rows now render real tasks; `↑0` gone; `1–15 of 83`
  scroll position in the border.

## [0.15.0] — 2026-06-04

Real-time + notifications. Backed by a research pass (cmux 0.64.x changelog,
plus what people ask for on HN/Reddit and what the field ships — ccboard,
hive, tmux-agent-sidebar): the two highest-value, zero-token features were a
live status stream and desktop alerts. Both land here. See
`docs/COMPETITIVE-AUDIT.md`.

### Added
- **Live status via `cmux events`** (`peek::parse_set_status` +
  `App::apply_live_status`). A long-lived, self-reconnecting thread tails cmux's
  sidebar event stream; each `set_status` push updates the agent's lane the
  instant cmux knows, instead of waiting for the ~1.5s poll. Agents resolve by
  workspace UUID (exact, stable; the UUID→ref map comes free from the checkpoint
  pass) or pid. The poll path stays as a backstop — `note_status` dedupes so a
  transition seen by both never double-fires. The `--reconnect` child is tracked
  by pid and killed on exit, so it never outlives the TUI.
- **Desktop notifications** on the transitions that want you — an agent now
  **needs you** (blocked) or just **finished** (done) — via `osascript` (no new
  dependency), off-thread, globally throttled so a burst can't spam. Toggle with
  `N`; a `🔔`/`🔕` badge in the header shows the state. Fires from every status
  path (live event, poll, native PTY).

### Notes
- The event stream is opt-out at the source: it only runs when cmux is present,
  carries `--no-ack` (read-only), and filters to `set_status` payloads, so volume
  is just real status changes — not heartbeat noise.
- Research deliverable: `docs/COMPETITIVE-AUDIT.md` maps cmux 0.64.x primitives,
  agentmaster's feature set, and the competitive gap (cost tracking, subagent
  trees, worktree isolation) into a prioritised backlog.
- fmt clean, clippy `-D warnings` 0, 34/34 tests. Verified live: event child
  spawns + dies with the TUI (0 orphans), `🔔` renders, coverage badge intact.

## [0.14.0] — 2026-06-04

Rides the newest cmux (0.64.x) primitives: the resume_binding behind Agent
Hibernation, and the richer workspace tags. Ground-truth `↩` time coverage jumps
**59% → 86%** and is now exact + drift-proof.

### Added
- **Exact session link via cmux `resume_binding` checkpoints**
  (`peek::cmux_checkpoints`). For every workspace — live, idle, OR hibernated —
  read `workspace.list` then `surface.list {workspace_id}` and pull
  `resume_binding.checkpoint_id` (the session id cmux persists to resume the
  agent), resolve it to a transcript, and key it by the **stable `workspace:NN`
  ref**. This is the link the old snapshot title-match couldn't make: it covers
  hibernated tabs (no live pid) and can't mis-date an agent when its title drifts.
  Verified live: 69/80 timed (86%), up from 48/82 (59%); the rest are non-agent
  shell tabs with no transcript.
- **Rich workspace tags** (`backend::CmuxWorkspace.{git,phase}`): cmux now emits
  `git` (dirty / ahead / clean / none) and `phase` (e.g. tests-pass / commit)
  tags in `cmux top`. The detail panel surfaces them as `⎇ <git> · ◇ <phase>` —
  amber when there's uncommitted/unpushed work — and they refresh on every poll.

### Changed
- cmux transcript resolution ladder is now: stable-ref checkpoint → live pid
  (`--resume`) → snapshot title match. Re-discovery still backfills a missing link
  on already-imported agents and re-anchors their clock.

### Notes
- The checkpoint scan is two `cmux rpc` calls per workspace (~18ms each), run on
  the discovery thread (boot + `d`), never the UI. Codex workspaces carry no
  checkpoint binding and resolve via the existing pid/cwd rollout path.
- fmt clean, clippy `-D warnings` 0, 33/33 tests. Verified live under a PTY on the
  80-agent fleet: 86% coverage badge + `⎇ ahead · ◇ tests-pass` render.

## [0.13.0] — 2026-06-04

Times that stay true across restarts + a detail panel that shows what each agent
last said. The headline complaint — "the times don't match" — was two bugs: a
~45s blank window after import, and the time-in-state clock resetting to 0 on
every (re)discover/restart. Both fixed; coverage of ground-truth `↩` times also
broadened.

### Fixed
- **Time-in-state no longer resets.** Imported agents stamped their clocks at
  import time, so a tab blocked for 2h showed "blocked 2s" right after discovery
  and reset again on the next restart. Now, on import, the clock is anchored to
  the transcript mtime (ground truth) when linked, and otherwise restored from a
  persisted `since` (new `seen` table) — so "blocked 2h46m" survives a restart.
- **Immediate last-response stamp.** `Agent::anchor_time` stats the transcript
  the moment it's linked instead of waiting up to ~45s for a housekeeping tick;
  no more blank `↩` window right after discovery.

### Added
- **Auto-peek detail panel.** Selecting an agent now shows its last user prompt,
  last assistant message (wrapped), and inferred next action read straight off
  the transcript — no `p` keypress, no tokens. Cached per-agent, refreshed on
  selection change and on a ~1.5s throttle.
- **Time-coverage badge** in the header (`↩ N/M timed X%`): the at-a-glance trust
  signal for the clock — how many agents show a ground-truth time vs fall back to
  observed time-in-state.
- **`quiet` sort** (S cycles to it): longest-silent agents first, untimed sink to
  the bottom — fleet triage for "who's gone dark".
- **Cross-restart state persistence** (`store::{save_seen,load_seen}`), keyed by
  the stable source ref (`cmux:workspace:NN` / `tmux:target` / `native:name`), not
  the drifting title.

### Changed
- **Broader transcript resolution** (`peek::snapshot_transcripts`): use the cmux
  snapshot's exact `full_path` first, then `session_id`, then a `dir` fallback
  (newest transcript in the surface cwd — Claude project dir or newest Codex
  rollout). Re-discovery now also backfills a still-missing transcript on already-
  imported agents and re-anchors their clock.

### Notes
- Coverage ceiling is real and honest: dead/hibernated cmux tabs whose title has
  drifted since their last snapshot can't be exactly transcript-linked (cmux
  exposes no stable workspace-id↔session map for them, and 75/81 tabs share a
  cwd so cwd-newest would mis-date them). Those keep an honest time-in-state
  clock — now durable across restarts — rather than a guessed `↩`.
- fmt clean, clippy `-D warnings` 0, 35/35 tests. Verified live under a PTY on the
  real 82-agent fleet: real times (`↩2h46m`, `blocked for 31m23s`), auto-peek, and
  the coverage badge all render.

## [0.12.0] — 2026-06-04

Maximised honest time coverage: live + recovered dead tabs.

### Added
- **Snapshot fallback for dead/hibernated tabs** (`peek::snapshot_transcripts`):
  when the exact live-pid path misses (the agent's process is gone), recover its
  transcript from the cmux snapshot's recorded `session_id`, matched by normalized
  title. Combined with the pid path, **61% of tabs now show a ground-truth `↩`
  time** (up from 35% via snapshot-only). Every `↩` shown is real — nothing is
  fabricated; tabs with no recoverable session fall back to time-in-state.

### Notes
- The remaining tabs are fully-closed cmux sessions cmux no longer records a
  session id for — no ground truth exists to show, so the tool stays honest
  rather than guessing.
- fmt clean, clippy `-D warnings` 0, 29/29 tests. Pipeline re-verified live:
  37 live (pid→`--resume`) + 14 recovered (snapshot) = 51/83.

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

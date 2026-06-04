# Competitive & Capability Audit — 2026-06-04

Deep pass behind the v0.14–v0.15 work: what cmux now exposes, what agentmaster
does with it, what the field ships, and what people actually ask for. Source of
the prioritisation in the changelog.

## 1. cmux 0.64.x primitives agentmaster can stand on

| Primitive | What it gives | agentmaster use |
|---|---|---|
| `cmux rpc workspace.list {}` | every workspace: `id` (UUID), `ref` (`workspace:NN`), `current_directory`, `title` | UUID↔ref map for the event stream |
| `cmux rpc surface.list {"workspace_id":UUID}` | per-ws `resume_binding.checkpoint_id` (= Claude session id), cwd, kind | **exact, stable-ref transcript link** → 59%→86% time coverage (v0.14) |
| `cmux events [--category sidebar] [--reconnect]` | live JSON event stream; `set_status` payload = `<kind> <status> --tab=UUID --pid=N` | **real-time lane updates** (v0.15) |
| `cmux top --all` tags | `tag git "dirty/ahead/clean/none"`, `tag phase "tests-pass/commit"`, `tag proc "N"`, `tag claude_code "<status>" pid=` | status + **git/phase badges** (v0.14) |
| `cmux-snapshot` → `~/.cmux-snapshots/*.json` | per-surface `full_path`, `session_id`, `dir`, `first_prompt` | dead-tab fallback transcript link |
| Agent Hibernation (#4165) | idle agents pause; resume via `resume_binding` | explains the pid-null fleet; checkpoint link covers them |
| `cmux hooks feed --source <agent>` | per-agent hook event feed | **unused** — candidate for richer activity |
| `cmux diff [--workspace ref]` | per-workspace diff | unused (cwds shared, not worktrees → low value here) |

GOTCHA: `surface.list` filters on `workspace_id` (UUID), **not** `workspace`
(ref — silently returns the focused surface).

## 2. agentmaster feature set (v0.15)

- Kanban + dense list views; one session sees/steers all cmux+tmux agents.
- Zero orchestration tax: status observed from output/tags, coordination on-disk
  (SQLite), never an LLM round-trip.
- Exact last-response times (checkpoint/pid/snapshot ladder, 86% coverage),
  anchored + persisted across restart (`seen` table).
- Auto-peek detail panel (last user/assistant/next action, zero-token).
- Live status stream + desktop notifications (needs-you/done).
- git/phase badges, cache-TTL countdown, goals + DoD + progress ratchet.
- Coverage badge, smart/stuck/cache/quiet sorts, jump-to-tab, orchestrator
  broadcast, voice, headless CLI (`ls/send/broadcast/goal/peek/events`).

## 3. The field (mined via ghmax, 2026-06-04)

| Tool | Stars | Lang | Standout features |
|---|---|---|---|
| ccboard | 68 | Rust | cost/token tracking, budget alerts + 30-day forecast, anomaly detection, hourly heatmap, subagent tree, conversation viewer w/ regex, code metrics (+N/-N), web dashboard |
| hive | 25 | Go | **git-worktree isolation** per agent (clone/recycle/spawn), real-time status |
| tmux-agent-sidebar | 242 | Rust | every Claude/Codex/OpenCode pane, **desktop notifications**, prompts/tool-calls/wait-reasons/task-progress, **worktree spawn+teardown**, git tab w/ PR numbers |
| ccgram, marmonitor, agent-dashboard | — | mixed | telegram/web mirrors, status monitoring |

What recurs (= validated demand): **real-time status**, **desktop
notifications**, **git state / diff stats**, **cost/token tracking**,
**worktree isolation**, **subagent trees**. HN ("Real-time dashboard for Claude
Code agent teams") also flags hook-latency in the agent critical path — which
vindicates agentmaster's zero-token, observe-don't-ask design.

## 4. Gap → backlog (prioritised)

| Feature | Value | Effort | Verdict |
|---|---|---|---|
| Live status stream | high | med | **done v0.15** |
| Desktop notifications | high | low | **done v0.15** |
| git/phase badges | med | low | **done v0.14** |
| Cost/token tracking | high | high | next — parse transcript usage; fits the prompt-cache HUD already shown |
| Subagent tree | med | med | parse transcript `parentUuid`/sidechain; show nested under parent |
| Worktree spawn/teardown | med | med | `git worktree add` + native spawn; one-keystroke teardown |
| `cmux hooks feed` activity | med | med | richer per-agent activity (tool calls, wait reasons) than `set_status` |
| git diff stats (+N/-N) | low here | low | deferred — cmux cwds are shared repos, not per-agent worktrees, so stats aren't agent-specific until worktrees land |

## 5. Design invariants held

Everything above stays inside the zero-tax contract: observed from
output/tags/transcripts or on-disk RPC, never a token-costing self-report.
Notifications + the event stream are off the render thread; the event child is
reaped on exit. Colors keep one-meaning-per-color; `ui.rs` stays pure.

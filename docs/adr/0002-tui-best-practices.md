# ADR 0002 — TUI & observability best practices

- Status: accepted
- Date: 2026-06-03

The goal asked for *perfect observability with TUI building best practices*. This
ADR codifies the rules agentmaster follows, each tied to where it lives in code so
future changes don't quietly regress them.

## TUI rendering

1. **Render is a pure function of state.** `ui::render(f, &App)` reads `App` and
   draws; it never mutates. All state changes happen in `app.rs`. → testable,
   no render-order bugs.
2. **One meaning per color, defined once.** `ui.rs` color constants
   (`C_WORKING` green, `C_BLOCKED` yellow, `C_REVIEW` magenta, `C_DONE` blue,
   `C_IDLE`/`C_DEAD` gray). `status_color()` / `kind_color()` are the only
   mappers. No ad-hoc colors inline.
3. **Exactly one focus highlight.** The focused lane uses a thick cyan border; the
   selected card uses a cyan border. Nothing else competes (von Restorff).
4. **Truncate, never overflow.** `trunc()` is char-based (UTF-8 safe) with an `…`
   marker and is applied to every dynamic string against the *actual* cell width.
5. **Empty + overflow states are explicit.** Empty lanes say `— empty —`; hidden
   cards say `+N more…` (no silent truncation); empty views guide the next action
   (`press n to spawn`).
6. **Contextual footer.** The keybind hint changes per mode (Normal / Input /
   Inspect / Help) so the next action is always on screen.
7. **Discoverable help.** `?` opens a full keymap overlay; `Clear` is drawn under
   overlays so they never bleed.
8. **Responsive by construction.** Layout uses ratio/min/length constraints, not
   absolute coordinates, so resize just works.
9. **Non-blocking loop, capped cadence.** `event::poll(50ms)` → ~20fps idle, no
   busy-spin; housekeeping every 10th tick.
10. **Clean restore always.** `ratatui::init()` installs a panic hook;
    `ratatui::restore()` runs on exit. Verified: PTY-driven quit leaves the
    terminal sane (exit 0).
11. **One input affordance.** A single overlay input box with a visible cursor
    (`▏`); `Esc` cancels, `Enter` submits. No modal soup.

## Observability

1. **Dual sink, always on.** Every state change and action writes to SQLite
   (`events`, queryable audit) *and* structured JSONL (`tracing`). One for
   queries, one for machines/tailing.
2. **Observed over reported.** Status is derived from output (`state.rs`), so the
   observability trail reflects reality, not an agent's self-description.
3. **Best-effort logging.** `store.log` swallows errors — telemetry must never
   crash the UI.
4. **Headless parity.** Everything visible in the TUI is reachable without it:
   `agentmaster events -n N` and `agentmaster doctor`.
5. **Tunable verbosity.** `AGENTMASTER_LOG` (env-filter) controls JSONL level
   without code changes; default `info`.
6. **Durable + rotating.** JSONL rotates daily under `~/.agentmaster/logs/`;
   SQLite uses WAL. State survives restarts.

## Enforcement

`cargo clippy --all-targets` must stay at 0 warnings; `cargo fmt` is canonical;
unit tests cover the non-UI logic (ANSI stripping, state detection). Render code
is exercised by the PTY-driven smoke test in CI.

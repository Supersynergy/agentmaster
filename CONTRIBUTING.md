# Contributing

agentmaster is a Rust TUI for observing and steering local coding-agent fleets.

## Development

```bash
just setup
just check
just ci
```

Use Rust 1.95+.

## Pull Requests

- Keep changes scoped and user-visible behavior documented in `CHANGELOG.md`.
- Keep `src/ui.rs` render-only; state mutation belongs in `src/app.rs`.
- Prefer observed/local state over token-costing self-report paths.
- Add focused tests for behavior changes.
- Run `just ci` before opening a PR.

## Release Notes

Lead with operator benefit. Mention the version, user-facing changes, verification
commands, and any residual limitations.

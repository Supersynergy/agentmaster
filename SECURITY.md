# Security

agentmaster runs local commands, talks to local tmux/cmux CLIs, reads local
transcripts, and writes an SQLite audit log under `~/.agentmaster`.

## Reporting

Please report security issues privately to the repository owner. Do not file a
public issue with exploit details.

## Scope

Relevant reports include:

- command injection through CLI arguments or agent names;
- unintended disclosure of transcript contents;
- unsafe handling of terminal escape sequences;
- destructive process control that targets the wrong PID/session;
- persistence bugs in `~/.agentmaster`.

## Local Data

Do not commit `~/.agentmaster`, transcripts, logs, or local session dumps. The
repository `.gitignore` excludes project-local `.agentmaster/` and `target/`.

//! Runtime adapter: map a runtime name to the program + args to launch. This is
//! the seam where Claude Code, Codex, Hermes, ggcoder, Aider, OpenCode, Gemini,
//! Cline, or a plain shell all become "an agent" the board can see and steer.
//! Pluggable by design — adding a CLI is one match arm.

pub struct RuntimeSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Resolve a runtime to a launchable command. `task`, when present, is passed as
/// the agent's initial instruction where the CLI supports it. Agents that take a
/// prompt as a positional arg (claude, codex, hermes, ggcoder, gemini, cline)
/// receive it directly; agents that don't (aider, opencode, shell) launch bare
/// and expect the task to arrive via `agentmaster send`.
pub fn resolve(runtime: &str, task: Option<&str>) -> RuntimeSpec {
    match runtime {
        "claude" => RuntimeSpec {
            program: "claude".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        "codex" => RuntimeSpec {
            program: "codex".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        "hermes" => RuntimeSpec {
            program: "hermes".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        "ggcoder" => RuntimeSpec {
            program: "ggcoder".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        "gemini" => RuntimeSpec {
            program: "gemini".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        "cline" => RuntimeSpec {
            program: "cline".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        // Kimi CLI (1.49+): prompt as positional arg.
        "kimi" => RuntimeSpec {
            program: "kimi".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        // Cursor's Composer agent ships as `cursor-agent`; prompt is positional.
        "cursor" | "cursor-agent" | "composer" => RuntimeSpec {
            program: "cursor-agent".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        // Devin CLI: prompt comes after a `--` separator. Model can be picked
        // in-session via `/model <name>` (35 families incl. Gemini Flash, Kimi,
        // GLM, Grok, GPT, Claude).
        "devin" => RuntimeSpec {
            program: "devin".into(),
            args: task
                .map(|t| vec!["--".to_string(), t.to_string()])
                .unwrap_or_default(),
        },
        // Aider's positionals are files, not a prompt — launch bare and send the
        // task via `agentmaster send`. User can `/add` files inside the TUI.
        "aider" => RuntimeSpec {
            program: "aider".into(),
            args: vec![],
        },
        // OpenCode's positional is a project path, not a prompt — launch bare TUI
        // and send the task via `agentmaster send`.
        "opencode" => RuntimeSpec {
            program: "opencode".into(),
            args: vec![],
        },
        // Kimi-worker: lean Kimi child from agent-token-saver (empty skills dir,
        // exit-75 retry, per-worker KIMI_SHARE_DIR isolation). Passes the task
        // as a positional arg. Falls back to `kimi` if kimi-worker isn't on PATH.
        "kimi-worker" => RuntimeSpec {
            program: "kimi-worker".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        // Claude-escalate: used when a cheaper lane fails and the swarm
        // escalates to Claude for a guaranteed-quality retry. Same binary
        // as `claude` but tagged differently in the audit log so the
        // swarm can distinguish "primary Claude lane" from "escalation".
        "claude-escalate" => RuntimeSpec {
            program: "claude".into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
        "shell" | "sh" | "" => {
            let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            RuntimeSpec {
                program: sh,
                args: vec![],
            }
        }
        // Unknown name: treat it as a bare program, task as its args.
        other => RuntimeSpec {
            program: other.into(),
            args: task.map(|t| vec![t.to_string()]).unwrap_or_default(),
        },
    }
}

/// All runtimes agentmaster knows how to launch. Used by `doctor` for the
/// runtime-availability check and by the TUI's new-agent completer.
pub const KNOWN_RUNTIMES: &[&str] = &[
    "claude",
    "codex",
    "hermes",
    "ggcoder",
    "aider",
    "opencode",
    "gemini",
    "cline",
    "kimi",
    "cursor-agent",
    "devin",
    "kimi-worker",
    "claude-escalate",
    "shell",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devin_prompt_after_separator() {
        let spec = resolve("devin", Some("fix tests"));
        assert_eq!(spec.program, "devin");
        assert_eq!(spec.args, vec!["--".to_string(), "fix tests".to_string()]);
    }

    #[test]
    fn devin_without_task_launches_bare() {
        let spec = resolve("devin", None);
        assert_eq!(spec.program, "devin");
        assert!(spec.args.is_empty());
    }

    #[test]
    fn composer_aliases_map_to_cursor_agent() {
        for alias in ["cursor", "cursor-agent", "composer"] {
            assert_eq!(resolve(alias, None).program, "cursor-agent");
        }
    }

    #[test]
    fn known_runtimes_include_new_clis() {
        for rt in ["kimi", "cursor-agent", "devin"] {
            assert!(KNOWN_RUNTIMES.contains(&rt), "{rt} missing");
        }
    }
}

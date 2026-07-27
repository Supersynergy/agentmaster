//! Runtime adapter: map a runtime name to the program + args to launch. This is
//! the seam where Claude Code, Codex, Hermes, ggcoder, Aider, OpenCode, Gemini,
//! Cline, or a plain shell all become "an agent" the board can see and steer.
//! Pluggable by design — adding a CLI is one match arm.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Resolve a runtime and place a verified model selector before its prompt.
/// Keeping this at the command boundary prevents model names from becoming
/// accidental natural-language task text.
pub fn resolve_with_model(runtime: &str, task: Option<&str>, model: Option<&str>) -> RuntimeSpec {
    let mut spec = resolve(runtime, task);
    let Some(model) = model else {
        return spec;
    };
    match runtime {
        // Verified locally against each installed CLI's `--help` on 2026-07-27.
        "claude" | "codex" | "gemini" | "hermes" | "ggcoder" => {
            spec.args
                .splice(0..0, ["--model".to_string(), model.to_string()]);
        }
        // The lean worker's registry entry is fixed to `kimi-k3`; its wrapper
        // accepts alternate aliases only via KIMI_WORKER_MODEL, not argv.
        "kimi-worker" => {}
        _ => {}
    }
    spec
}

/// Resolve a non-interactive runtime command for the parallel swarm engine.
/// These argv contracts are verified against each installed CLI's `--help`;
/// other runtimes retain their existing resolver contract.
pub fn resolve_headless_with_model(
    runtime: &str,
    task: &str,
    model: Option<&str>,
    usage_file: Option<&Path>,
) -> RuntimeSpec {
    let mut args = Vec::new();
    match runtime {
        "claude" | "claude-escalate" => {
            args.push("--print".to_string());
            push_model(&mut args, model);
            args.push(task.to_string());
            RuntimeSpec {
                program: "claude".into(),
                args,
            }
        }
        "codex" => {
            args.push("exec".to_string());
            push_model(&mut args, model);
            args.push(task.to_string());
            RuntimeSpec {
                program: "codex".into(),
                args,
            }
        }
        "gemini" => {
            push_model(&mut args, model);
            args.extend(["--prompt".to_string(), task.to_string()]);
            RuntimeSpec {
                program: "gemini".into(),
                args,
            }
        }
        "hermes" => {
            push_model(&mut args, model);
            args.extend(["-z".to_string(), task.to_string(), "--cli".to_string()]);
            if let Some(path) = usage_file {
                args.extend([
                    "--usage-file".to_string(),
                    path.to_string_lossy().into_owned(),
                ]);
            }
            RuntimeSpec {
                program: "hermes".into(),
                args,
            }
        }
        "ggcoder" => {
            push_model(&mut args, model);
            args.extend(["--json".to_string(), task.to_string()]);
            RuntimeSpec {
                program: "ggcoder".into(),
                args,
            }
        }
        _ => resolve_with_model(runtime, Some(task), model),
    }
}

fn push_model(args: &mut Vec<String>, model: Option<&str>) {
    if let Some(model) = model {
        args.extend(["--model".to_string(), model.to_string()]);
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
    fn model_flag_precedes_the_prompt_for_supported_runtimes() {
        let spec = resolve_with_model("codex", Some("fix tests"), Some("gpt-5.6"));
        assert_eq!(spec.args, ["--model", "gpt-5.6", "fix tests"]);
        let worker = resolve_with_model("kimi-worker", Some("fix tests"), Some("kimi-k3"));
        assert_eq!(worker.args, ["fix tests"]);
    }

    #[test]
    fn headless_resolver_uses_help_verified_argv_contracts() {
        let codex = resolve_headless_with_model("codex", "fix tests", Some("gpt-5.6"), None);
        assert_eq!(codex.args, ["exec", "--model", "gpt-5.6", "fix tests"]);

        let gemini = resolve_headless_with_model("gemini", "fix tests", Some("gemini-flash"), None);
        assert_eq!(
            gemini.args,
            ["--model", "gemini-flash", "--prompt", "fix tests"]
        );

        let usage = Path::new("/tmp/hermes usage.json");
        let hermes =
            resolve_headless_with_model("hermes", "fix tests", Some("kimi-k3"), Some(usage));
        assert_eq!(
            hermes.args,
            [
                "--model",
                "kimi-k3",
                "-z",
                "fix tests",
                "--cli",
                "--usage-file",
                "/tmp/hermes usage.json"
            ]
        );
    }

    #[test]
    fn headless_claude_and_ggcoder_disable_interactive_ui() {
        let claude = resolve_headless_with_model("claude", "fix tests", Some("sonnet"), None);
        assert_eq!(claude.args, ["--print", "--model", "sonnet", "fix tests"]);

        let ggcoder = resolve_headless_with_model("ggcoder", "fix tests", Some("qwen"), None);
        assert_eq!(ggcoder.args, ["--model", "qwen", "--json", "fix tests"]);
    }

    #[test]
    fn headless_hermes_never_blanket_accepts_hooks() {
        let spec = resolve_headless_with_model("hermes", "task", None, None);
        assert!(!spec.args.iter().any(|arg| arg == "--accept-hooks"));
    }

    #[test]
    fn known_runtimes_include_new_clis() {
        for rt in ["kimi", "cursor-agent", "devin"] {
            assert!(KNOWN_RUNTIMES.contains(&rt), "{rt} missing");
        }
    }
}

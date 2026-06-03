//! Runtime adapter: map a runtime name to the program + args to launch. This is
//! the seam where Claude Code, Codex, or a plain shell all become "an agent" the
//! board can see and steer. Pluggable by design — adding a CLI is one match arm.

pub struct RuntimeSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Resolve a runtime to a launchable command. `task`, when present, is passed as
/// the agent's initial instruction where the CLI supports it.
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

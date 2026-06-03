//! Orchestrator bridge: the cmux-meta-orchestrator features, native in Rust.
//! Session discovery/cold-start passes through to `session-restore` (sr) so the
//! distill+seed machinery stays single-sourced; fan-out (`batch`) spawns one
//! seeded cmux workspace per task. No Python, no extra runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

fn sr_bin() -> PathBuf {
    std::env::var("SR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/bin/session-restore")
        })
}

fn cmux_bin() -> String {
    std::env::var("CMUX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/bin/cmux")
    })
}

/// Passthrough to `sr` (inherits stdio so ANSI + the cmux-spawn side effect work).
fn run_sr(args: &[&str]) -> Result<()> {
    let bin = sr_bin();
    if !bin.exists() {
        bail!(
            "session-restore not found: {} (set SR_BIN=…)",
            bin.display()
        );
    }
    let status = Command::new(&bin).args(args).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

/// `find <query>` — content search over ALL distilled session states, ranked.
pub fn find(query: &str) -> Result<()> {
    run_sr(&["find", query])
}

/// `dash [--all] [query]` — grouped session dashboard (project → sessions).
pub fn dash(all: bool, query: Option<&str>) -> Result<()> {
    let mut args: Vec<&str> = Vec::new();
    if all {
        args.push("--all");
    }
    args.push("dash");
    if let Some(q) = query {
        args.push(q);
    }
    run_sr(&args)
}

/// `start <id>` — cold-start a distilled session: seeded cmux workspace by
/// default, or in the current terminal with `here`.
pub fn start(id: &str, here: bool, focus: bool) -> Result<()> {
    if here {
        run_sr(&["go", id])
    } else if focus {
        run_sr(&["cmux", id, "--focus"])
    } else {
        run_sr(&["cmux", id])
    }
}

/// One fan-out unit parsed from a tasks file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    pub workdir: String,
    pub prompt: String,
    pub model: Option<String>,
}

/// Parse a tasks file. `.json` = a list, or `{ "tasks": [...] }`; anything else =
/// Markdown `## name` sections with `- key: value` lines and a free-text prompt
/// body (the claude-fleet convention).
pub fn load_tasks(path: &Path) -> Result<Vec<Task>> {
    let text = std::fs::read_to_string(path)?;
    let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
    let tasks = if is_json {
        parse_json_tasks(&text)?
    } else {
        parse_md_tasks(&text)
    };
    if tasks.is_empty() {
        bail!("no tasks parsed from {}", path.display());
    }
    Ok(tasks)
}

fn parse_json_tasks(text: &str) -> Result<Vec<Task>> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(text)?;
    let arr = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .get("tasks")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for (i, t) in arr.iter().enumerate() {
        let g = |k: &str| t.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let name = g("name")
            .or_else(|| g("id"))
            .unwrap_or(format!("task-{}", i + 1));
        let workdir = g("workdir")
            .or_else(|| g("cwd"))
            .unwrap_or_else(|| ".".into());
        let prompt = g("prompt").unwrap_or_default();
        out.push(Task {
            name,
            workdir,
            prompt,
            model: g("model"),
        });
    }
    Ok(out)
}

fn parse_md_tasks(text: &str) -> Vec<Task> {
    let mut tasks = Vec::new();
    let mut name = String::new();
    let mut workdir = String::from(".");
    let mut model: Option<String> = None;
    let mut body: Vec<String> = Vec::new();
    let mut open = false;

    let flush = |name: &str,
                 workdir: &str,
                 model: &Option<String>,
                 body: &[String],
                 tasks: &mut Vec<Task>| {
        if name.is_empty() {
            return;
        }
        tasks.push(Task {
            name: name.to_string(),
            workdir: workdir.to_string(),
            prompt: body.join("\n").trim().to_string(),
            model: model.clone(),
        });
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if open {
                flush(&name, &workdir, &model, &body, &mut tasks);
            }
            open = true;
            name = rest.trim().to_string();
            workdir = ".".into();
            model = None;
            body.clear();
        } else if open {
            let t = line.trim_start();
            if let Some(kv) = t.strip_prefix("- ")
                && let Some((k, v)) = kv.split_once(':')
            {
                match k.trim().to_lowercase().as_str() {
                    "workdir" | "cwd" => workdir = v.trim().to_string(),
                    "model" => model = Some(v.trim().to_string()),
                    "prompt" => body.push(v.trim().to_string()),
                    _ => {}
                }
            } else if !line.trim().is_empty() {
                body.push(line.to_string());
            }
        }
    }
    if open {
        flush(&name, &workdir, &model, &body, &mut tasks);
    }
    tasks
}

fn expand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Fan-out: spawn one fresh cmux workspace per task, each launching `claude`
/// seeded with the task prompt. Dry-run prints the plan; `yes` actually spawns.
/// Returns how many were spawned (0 on dry-run).
pub fn batch(tasks: &[Task], default_model: Option<&str>, yes: bool) -> Result<usize> {
    let cmux = cmux_bin();
    let claude = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let mut spawned = 0;
    for (i, t) in tasks.iter().enumerate() {
        if t.prompt.trim().is_empty() {
            println!("skip {}: empty prompt", t.name);
            continue;
        }
        let workdir = expand(&t.workdir);
        let model = t.model.as_deref().or(default_model);
        let mut launch = vec![claude.clone()];
        if let Some(m) = model {
            launch.push("--model".into());
            launch.push(m.to_string());
        }
        launch.push(t.prompt.clone());
        let launch_cmd = launch.join(" ");
        if !yes {
            println!(
                "[dry] {:02} {}  cwd={}  model={}",
                i + 1,
                t.name,
                workdir,
                model.unwrap_or("default")
            );
            continue;
        }
        let out = Command::new(&cmux)
            .args([
                "new-workspace",
                "--name",
                &t.name,
                "--cwd",
                &workdir,
                "--command",
                &launch_cmd,
            ])
            .output()?;
        let first = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("spawned")
            .to_string();
        println!("{:02} {}: {}", i + 1, t.name, first);
        spawned += 1;
    }
    if yes {
        println!("\nspawned {spawned}/{} workspace(s).", tasks.len());
    } else {
        println!("\ndry-run: {} task(s). Add --yes to spawn.", tasks.len());
    }
    Ok(spawned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_md_tasks() {
        let s = "## fix-parser\n- cwd: ~/p/app\n- model: opus\nmake the parser robust\nand fast\n\n## docs\nwrite the readme\n";
        let t = parse_md_tasks(s);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].name, "fix-parser");
        assert_eq!(t[0].workdir, "~/p/app");
        assert_eq!(t[0].model.as_deref(), Some("opus"));
        assert!(t[0].prompt.contains("robust"));
        assert_eq!(t[1].name, "docs");
        assert_eq!(t[1].workdir, ".");
        assert_eq!(t[1].prompt, "write the readme");
    }

    #[test]
    fn parses_json_tasks() {
        let s = r#"{"tasks":[{"name":"a","prompt":"do a","cwd":"/t"},{"id":"b","prompt":"do b"}]}"#;
        let t = parse_json_tasks(s).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].name, "a");
        assert_eq!(t[0].workdir, "/t");
        assert_eq!(t[1].name, "b");
        assert_eq!(t[1].workdir, ".");
    }
}

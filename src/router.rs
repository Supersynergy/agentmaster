//! Model registry + task classifier + auto-router.
//!
//! The single seam where "which model for this task?" becomes a machine
//! decision, not a human guess. Static table of 2026-07 model pricing +
//! strengths/weaknesses, a keyword-based task classifier, and pick functions
//! that return the cheapest passing model for a task type — or a diverse
//! swarm of N models for oracle-gated convergence.
//!
//! Ponytail: no external API, no runtime discovery, no framework. One table,
//! one classifier, one pick function. Adding a model = one row.

use std::fmt;

/// What kind of work is the task? Determines which model strengths matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskType {
    CodeGeneration,
    CodeReview,
    Planning,
    Research,
    ShellProjection,
    LongContext,
    Verification,
    Creative,
    General,
}

impl fmt::Display for TaskType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskType::CodeGeneration => write!(f, "code_generation"),
            TaskType::CodeReview => write!(f, "code_review"),
            TaskType::Planning => write!(f, "planning"),
            TaskType::Research => write!(f, "research"),
            TaskType::ShellProjection => write!(f, "shell_projection"),
            TaskType::LongContext => write!(f, "long_context"),
            TaskType::Verification => write!(f, "verification"),
            TaskType::Creative => write!(f, "creative"),
            TaskType::General => write!(f, "general"),
        }
    }
}

/// Pricing tier — used for cost-ladder sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    OpenWeight,
    Mid,
    Frontier,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tier::OpenWeight => write!(f, "open_weight"),
            Tier::Mid => write!(f, "mid_tier"),
            Tier::Frontier => write!(f, "frontier_closed"),
        }
    }
}

/// A model the router can pick. `runtime` is the agentmaster runtime name
/// (claude/codex/hermes/gemini/...); `model_flag` is the `--model` value
/// passed to that runtime. Prices are USD per 1M tokens (input/output).
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub name: &'static str,
    pub runtime: &'static str,
    pub model_flag: &'static str,
    pub vendor: &'static str,
    pub tier: Tier,
    pub input_price: f64,
    pub output_price: f64,
    pub context_window: usize,
    pub strengths: &'static [&'static str],
    pub weaknesses: &'static [&'static str],
    /// Task types this model is strong at. Used by `pick`.
    pub best_for: &'static [TaskType],
}

impl ModelSpec {
    /// Blended cost per 1M tokens assuming 3:1 input:output ratio (typical
    /// coding task). Used for cost-ladder sorting only — not a billing claim.
    pub fn blended_cost(&self) -> f64 {
        (self.input_price * 3.0 + self.output_price) / 4.0
    }
}

/// The 2026-07 model registry. Prices are USD per 1M tokens (input/output),
/// sourced from the token-cfo pricing table (2026-07-23). Adding a model =
/// one row. The router never reads from a remote API — this table is the
/// single source of truth for cost-ladder decisions.
pub static REGISTRY: &[ModelSpec] = &[
    // --- Frontier (closed) --------------------------------------------------
    ModelSpec {
        name: "claude-fable-5",
        runtime: "claude",
        model_flag: "claude-fable-5",
        vendor: "Anthropic",
        tier: Tier::Frontier,
        input_price: 15.0,
        output_price: 75.0,
        context_window: 200_000,
        strengths: &[
            "deep reasoning",
            "agentic loops",
            "code generation",
            "long-context synthesis",
        ],
        weaknesses: &["expensive", "slow for trivial tasks"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::Planning,
            TaskType::LongContext,
            TaskType::CodeReview,
        ],
    },
    ModelSpec {
        name: "gpt-5.6",
        runtime: "codex",
        model_flag: "gpt-5.6",
        vendor: "OpenAI",
        tier: Tier::Frontier,
        input_price: 10.0,
        output_price: 40.0,
        context_window: 400_000,
        strengths: &[
            "code generation",
            "tool use",
            "fast frontier",
            "wide knowledge",
        ],
        weaknesses: &["expensive", "less agentic than Claude"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::Research,
            TaskType::Planning,
        ],
    },
    ModelSpec {
        name: "gemini-3.1-pro",
        runtime: "gemini",
        model_flag: "gemini-3.1-pro",
        vendor: "Google",
        tier: Tier::Frontier,
        input_price: 7.0,
        output_price: 21.0,
        context_window: 2_000_000,
        strengths: &["longest context (2M)", "multimodal", "cheap frontier"],
        weaknesses: &["less agentic", "tool use weaker than Claude"],
        best_for: &[
            TaskType::LongContext,
            TaskType::Research,
            TaskType::Creative,
        ],
    },
    // --- Mid-tier -----------------------------------------------------------
    ModelSpec {
        name: "claude-sonnet-4.6",
        runtime: "claude",
        model_flag: "claude-sonnet-4.6",
        vendor: "Anthropic",
        tier: Tier::Mid,
        input_price: 3.0,
        output_price: 15.0,
        context_window: 200_000,
        strengths: &["agentic", "code review", "balanced cost/quality"],
        weaknesses: &["weaker than Fable on deep reasoning"],
        best_for: &[
            TaskType::CodeReview,
            TaskType::CodeGeneration,
            TaskType::Verification,
        ],
    },
    ModelSpec {
        name: "gpt-5.6-mini",
        runtime: "codex",
        model_flag: "gpt-5.6-mini",
        vendor: "OpenAI",
        tier: Tier::Mid,
        input_price: 0.40,
        output_price: 1.60,
        context_window: 200_000,
        strengths: &["very cheap mid-tier", "fast", "good for shell projection"],
        weaknesses: &["weaker on complex code"],
        best_for: &[
            TaskType::ShellProjection,
            TaskType::Verification,
            TaskType::General,
        ],
    },
    ModelSpec {
        name: "gemini-3.1-flash",
        runtime: "gemini",
        model_flag: "gemini-3.1-flash",
        vendor: "Google",
        tier: Tier::Mid,
        input_price: 0.35,
        output_price: 1.50,
        context_window: 1_000_000,
        strengths: &["cheapest mid-tier", "1M context", "fast"],
        weaknesses: &["weaker agentic", "less reliable for code gen"],
        best_for: &[
            TaskType::Research,
            TaskType::LongContext,
            TaskType::ShellProjection,
        ],
    },
    // --- Open-weight (cheapest lanes) ---------------------------------------
    ModelSpec {
        name: "glm-5.2",
        runtime: "hermes",
        model_flag: "glm-5.2",
        vendor: "Zhipu",
        tier: Tier::OpenWeight,
        input_price: 0.30,
        output_price: 1.10,
        context_window: 128_000,
        strengths: &["cheap", "good code generation", "open-weight"],
        weaknesses: &["weaker agentic", "smaller context"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::ShellProjection,
            TaskType::General,
        ],
    },
    ModelSpec {
        name: "deepseek-v4.5",
        runtime: "hermes",
        model_flag: "deepseek-v4.5",
        vendor: "DeepSeek",
        tier: Tier::OpenWeight,
        input_price: 0.28,
        output_price: 1.10,
        context_window: 128_000,
        strengths: &["cheapest strong coder", "open-weight", "good reasoning"],
        weaknesses: &["weaker agentic", "smaller context"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::CodeReview,
            TaskType::ShellProjection,
        ],
    },
    ModelSpec {
        name: "kimi-k3",
        runtime: "hermes",
        model_flag: "kimi-k3",
        vendor: "Moonshot",
        tier: Tier::OpenWeight,
        input_price: 0.20,
        output_price: 0.80,
        context_window: 256_000,
        strengths: &[
            "cheapest lane",
            "long context (256k)",
            "good for shell projection",
        ],
        weaknesses: &["weakest reasoning", "not for complex code gen"],
        best_for: &[
            TaskType::ShellProjection,
            TaskType::General,
            TaskType::Verification,
        ],
    },
    ModelSpec {
        name: "kimi-worker",
        runtime: "kimi-worker",
        model_flag: "kimi-k3",
        vendor: "Moonshot",
        tier: Tier::OpenWeight,
        input_price: 0.20,
        output_price: 0.80,
        context_window: 256_000,
        strengths: &[
            "lean Kimi child (empty skills dir, exit-75 retry, KIMI_SHARE_DIR isolation)",
            "cheapest passing lane for shell projection",
            "measured 16% of Claude team gross input",
        ],
        weaknesses: &[
            "weakest reasoning",
            "not for complex code gen",
            "requires agent-token-saver install",
        ],
        best_for: &[
            TaskType::ShellProjection,
            TaskType::Verification,
            TaskType::General,
        ],
    },
    ModelSpec {
        name: "qwen-3-coder",
        runtime: "ggcoder",
        model_flag: "qwen-3-coder",
        vendor: "Alibaba",
        tier: Tier::OpenWeight,
        input_price: 0.25,
        output_price: 0.90,
        context_window: 256_000,
        strengths: &[
            "strong code generation",
            "open-weight",
            "long context (256k)",
            "tool use",
        ],
        weaknesses: &["weaker agentic than Claude", "less ecosystem than DeepSeek"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::CodeReview,
            TaskType::LongContext,
        ],
    },
];

/// Classify a task by keywords. Ponytail: keyword match, no ML, no API call.
/// The classifier is intentionally conservative — when in doubt, `General`
/// routes to the cheapest passing mid-tier model.
pub fn classify(task: &str) -> TaskType {
    let t = task.to_lowercase();
    let has = |k: &str| t.contains(k);

    // Long-context signals: explicit large-file/refactor mentions.
    if has("entire repo")
        || has("whole codebase")
        || has("large file")
        || has("refactor all")
        || has("audit all")
        || has("survey all")
        || has("1m context")
        || has("long context")
    {
        return TaskType::LongContext;
    }
    // Shell projection: deterministic, scriptable, no reasoning.
    if has("shell")
        || has("bash")
        || has("git ")
        || has("grep ")
        || has("awk ")
        || has("sed ")
        || has("curl ")
        || has("make ")
        || has("just ")
        || has("cargo ")
        || has("npm ")
        || has("pnpm ")
        || has("bun ")
        || has("run command")
        || has("execute")
        || has("script")
    {
        return TaskType::ShellProjection;
    }
    // Code review / verification.
    if has("review")
        || has("audit code")
        || has("verify")
        || has("check tests")
        || has("lint")
        || has("refute")
        || has("cross-check")
        || has("metareview")
    {
        return TaskType::CodeReview;
    }
    if has("test")
        || has("oracle")
        || has("pass/fail")
        || has("tsc --noemit")
        || has("cargo test")
        || has("bun test")
    {
        return TaskType::Verification;
    }
    // Planning / architecture.
    if has("plan")
        || has("architect")
        || has("design")
        || has("spec")
        || has("adr")
        || has("decompose")
        || has("roadmap")
        || has("strategy")
    {
        return TaskType::Planning;
    }
    // Research.
    if has("research")
        || has("find ")
        || has("search ")
        || has("compare")
        || has("analyze")
        || has("investigate")
        || has("survey")
    {
        return TaskType::Research;
    }
    // Creative.
    if has("write")
        || has("copy")
        || has("headline")
        || has("landing page")
        || has("marketing")
        || has("brand")
        || has("story")
        || has("content")
    {
        return TaskType::Creative;
    }
    // Code generation: explicit code verbs.
    if has("implement")
        || has("build")
        || has("create")
        || has("add feature")
        || has("fix bug")
        || has("refactor")
        || has("port")
        || has("migrate")
        || has("generate code")
        || has("write function")
        || has("write script")
    {
        return TaskType::CodeGeneration;
    }
    TaskType::General
}

/// Pick the cheapest model that's strong at the given task type.
/// Falls back to the globally cheapest model if no model lists the task type
/// in `best_for`.
pub fn pick(task_type: TaskType) -> &'static ModelSpec {
    let mut candidates: Vec<&ModelSpec> = REGISTRY
        .iter()
        .filter(|m| m.best_for.contains(&task_type))
        .collect();
    if candidates.is_empty() {
        candidates = REGISTRY.iter().collect();
    }
    candidates.sort_by(|a, b| {
        a.blended_cost()
            .partial_cmp(&b.blended_cost())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.first().expect("REGISTRY is non-empty")
}

/// Pick a diverse swarm of N models for the same task. Strategy:
/// - Always include the cheapest passing model (cost floor).
/// - Add one mid-tier and one frontier model if N >= 3 (diversity = robustness).
/// - Never duplicate runtimes within the swarm (different agents = different
///   failure modes).
pub fn pick_swarm(task_type: TaskType, n: usize) -> Vec<&'static ModelSpec> {
    let n = n.min(REGISTRY.len());
    let mut by_tier: [Vec<&ModelSpec>; 3] = [
        REGISTRY
            .iter()
            .filter(|m| m.tier == Tier::OpenWeight)
            .collect(),
        REGISTRY.iter().filter(|m| m.tier == Tier::Mid).collect(),
        REGISTRY
            .iter()
            .filter(|m| m.tier == Tier::Frontier)
            .collect(),
    ];
    for lane in &mut by_tier {
        lane.sort_by(|a, b| {
            a.blended_cost()
                .partial_cmp(&b.blended_cost())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let mut out: Vec<&'static ModelSpec> = Vec::new();
    let mut used_runtimes: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();
    let mut used_names: std::collections::HashSet<&'static str> = std::collections::HashSet::new();

    // Lane 1: cheapest passing model (cost floor).
    let cheapest = pick(task_type);
    out.push(cheapest);
    used_runtimes.insert(cheapest.runtime);
    used_names.insert(cheapest.name);

    // Lane 2: cheapest mid-tier with a different runtime.
    if n >= 2
        && let Some(m) = by_tier[1]
            .iter()
            .find(|m| !used_runtimes.contains(m.runtime))
    {
        out.push(m);
        used_runtimes.insert(m.runtime);
        used_names.insert(m.name);
    }

    // Lane 3: cheapest frontier with a different runtime.
    if n >= 3
        && let Some(m) = by_tier[2]
            .iter()
            .find(|m| !used_runtimes.contains(m.runtime))
    {
        out.push(m);
        used_runtimes.insert(m.runtime);
        used_names.insert(m.name);
    }

    // Fill remaining slots: first try distinct runtimes (any tier, cheapest
    // first); once all runtimes are used, allow duplicate runtimes with
    // different models (cheapest unused model first). This lets a 10-entity
    // jury fill all 10 slots instead of capping at distinct runtimes.
    if out.len() < n {
        let mut all: Vec<&ModelSpec> = REGISTRY.iter().collect();
        all.sort_by(|a, b| {
            a.blended_cost()
                .partial_cmp(&b.blended_cost())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Pass 1: distinct runtimes.
        for m in all.iter() {
            if out.len() >= n {
                break;
            }
            if !used_runtimes.contains(m.runtime) && !used_names.contains(m.name) {
                out.push(*m);
                used_runtimes.insert(m.runtime);
                used_names.insert(m.name);
            }
        }
        // Pass 2: duplicate runtimes OK, different models only.
        for m in all.iter() {
            if out.len() >= n {
                break;
            }
            if !used_names.contains(m.name) {
                out.push(*m);
                used_names.insert(m.name);
            }
        }
    }

    out.truncate(n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_shell_projection() {
        assert_eq!(
            classify("run cargo test and report"),
            TaskType::ShellProjection
        );
        assert_eq!(classify("git log --oneline"), TaskType::ShellProjection);
    }

    #[test]
    fn classify_code_generation() {
        assert_eq!(
            classify("implement the parser fix"),
            TaskType::CodeGeneration
        );
        assert_eq!(classify("build the new feature"), TaskType::CodeGeneration);
    }

    #[test]
    fn classify_long_context() {
        assert_eq!(
            classify("audit the entire repo for dead code"),
            TaskType::LongContext
        );
    }

    #[test]
    fn classify_planning() {
        assert_eq!(
            classify("design the architecture for the new system"),
            TaskType::Planning
        );
    }

    #[test]
    fn classify_general_fallback() {
        assert_eq!(classify("hello world"), TaskType::General);
    }

    #[test]
    fn pick_returns_cheapest_passing() {
        let m = pick(TaskType::ShellProjection);
        // Kimi K3 is the cheapest model that lists ShellProjection in best_for.
        assert_eq!(m.name, "kimi-k3");
    }

    #[test]
    fn pick_code_generation_prefers_cheap() {
        let m = pick(TaskType::CodeGeneration);
        // Qwen-3-Coder is the cheapest model strong at code generation
        // (blend $0.41/M vs DeepSeek V4.5 $0.49/M).
        assert_eq!(m.name, "qwen-3-coder");
    }

    #[test]
    fn pick_swarm_returns_diverse_runtimes() {
        let swarm = pick_swarm(TaskType::CodeGeneration, 3);
        assert_eq!(swarm.len(), 3);
        let runtimes: std::collections::HashSet<&str> = swarm.iter().map(|m| m.runtime).collect();
        // All 3 must be different runtimes (diversity = robustness).
        assert_eq!(
            runtimes.len(),
            3,
            "swarm must have diverse runtimes: {:?}",
            runtimes
        );
    }

    #[test]
    fn pick_swarm_includes_cheapest_first() {
        let swarm = pick_swarm(TaskType::ShellProjection, 3);
        assert!(!swarm.is_empty());
        // First entry is always the cheapest passing model.
        assert_eq!(swarm[0].name, "kimi-k3");
    }

    #[test]
    fn pick_swarm_n_capped_at_registry_size() {
        let swarm = pick_swarm(TaskType::General, 100);
        assert!(swarm.len() <= REGISTRY.len());
    }

    #[test]
    fn pick_swarm_10_fills_all_slots() {
        // A 10-entity jury requests 10 and gets 10 (REGISTRY has 11 models,
        // so 10 is within bounds). The key invariant: the swarm does NOT
        // cap at 4 distinct runtimes — it fills all 10 requested slots.
        let swarm = pick_swarm(TaskType::CodeGeneration, 10);
        assert_eq!(
            swarm.len(),
            10,
            "10-entity jury must fill all 10 requested slots"
        );
        // All entries must be distinct models (no exact duplicates).
        let names: std::collections::HashSet<&str> = swarm.iter().map(|m| m.name).collect();
        assert_eq!(
            names.len(),
            10,
            "all 10 lanes must be distinct models: {:?}",
            names
        );
    }

    #[test]
    fn pick_swarm_10_first_three_distinct_runtimes() {
        // The first 3 lanes should still prefer distinct runtimes for
        // diversity (cheapest + mid-tier + frontier, different runtimes).
        let swarm = pick_swarm(TaskType::CodeGeneration, 10);
        let first3: std::collections::HashSet<&str> =
            swarm.iter().take(3).map(|m| m.runtime).collect();
        assert_eq!(
            first3.len(),
            3,
            "first 3 lanes must be distinct runtimes: {:?}",
            first3
        );
    }

    #[test]
    fn blended_cost_sorts_correctly() {
        let mut all: Vec<&ModelSpec> = REGISTRY.iter().collect();
        all.sort_by(|a, b| {
            a.blended_cost()
                .partial_cmp(&b.blended_cost())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Kimi K3 is the cheapest, Claude Fable 5 is the most expensive.
        assert_eq!(all.first().unwrap().name, "kimi-k3");
        assert_eq!(all.last().unwrap().name, "claude-fable-5");
    }
}

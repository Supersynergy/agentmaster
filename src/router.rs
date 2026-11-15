//! Model registry + task classifier + auto-router.
//!
//! The single seam where "which model for this task?" becomes a machine
//! decision, not a human guess. Dated table of provider API pricing +
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
    pub price_source: &'static str,
    pub price_basis: &'static str,
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

/// Published rates for uncached text, in USD per million tokens.
/// Tariffs are planning inputs. A local CLI subscription can bill differently.
pub const PRICING_DATE: &str = "2026-11-15";
pub static REGISTRY: &[ModelSpec] = &[
    ModelSpec {
        name: "gpt-6-astra",
        runtime: "codex",
        model_flag: "gpt-6-astra",
        vendor: "OpenAI",
        tier: Tier::Frontier,
        price_source: "https://developers.openai.com/api/docs/pricing",
        price_basis: "Standard, short context (up to 272K input tokens)",
        input_price: 10.0,
        output_price: 50.0,
        context_window: 1_050_000,
        strengths: &["multi-step reasoning"],
        weaknesses: &["higher latency"],
        best_for: &[
            TaskType::Planning,
            TaskType::CodeReview,
            TaskType::LongContext,
        ],
    },
    ModelSpec {
        name: "gpt-6.1-sol",
        runtime: "codex",
        model_flag: "gpt-6.1-sol",
        vendor: "OpenAI",
        tier: Tier::Frontier,
        price_source: "https://developers.openai.com/api/docs/pricing",
        price_basis: "Standard, short context (up to 272K input tokens)",
        input_price: 2.0,
        output_price: 10.0,
        context_window: 1_050_000,
        strengths: &["software engineering"],
        weaknesses: &["higher rate than Luna"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::CodeReview,
            TaskType::Planning,
        ],
    },
    ModelSpec {
        name: "gpt-6.1-terra",
        runtime: "codex",
        model_flag: "gpt-6.1-terra",
        vendor: "OpenAI",
        tier: Tier::Mid,
        price_source: "https://developers.openai.com/api/docs/pricing",
        price_basis: "Standard, short context (up to 272K input tokens)",
        input_price: 1.0,
        output_price: 5.0,
        context_window: 1_050_000,
        strengths: &["visual reasoning and scoped edits"],
        weaknesses: &["complex planning may need a larger model"],
        best_for: &[TaskType::CodeGeneration, TaskType::Verification],
    },
    ModelSpec {
        name: "gpt-6-luna",
        runtime: "codex",
        model_flag: "gpt-6-luna",
        vendor: "OpenAI",
        tier: Tier::Mid,
        price_source: "https://developers.openai.com/api/docs/pricing",
        price_basis: "Standard, short context (up to 272K input tokens)",
        input_price: 0.1,
        output_price: 0.5,
        context_window: 1_050_000,
        strengths: &["focused high-volume tasks"],
        weaknesses: &["limited suitability for complex planning"],
        best_for: &[
            TaskType::ShellProjection,
            TaskType::Verification,
            TaskType::General,
        ],
    },
    ModelSpec {
        name: "claude-fable-5-2",
        runtime: "claude",
        model_flag: "claude-fable-5-2",
        vendor: "Anthropic",
        tier: Tier::Frontier,
        price_source: "https://platform.claude.com/docs/en/about-claude/pricing",
        price_basis: "Base tokens, standard inference",
        input_price: 8.0,
        output_price: 40.0,
        context_window: 1_000_000,
        strengths: &["long-horizon reasoning"],
        weaknesses: &["higher cost"],
        best_for: &[TaskType::Planning, TaskType::LongContext],
    },
    ModelSpec {
        name: "claude-opus-5-5",
        runtime: "claude",
        model_flag: "claude-opus-5-5",
        vendor: "Anthropic",
        tier: Tier::Frontier,
        price_source: "https://platform.claude.com/docs/en/about-claude/pricing",
        price_basis: "Base tokens, standard inference",
        input_price: 4.0,
        output_price: 20.0,
        context_window: 1_000_000,
        strengths: &["long-running agentic coding"],
        weaknesses: &["higher cost"],
        best_for: &[
            TaskType::Planning,
            TaskType::CodeGeneration,
            TaskType::CodeReview,
        ],
    },
    ModelSpec {
        name: "claude-sonnet-5-5",
        runtime: "claude",
        model_flag: "claude-sonnet-5-5",
        vendor: "Anthropic",
        tier: Tier::Mid,
        price_source: "https://platform.claude.com/docs/en/about-claude/pricing",
        price_basis: "Base tokens, standard inference",
        input_price: 2.0,
        output_price: 10.0,
        context_window: 1_000_000,
        strengths: &["coding and review"],
        weaknesses: &["higher rate than Haiku"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::CodeReview,
            TaskType::Creative,
        ],
    },
    ModelSpec {
        name: "claude-haiku-5-5",
        runtime: "claude",
        model_flag: "claude-haiku-5-5",
        vendor: "Anthropic",
        tier: Tier::Mid,
        price_source: "https://platform.claude.com/docs/en/about-claude/pricing",
        price_basis: "Base tokens, standard inference, up to 100K input tokens",
        input_price: 0.1,
        output_price: 0.5,
        context_window: 1_000_000,
        strengths: &["fast extraction and classification"],
        weaknesses: &["higher rates above 100K input tokens"],
        best_for: &[TaskType::General, TaskType::Verification],
    },
    ModelSpec {
        name: "gemini-4-carbon",
        runtime: "gemini",
        model_flag: "gemini-4-carbon",
        vendor: "Google",
        tier: Tier::Frontier,
        price_source: "https://ai.google.dev/gemini-api/docs/pricing",
        price_basis: "Standard paid tier through 2026-11-30",
        input_price: 2.0,
        output_price: 10.0,
        context_window: 1_000_000,
        strengths: &["multimodal research and reasoning"],
        weaknesses: &["promotional pricing"],
        best_for: &[
            TaskType::Research,
            TaskType::Planning,
            TaskType::LongContext,
            TaskType::Creative,
        ],
    },
    ModelSpec {
        name: "gemini-4-argon",
        runtime: "gemini",
        model_flag: "gemini-4-argon",
        vendor: "Google",
        tier: Tier::Frontier,
        price_source: "https://ai.google.dev/gemini-api/docs/pricing",
        price_basis: "Standard paid tier through 2026-11-30",
        input_price: 2.0,
        output_price: 10.0,
        context_window: 1_000_000,
        strengths: &["efficient multimodal reasoning"],
        weaknesses: &["promotional pricing"],
        best_for: &[
            TaskType::Research,
            TaskType::CodeGeneration,
            TaskType::LongContext,
        ],
    },
    ModelSpec {
        name: "gemini-4-hydrogen",
        runtime: "gemini",
        model_flag: "gemini-4-hydrogen",
        vendor: "Google",
        tier: Tier::Mid,
        price_source: "https://ai.google.dev/gemini-api/docs/pricing",
        price_basis: "Standard paid tier",
        input_price: 1.5,
        output_price: 7.5,
        context_window: 1_000_000,
        strengths: &["interactive multimodal tasks"],
        weaknesses: &["tool use can incur additional costs"],
        best_for: &[TaskType::Research, TaskType::LongContext, TaskType::General],
    },
    ModelSpec {
        name: "glm-5.2",
        runtime: "hermes",
        model_flag: "glm-5.2",
        vendor: "Z.ai",
        tier: Tier::OpenWeight,
        price_source: "https://docs.z.ai/guides/overview/pricing",
        price_basis: "Standard, uncached input",
        input_price: 1.4,
        output_price: 4.4,
        context_window: 1_000_000,
        strengths: &["long-horizon coding"],
        weaknesses: &["runtime provider must expose this model"],
        best_for: &[TaskType::CodeGeneration, TaskType::Planning],
    },
    ModelSpec {
        name: "deepseek-flash",
        runtime: "hermes",
        model_flag: "deepseek-flash",
        vendor: "DeepSeek",
        tier: Tier::OpenWeight,
        price_source: "https://api-docs.deepseek.com/quick_start/pricing/",
        price_basis: "Peak, cache miss",
        input_price: 0.3,
        output_price: 1.2,
        context_window: 1_000_000,
        strengths: &["coding and fast reasoning"],
        weaknesses: &["price varies by time of day"],
        best_for: &[
            TaskType::CodeGeneration,
            TaskType::CodeReview,
            TaskType::ShellProjection,
        ],
    },
    ModelSpec {
        name: "kimi-k3.1-pro",
        runtime: "hermes",
        model_flag: "kimi-k3.1-pro",
        vendor: "Moonshot",
        tier: Tier::OpenWeight,
        price_source: "https://platform.kimi.ai/docs/pricing/chat",
        price_basis: "Standard Kimi API base tokens",
        input_price: 2.0,
        output_price: 10.0,
        context_window: 1_048_576,
        strengths: &["long-horizon coding"],
        weaknesses: &["cache writes priced separately"],
        best_for: &[
            TaskType::Planning,
            TaskType::CodeGeneration,
            TaskType::LongContext,
        ],
    },
    ModelSpec {
        name: "kimi-worker",
        runtime: "kimi-worker",
        model_flag: "kimi-code/k3",
        vendor: "Moonshot",
        tier: Tier::OpenWeight,
        price_source: "https://platform.kimi.ai/docs/pricing/chat",
        price_basis: "Kimi K3 API reference rate; Kimi Code subscription billed separately",
        input_price: 3.0,
        output_price: 15.0,
        context_window: 1_048_576,
        strengths: &["metered Kimi worker"],
        weaknesses: &["requires a configured Kimi Code K3 alias"],
        best_for: &[
            TaskType::Verification,
            TaskType::ShellProjection,
            TaskType::General,
        ],
    },
    ModelSpec {
        name: "qwen3-coder-plus",
        runtime: "ggcoder",
        model_flag: "qwen3-coder-plus",
        vendor: "Alibaba",
        tier: Tier::Mid,
        price_source: "https://www.alibabacloud.com/help/en/model-studio/model-pricing",
        price_basis: "Singapore international deployment, up to 32K input tokens",
        input_price: 1.0,
        output_price: 5.0,
        context_window: 1_000_000,
        strengths: &["coding and tool use"],
        weaknesses: &["higher rates for longer inputs"],
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
        assert_eq!(m.name, "gpt-6-luna");
    }

    #[test]
    fn pick_code_generation_prefers_cheap() {
        let m = pick(TaskType::CodeGeneration);
        assert_eq!(m.name, "deepseek-flash");
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
        assert_eq!(swarm[0].name, "gpt-6-luna");
    }

    #[test]
    fn pick_swarm_n_capped_at_registry_size() {
        let swarm = pick_swarm(TaskType::General, 100);
        assert!(swarm.len() <= REGISTRY.len());
    }

    #[test]
    fn pick_swarm_10_fills_all_slots() {
        // A 10-entity jury requests 10 and gets 10 (REGISTRY has at least 10 models,
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
        assert_eq!(all.first().unwrap().name, "gpt-6-luna");
        assert_eq!(all.last().unwrap().name, "gpt-6-astra");
    }
}

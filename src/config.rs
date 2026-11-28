use crate::prelude::*;

use anylm::options::Options;
use rigging::Color;

/// Default system prompt.
const SYSTEM_PROMPT: &'static str = "\
System Info:
{SYSTEM_INFO}

Working directory:
{CURRENT_PATH}

Datetime:
* Global (UTC): {DATETIME_GLOBAL}
* Local: {DATETIME_LOCAL}

Use Local time for user responses unless specified otherwise.
Use Global UTC for tool arguments unless a tool explicitly requires another timezone.\
";

/// Default assistant prompt.
const ASSISTANT_PROMPT: &'static str = "\
Role: You are Osy, a smart personal assistant.
Archetype: Pragmatic and exceptionally precise.

Response Rules:
* Language: Match the user's language.
* Format: Polite, concise, structured, and strictly to the point.
* Tone: Calm confidence. Subtle humor is acceptable.
* Substance: Facts, algorithms, and architectural logic only.
* Closing: End with a concise clarifying question or direct next step when appropriate.
* Formatting: Use Markdown (tables, lists, clean structure).

---

Available Skills:
{AGENTS_LIST}

---

TOOL AND RUNTIME USAGE RULES!:

1. JS Runtime:
* Use for pure math and date and time conversion.
* Remember: it has no access to the OS, network, files, or user context.

2. search_fact:
* Relevant user facts are already preloaded in your context when they exist. Do NOT search for them again.
* Call `search_fact` only if a specific personal detail is missing. Write the query in the user's language, 2-6 words.

3. Skills:
* For specialized actions, use STRICTLY only the skills explicitly declared in your current context (never invent skill names).
* If a task requires a skill that is not in the available list, directly inform the user that you lack this capability.\
";

/// Default skill prompt.
const SKILL_PROMPT: &'static str = "\
Role: You are a task executor inside Osy. Your result is read by another assistant or directly by the user.

Response Rules:
* Return ONLY the result: facts, values, status, errors. No greetings, no questions, no persuasion.
* Be as short as possible. Use compact Markdown only when it helps.
* If a tool fails, read the error, fix the arguments and call it again. Report the failure only if it repeats.

---

TOOL AND RUNTIME USAGE RULES!:

1. JS Runtime:
* Use for pure math and date and time conversion.
* Remember: it has no access to the OS, network, files, or user context.

2. search_fact:
* Call `search_fact` only if a specific personal detail is required. Write the query in the user's language.\
";

/// Default control query prompt.
const CONTROL_PROMPT: &'static str = "\
The background tasks/tools have finished. Now write the FINAL response to the user.

CRITICAL INSTRUCTIONS:
1. The user CANNOT see tool outputs or background task logs. You must explicitly present all gathered results, findings, and data in your text answer.
2. NEVER say \"as shown above\", \"already provided\", or refer to previous turns. Act as if you are delivering this information for the very first time.
3. If a task failed, you may retry it once with corrected parameters. If the error repeats, or no tools are left, honestly tell the user what failed and why. Otherwise, deliver the final complete answer.\
";

/// Default compression prompt.
const COMPRESSION_PROMPT: &'static str = "\
Summarize the dialogue history into a clear, structured summary.

Requirements:
* Preserve essential decisions, facts, user constraints, and active task states.
* Output ONLY the summary formatted as a numbered list.
* Omit meta-commentary, introductory text, or explanations about compression.\
";

/// Theme color palette settings (for rigging widgets).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThemeOptions {
    pub brand_color: (u8, u8, u8),
    pub alt_color: (u8, u8, u8),
    pub bg_color: (u8, u8, u8),
    pub blink_color: (u8, u8, u8),
}

impl Default for ThemeOptions {
    fn default() -> Self {
        Self {
            brand_color: (240, 35, 37),
            alt_color: (183, 184, 187),
            bg_color: (6, 12, 20),
            blink_color: (12, 17, 28),
        }
    }
}

impl ThemeOptions {
    /// Returns [Color] struct from `RGB` pattern.
    pub fn rgb_to_color(rgb: (u8, u8, u8)) -> Color {
        Color::Rgb {
            r: rgb.0,
            g: rgb.1,
            b: rgb.2,
        }
    }

    pub fn brand_color(&self) -> Color {
        Self::rgb_to_color(self.brand_color)
    }

    pub fn bg_color(&self) -> Color {
        Self::rgb_to_color(self.bg_color)
    }

    pub fn alt_color(&self) -> Color {
        Self::rgb_to_color(self.alt_color)
    }

    pub fn blink_color(&self) -> Color {
        Self::rgb_to_color(self.blink_color)
    }
}

/// Kernel server options.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerOptions {
    /// Network port for the server to listen on.
    pub port: u16,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self { port: 7878 }
    }
}

/// Execution control options for assistant runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionOptions {
    /// Number of last message pairs that are always loaded without trimming the length.
    pub preserve_pairs: usize,
    /// Maximum number of consecutive retries for failed AI calls (resets after a successful step).
    pub max_retries: usize,
    /// Maximum number of recursive orchestrator cycles per user query.
    pub max_iterations: usize,
    /// Maximum number of LLM steps inside one skill run.
    pub skill_max_iterations: usize,
    /// Maximum total number of AI errors inside one run (protects from endless alternation of fail/success).
    pub max_total_errors: usize,
    /// How many times the same tool call (same name and arguments) is executed within one skill run.
    pub max_identical_calls: usize,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            preserve_pairs: 1,
            max_retries: 3,
            max_iterations: 5,
            skill_max_iterations: 8,
            max_total_errors: 6,
            max_identical_calls: 3,
        }
    }
}

/// JavaScript runtime options.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeOptions {
    /// Maximum number of VM instructions per run.
    pub instruction_limit: Option<u64>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            instruction_limit: Some(5_000_000), // ~20-50ms
        }
    }
}

/// LLM completions/embeddings options.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LLMOptions {
    /// Base system prompt template.
    pub system_prompt: String,
    /// Primary assistant role and behavior prompt.
    pub assist_prompt: String,
    /// Primary skill role and behavior prompt.
    pub skill_prompt: String,
    /// Control prompt for evaluating agent task execution.
    pub control_prompt: String,
    /// Prompt used for summarizing and compressing context.
    pub compress_prompt: String,

    /// Assistant temperature.
    pub assist_temperature: f32,
    /// Agent skill temperature.
    pub skill_temperature: f32,
    /// Chat compession temperature.
    pub compress_temperature: f32,

    /// Completions provider options.
    pub completions: Options,
    /// Embeddings provider options.
    pub embeddings: Options,
}

impl Default for LLMOptions {
    fn default() -> Self {
        Self {
            system_prompt: SYSTEM_PROMPT.trim().into(),
            assist_prompt: ASSISTANT_PROMPT.trim().into(),
            skill_prompt: SKILL_PROMPT.trim().into(),
            control_prompt: CONTROL_PROMPT.trim().into(),
            compress_prompt: COMPRESSION_PROMPT.trim().into(),

            assist_temperature: 0.6,
            skill_temperature: 0.2,
            compress_temperature: 0.4,

            completions: Options::openai()
                .env_var("ROUTERAI_API_KEY")
                .base_url("https://routerai.ru/api")
                .model("qwen/qwen3-coder-next")
                .max_tokens(16384),
            embeddings: Options::openai()
                .env_var("ROUTERAI_API_KEY")
                .base_url("https://routerai.ru/api")
                .model("perplexity/pplx-embed-v1-0.6b"),
        }
    }
}

impl LLMOptions {
    pub fn system_prompt(&self) -> &str {
        self.system_prompt.trim()
    }
    pub fn assist_prompt(&self) -> &str {
        self.assist_prompt.trim()
    }
    pub fn skill_prompt(&self) -> &str {
        self.skill_prompt.trim()
    }
    pub fn control_prompt(&self) -> &str {
        self.control_prompt.trim()
    }
    pub fn compress_prompt(&self) -> &str {
        self.compress_prompt.trim()
    }

    pub fn assist_options(&self) -> Options {
        self.completions
            .clone()
            .temperature(self.assist_temperature)
    }

    pub fn skill_options(&self) -> Options {
        self.completions.clone().temperature(self.skill_temperature)
    }

    pub fn compress_options(&self) -> Options {
        self.completions
            .clone()
            .temperature(self.compress_temperature)
    }

    pub fn completions_options(&self) -> Options {
        self.completions.clone()
    }

    pub fn embeddings_options(&self) -> Options {
        self.embeddings.clone()
    }
}

/// Context and RAG memory options.
///
/// Thresholds are clamped by the accessors below, so a wrong value in `config.toml`
/// cannot silently destroy facts or flood the prompt.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextOptions {
    /// Minimum cosine similarity of a fact to be returned by search (0.0..=1.0).
    /// The baseline depends on the embeddings model: calibrate it with `/facts probe <query>`.
    pub fact_similarity: f32,
    /// Vector hits further than this from the best one are dropped (cuts the noisy tail, 0 = off).
    pub fact_margin: f32,
    /// Also search facts by exact words (names, versions, models) and fuse both rankings.
    pub hybrid_search: bool,
    /// Similarity at which a new fact is considered a near-duplicate of an existing one.
    /// Facts are never deleted automatically: the model is asked to confirm the replacement.
    pub dedup_similarity: f32,
    /// Maximum facts to retrieve per query.
    pub search_limit: usize,
    /// Character budget of the auto-loaded facts block in the prompt.
    pub facts_max_chars: usize,
    /// How many latest turns are shown in the dialog map (older ones are available via `read_history`).
    pub dialog_map_turns: usize,
    /// Maximum characters per side (user / assistant) of one dialog map line.
    pub dialog_map_chars: usize,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            // lenient on purpose: the injected block is bounded by `search_limit` and `facts_max_chars`
            fact_similarity: 0.3,
            fact_margin: 0.12,
            hybrid_search: true,
            dedup_similarity: 0.92,
            search_limit: 8,
            facts_max_chars: 800,
            dialog_map_turns: 30,
            dialog_map_chars: 160,
        }
    }
}

impl ContextOptions {
    /// Search threshold (always within 0.0..=1.0).
    pub fn similarity_threshold(&self) -> f32 {
        if self.fact_similarity.is_finite() {
            self.fact_similarity.clamp(0.0, 1.0)
        } else {
            0.3
        }
    }

    /// Margin of the best-hit window (`None` = disabled).
    pub fn margin(&self) -> Option<f32> {
        if self.fact_margin.is_finite() && self.fact_margin > 0.0 {
            Some(self.fact_margin.clamp(0.0, 1.0))
        } else {
            None
        }
    }

    /// Near-duplicate threshold (never below 0.85: loose values would merge different facts).
    pub fn dedup_threshold(&self) -> f32 {
        if self.dedup_similarity.is_finite() {
            self.dedup_similarity.clamp(0.85, 1.0)
        } else {
            0.92
        }
    }

    /// Facts per query (at least 1, at most 50).
    pub fn limit(&self) -> usize {
        self.search_limit.clamp(1, 50)
    }
}

/// Kernel config.
#[atoman::config]
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// TUI/GUI theme options.
    pub theme: ThemeOptions,
    /// Server infrastructure settings.
    pub server: ServerOptions,
    /// Execution control options for assistant runs.
    pub execution: ExecutionOptions,
    /// JavaScript runtime options.
    pub runtime: RuntimeOptions,
    /// LLM prompts & provider options.
    pub llm: LLMOptions,
    /// RAG memory and context settings.
    pub context: ContextOptions,
}

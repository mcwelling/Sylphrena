use std::path::Path;

// ---------------------------------------------------------------------------
// Model catalog — single source of truth
// ---------------------------------------------------------------------------

/// A Claude model entry.  **To add a new model, add one entry here — no
/// other file needs editing.**
///
/// `bedrock_id` is the full AWS Bedrock model identifier including date suffix.
/// Leave it empty (`""`) if the model is not yet available on Bedrock;
/// `bedrock::bedrock_model_id` will then pass the friendly `id` through as-is.
pub struct ModelInfo {
    /// Friendly name accepted by the Anthropic API (e.g. `"claude-opus-4-6"`).
    pub id: &'static str,
    /// One-line description shown in the `/model` picker.
    pub description: &'static str,
    /// Full AWS Bedrock model ID (e.g. `"anthropic.claude-opus-4-6-20250514-v1:0"`).
    pub bedrock_id: &'static str,
}

pub const CLAUDE_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-5-5",
        description: "Most capable — complex reasoning, coding, deep analysis",
        bedrock_id: "anthropic.claude-opus-5-5",
    },
    ModelInfo {
        id: "claude-opus-4-6",
        description: "Previous Opus — complex reasoning, coding, deep analysis",
        bedrock_id: "anthropic.claude-opus-4-6-20250514-v1:0",
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        description: "Balanced — strong performance at faster speed",
        bedrock_id: "anthropic.claude-sonnet-4-6-20250514-v1:0",
    },
    ModelInfo {
        id: "claude-haiku-4-5",
        description: "Fastest — efficient for simple tasks and high volume",
        bedrock_id: "anthropic.claude-haiku-4-5-20251001-v1:0",
    },
];

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_BACKEND: &str = "anthropic";
pub const DEFAULT_RATE_LIMIT: u64 = 60_000;
/// Max *output* tokens sent in each API request.  Modern Claude models support
/// 32K–64K output tokens; 8096 was an overly conservative legacy value that
/// would silently truncate long code-generation responses.
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Whether `model` accepts manual extended thinking
/// (`thinking: {type: "enabled", budget_tokens}`).
///
/// Claude Opus 4.7 and later, and every 5.x model, reject it with a 400:
/// their thinking is adaptive, and depth is set with the `effort` parameter.
/// Unknown/older models are assumed to support it.
pub fn supports_manual_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("opus-4-7") || m.contains("opus-4-8") {
        return false;
    }
    // claude-{opus,sonnet,haiku,fable,mythos}-5[...]
    !["opus-5", "sonnet-5", "haiku-5", "fable-5", "mythos-5"]
        .iter()
        .any(|f| m.contains(f))
}

pub const BAKED_IMAGE: &str = "syl-agent-ready:latest";
/// Container name used by Syl versions before per-project names existed.
pub const LEGACY_CONTAINER_NAME: &str = "syl-agent";

/// Podman container name for this project: `syl-agent-<hash>` where the hash is
/// derived from the project directory. Each project gets its own container, so
/// running Syl in two projects at once no longer collides.
pub fn container_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let dir = std::env::current_dir().unwrap_or_default();
        let dir = dir.canonicalize().unwrap_or(dir);
        container_name_for(&dir)
    })
}

/// Pure helper behind `container_name()` (exposed for tests).
pub fn container_name_for(project_dir: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(project_dir.to_string_lossy().as_bytes());
    format!("syl-agent-{}", &hex::encode(digest)[..10])
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// Merged runtime configuration (CLI > env > file > default).
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub model: String,
    pub backend: String,
    pub rate_limit: u64,
    pub theme: String,
    pub image: String,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub thinking_budget: Option<u64>,
    pub api_key: String,
    pub api_url: String,
    /// AWS region (Bedrock backend only)
    pub aws_region: String,
    /// Bedrock cross-region inference prefix (e.g. `"us."`, `"eu."`, `"ap."`).
    /// Depends on the AWS region; `"us."` is the default for `us-east-1`.
    pub bedrock_prefix: String,
    pub aws_secret_key: String,
    pub aws_session_token: Option<String>,
    pub resume: bool,
    pub fresh: bool,
    pub no_banner: bool,
    pub light: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            model: DEFAULT_MODEL.to_string(),
            backend: DEFAULT_BACKEND.to_string(),
            rate_limit: DEFAULT_RATE_LIMIT,
            theme: "dark".to_string(),
            image: "auto".to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking_budget: None,
            api_key: String::new(),
            api_url: ANTHROPIC_API_URL.to_string(),
            aws_region: "us-east-1".to_string(),
            bedrock_prefix: "us.".to_string(),
            aws_secret_key: String::new(),
            aws_session_token: None,
            resume: false,
            fresh: false,
            no_banner: false,
            light: false,
        }
    }
}

// ---------------------------------------------------------------------------
// File config
// ---------------------------------------------------------------------------

/// File-level config (from `.syl/config.toml`).
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    model: Option<String>,
    backend: Option<String>,
    rate_limit: Option<u64>,
    theme: Option<String>,
    image: Option<String>,
}

/// Load the TOML config file. Returns `Default` if missing or unparseable.
fn load_toml_config(path: &Path) -> FileConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return FileConfig::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// CLI parser
// ---------------------------------------------------------------------------

/// CLI argument parser. Mirrors parse-cli-args in syl.bb.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CliArgs {
    pub model: Option<String>,
    pub timeout: Option<u64>,
    pub thinking: Option<u64>,
    pub rate_limit: Option<u64>,
    pub backend: Option<String>,
    pub image: Option<String>,
    pub resume: bool,
    pub fresh: bool,
    pub no_banner: bool,
    pub light: bool,
    pub help: bool,
}

pub fn parse_cli_args(args: &[&str]) -> CliArgs {
    let mut out = CliArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--model" => {
                if i + 1 < args.len() {
                    out.model = Some(args[i + 1].to_string());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--timeout" => {
                if i + 1 < args.len() {
                    out.timeout = args[i + 1].parse().ok();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--thinking" => {
                if i + 1 < args.len() {
                    out.thinking = args[i + 1].parse().ok();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--rate-limit" => {
                if i + 1 < args.len() {
                    out.rate_limit = args[i + 1].parse().ok();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--backend" => {
                if i + 1 < args.len() {
                    out.backend = Some(args[i + 1].to_string());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--image" => {
                if i + 1 < args.len() {
                    out.image = Some(args[i + 1].to_string());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--resume" => {
                out.resume = true;
                i += 1;
            }
            "--fresh" => {
                out.fresh = true;
                i += 1;
            }
            "--no-banner" => {
                out.no_banner = true;
                i += 1;
            }
            "--light" => {
                out.light = true;
                i += 1;
            }
            "--help" => {
                out.help = true;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// build_config
// ---------------------------------------------------------------------------

/// Build an `AppConfig` from CLI args, environment, and the config file.
/// Priority: CLI flag > env var > config file > default.
pub fn build_config(cli: &CliArgs, syl_dir: &Path) -> AppConfig {
    let toml_path = syl_dir.join("config.toml");
    let file = load_toml_config(&toml_path);
    let mut cfg = AppConfig::default();

    // model
    if let Some(v) = cli
        .model
        .clone()
        .or_else(|| std::env::var("SYL_MODEL").ok())
        .or(file.model)
    {
        cfg.model = v;
    }

    // backend
    if let Some(v) = cli
        .backend
        .clone()
        .or_else(|| std::env::var("SYL_BACKEND").ok())
        .or(file.backend)
    {
        cfg.backend = v;
    }

    // rate_limit
    if let Some(v) = cli
        .rate_limit
        .or_else(|| {
            std::env::var("SYL_RATE_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .or(file.rate_limit)
    {
        cfg.rate_limit = v;
    }

    // theme / image
    if let Some(v) = std::env::var("SYL_THEME").ok().or(file.theme) {
        cfg.theme = v;
    }
    if let Some(v) = cli
        .image
        .clone()
        .or_else(|| std::env::var("SYL_IMAGE").ok())
        .or(file.image)
    {
        cfg.image = v;
    }

    // timeout from CLI
    if let Some(v) = cli.timeout {
        cfg.timeout_ms = v * 1000;
    }

    // thinking budget from CLI
    cfg.thinking_budget = cli.thinking;

    // boolean flags
    cfg.resume = cli.resume;
    cfg.fresh = cli.fresh;
    cfg.no_banner = cli.no_banner;
    cfg.light = cli.light || cfg.theme == "light";

    // API key from environment
    cfg.api_key = match cfg.backend.as_str() {
        "bedrock" => std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        _ => std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
    };

    // AWS region — also informs bedrock_prefix
    if let Ok(r) = std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION")) {
        // Derive the cross-region inference prefix from the region name.
        // AWS uses "us.", "eu.", or "ap." depending on the region partition.
        cfg.bedrock_prefix = if r.starts_with("eu-") {
            "eu.".to_string()
        } else if r.starts_with("ap-") {
            "ap.".to_string()
        } else {
            "us.".to_string() // us-east-*, us-west-*, and default
        };
        cfg.aws_region = r;
    }

    cfg.aws_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
    cfg.aws_session_token = std::env::var("AWS_SESSION_TOKEN").ok();

    cfg
}

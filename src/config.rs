use std::path::Path;

/// Default values.
pub const DEFAULT_MODEL: &str = "claude-opus-4-6";
pub const DEFAULT_BACKEND: &str = "anthropic";
pub const DEFAULT_RATE_LIMIT: u64 = 60_000;
pub const DEFAULT_MAX_TOKENS: u32 = 8096;
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const BAKED_IMAGE: &str = "syl-agent-ready:latest";
pub const CONTAINER_NAME: &str = "syl-agent";

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
    /// Bedrock uses cross-region inference prefix by default
    pub bedrock_prefix: String,
    pub aws_secret_key:    String,
    pub aws_session_token: Option<String>,
    pub resume: bool,
    pub fresh: bool,
    pub no_banner: bool,
    pub light: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            model:           DEFAULT_MODEL.to_string(),
            backend:         DEFAULT_BACKEND.to_string(),
            rate_limit:      DEFAULT_RATE_LIMIT,
            theme:           "dark".to_string(),
            image:           "auto".to_string(),
            timeout_ms:      DEFAULT_TIMEOUT_MS,
            max_tokens:      DEFAULT_MAX_TOKENS,
            thinking_budget: None,
            api_key:         String::new(),
            api_url:         ANTHROPIC_API_URL.to_string(),
            aws_region:        "us-east-1".to_string(),
            bedrock_prefix:    "us.".to_string(),
            aws_secret_key:    String::new(),
            aws_session_token: None,
            resume:            false,
            fresh:           false,
            no_banner:       false,
            light:           false,
        }
    }
}

/// File-level config (from .syl/config.toml).
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    model:      Option<String>,
    backend:    Option<String>,
    rate_limit: Option<u64>,
    theme:      Option<String>,
    image:      Option<String>,
}

/// CLI argument parser. Mirrors parse-cli-args in syl.bb.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CliArgs {
    pub model:      Option<String>,
    pub timeout:    Option<u64>,
    pub thinking:   Option<u64>,
    pub rate_limit: Option<u64>,
    pub backend:    Option<String>,
    pub image:      Option<String>,
    pub resume:     bool,
    pub fresh:      bool,
    pub no_banner:  bool,
    pub light:      bool,
    pub help:       bool,
}

pub fn parse_cli_args(args: &[&str]) -> CliArgs {
    let mut out = CliArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--model"      => { if i + 1 < args.len() { out.model      = Some(args[i+1].to_string()); i += 2; } else { i += 1; } }
            "--timeout"    => { if i + 1 < args.len() { out.timeout    = args[i+1].parse().ok();       i += 2; } else { i += 1; } }
            "--thinking"   => { if i + 1 < args.len() { out.thinking   = args[i+1].parse().ok();       i += 2; } else { i += 1; } }
            "--rate-limit" => { if i + 1 < args.len() { out.rate_limit = args[i+1].parse().ok();       i += 2; } else { i += 1; } }
            "--backend"    => { if i + 1 < args.len() { out.backend    = Some(args[i+1].to_string()); i += 2; } else { i += 1; } }
            "--image"      => { if i + 1 < args.len() { out.image      = Some(args[i+1].to_string()); i += 2; } else { i += 1; } }
            "--resume"     => { out.resume    = true; i += 1; }
            "--fresh"      => { out.fresh     = true; i += 1; }
            "--no-banner"  => { out.no_banner = true; i += 1; }
            "--light"      => { out.light     = true; i += 1; }
            "--help"       => { out.help      = true; i += 1; }
            _              => { i += 1; }
        }
    }
    out
}

/// Load the TOML config file. Returns `Default` if missing.
fn load_toml_config(path: &Path) -> FileConfig {
    let Ok(text) = std::fs::read_to_string(path) else { return FileConfig::default(); };
    toml::from_str(&text).unwrap_or_default()
}

/// Targeted EDN value extractor. Handles `:key "string"` and `:key number`.
/// Does NOT implement a general EDN parser — only extracts known scalar fields.
pub fn parse_edn_value<'a>(edn: &'a str, key: &str) -> Option<&'a str> {
    // Match :key "value" (string) or :key value (bare token up to whitespace/})
    let pattern = format!(r#":{}\s+"([^"]+)""#, key);
    let re = regex::Regex::new(&pattern).ok()?;
    re.captures(edn).and_then(|c| c.get(1)).map(|m| m.as_str())
}

/// One-shot EDN → TOML migration. Called on first run when .syl/config.edn exists
/// but .syl/config.toml does not.
///
/// Reads the known fields from EDN and writes them to TOML. Prints a one-time
/// notice. Never grows into a general EDN reader; delete after 6 months.
pub fn migrate_edn_to_toml(edn_path: &Path, toml_path: &Path) -> anyhow::Result<()> {
    let edn = std::fs::read_to_string(edn_path)?;
    let mut out = String::new();

    for key in &["model", "backend", "theme", "image"] {
        if let Some(val) = parse_edn_value(&edn, key) {
            out.push_str(&format!("{} = \"{}\"\n", key, val));
        }
    }
    // rate-limit is a number (not quoted in EDN)
    let rate_re = regex::Regex::new(r#":rate-limit\s+(\d+)"#).unwrap();
    if let Some(cap) = rate_re.captures(&edn) {
        out.push_str(&format!("rate_limit = {}\n", &cap[1]));
    }

    std::fs::write(toml_path, out)?;
    println!("[Syl] Migrated .syl/config.edn → .syl/config.toml");
    Ok(())
}

/// Build an `AppConfig` from CLI args, environment, and the config file.
/// Priority: CLI flag > env var > config file > default.
pub fn build_config(cli: &CliArgs, syl_dir: &Path) -> AppConfig {
    let toml_path = syl_dir.join("config.toml");
    let edn_path  = syl_dir.join("config.edn");

    // One-shot EDN migration (only when toml absent and edn present)
    if !toml_path.exists() && edn_path.exists() {
        let _ = migrate_edn_to_toml(&edn_path, &toml_path);
    }

    let file = load_toml_config(&toml_path);

    let mut cfg = AppConfig::default();

    // model
    if let Some(v) = cli.model.clone()
        .or_else(|| std::env::var("SYL_MODEL").ok())
        .or(file.model) { cfg.model = v; }

    // backend
    if let Some(v) = cli.backend.clone()
        .or_else(|| std::env::var("SYL_BACKEND").ok())
        .or(file.backend) { cfg.backend = v; }

    // rate_limit
    if let Some(v) = cli.rate_limit
        .or_else(|| std::env::var("SYL_RATE_LIMIT").ok().and_then(|s| s.parse().ok()))
        .or(file.rate_limit) { cfg.rate_limit = v; }

    // theme / image from file + env (no CLI flag)
    if let Some(v) = std::env::var("SYL_THEME").ok().or(file.theme) { cfg.theme = v; }
    if let Some(v) = cli.image.clone()
        .or_else(|| std::env::var("SYL_IMAGE").ok())
        .or(file.image) { cfg.image = v; }

    // timeout from CLI
    if let Some(v) = cli.timeout { cfg.timeout_ms = v * 1000; }

    // thinking budget from CLI
    cfg.thinking_budget = cli.thinking;

    // boolean flags
    cfg.resume    = cli.resume;
    cfg.fresh     = cli.fresh;
    cfg.no_banner = cli.no_banner;
    cfg.light     = cli.light || cfg.theme == "light";

    // API key from environment
    cfg.api_key = match cfg.backend.as_str() {
        "bedrock" => std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        _         => std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
    };

    // AWS region
    if let Ok(r) = std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION")) {
        cfg.aws_region = r;
    }

    cfg.aws_secret_key    = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
    cfg.aws_session_token = std::env::var("AWS_SESSION_TOKEN").ok();

    cfg
}

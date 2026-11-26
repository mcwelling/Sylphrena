use std::path::Path;

// ---------------------------------------------------------------------------
// Model catalog — single source of truth
// ---------------------------------------------------------------------------

/// A Claude model entry.  **To add a new model, add one entry here — no
/// other file needs editing.**
pub struct ModelInfo {
    /// Friendly name accepted by the Anthropic API (e.g. `"claude-opus-4-6"`).
    pub id: &'static str,
    /// One-line description shown in the `/model` picker.
    pub description: &'static str,
}

pub const CLAUDE_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-5-5",
        description: "Most capable — complex reasoning, coding, deep analysis",
    },
    ModelInfo {
        id: "claude-opus-4-6",
        description: "Previous Opus — complex reasoning, coding, deep analysis",
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        description: "Balanced — strong performance at faster speed",
    },
    ModelInfo {
        id: "claude-haiku-4-5",
        description: "Fastest — efficient for simple tasks and high volume",
    },
];

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// No cap of Syl's own by default: the API's limits (from its response
/// headers) are followed instead. `rate_limit` sets a cap below them.
pub const DEFAULT_RATE_LIMIT: u64 = 0;
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

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// Merged runtime configuration (CLI > env > file > default).
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub model: String,
    pub rate_limit: u64,
    pub theme: String,
    pub image: String,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub thinking_budget: Option<u64>,
    pub api_key: String,
    pub api_url: String,
    pub resume: bool,
    pub fresh: bool,
    pub no_banner: bool,
    pub light: bool,
    /// Context window override in tokens (`context_window` in config.toml).
    /// When unset, the Models API's `max_input_tokens` is used, then a
    /// per-model default.
    pub context_window: Option<u64>,
    /// Compact once the context is within this fraction of the window
    /// (`compact_buffer`, default 0.15).
    pub compact_buffer: f64,
    /// Estimated tokens of recent history kept verbatim when compacting
    /// (`compact_keep_tokens`, default 40000).
    pub compact_keep_tokens: u64,
    /// Offer Syl's tools in requests. Sub-calls that must answer in text,
    /// like the compaction summarizer, turn this off.
    pub tools: bool,
    /// `[[permission]]` rules from config.toml, in file order.
    pub permissions: Vec<crate::permissions::Rule>,
    /// `[reviewer]`: a cheap model that may approve `ask` calls.
    pub reviewer: Option<ReviewerConfig>,
    /// `[container]` options.
    pub container: ContainerConfig,
    /// Why config.toml couldn't be read, if it couldn't. The defaults are
    /// used, and the client should say so: a typo must not quietly turn
    /// permission rules off.
    pub config_error: Option<String>,
    /// Things in config.toml that were ignored, for the client to show.
    pub config_warnings: Vec<String>,
    /// `[[hook]]` commands (they run in the container).
    pub hooks: Vec<crate::hooks::Hook>,
    /// `[[mcp]]` servers: `~/.syl/config.toml`'s, then the project's.
    pub mcp: Vec<crate::mcp::McpServerConfig>,
    /// The project's hooks and MCP servers when they aren't trusted yet:
    /// not in `hooks` or `mcp` until [`AppConfig::trust_project`].
    pub untrusted: Option<crate::trust::ProjectExtensions>,
    /// Tool definitions sent with the built-in ones: MCP tools and the
    /// `skill` tool, fixed for the conversation.
    pub extra_tools: std::sync::Arc<Vec<serde_json::Value>>,
}

/// `[reviewer]` in config.toml.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerConfig {
    /// Default `claude-haiku-4-5`.
    #[serde(default = "default_reviewer_model")]
    pub model: String,
    /// How long to wait for a verdict before asking the user. Default 20.
    #[serde(default = "default_reviewer_timeout")]
    pub timeout_secs: u64,
}

fn default_reviewer_model() -> String {
    "claude-haiku-4-5".to_string()
}

fn default_reviewer_timeout() -> u64 {
    20
}

/// `[container]` in config.toml.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerConfig {
    /// `"none"` runs the container without a network.
    pub network: Option<String>,
    /// Memory limit, as podman takes it (`"4g"`), or `"auto"` (the default):
    /// three quarters of podman's memory (its VM's, on macOS).
    pub memory: Option<String>,
    /// CPU limit (`"2"`, `"1.5"`). Default: no limit.
    pub cpus: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            model: DEFAULT_MODEL.to_string(),
            rate_limit: DEFAULT_RATE_LIMIT,
            theme: "dark".to_string(),
            image: "auto".to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking_budget: None,
            api_key: String::new(),
            api_url: ANTHROPIC_API_URL.to_string(),
            resume: false,
            fresh: false,
            no_banner: false,
            light: false,
            context_window: None,
            compact_buffer: crate::api::compaction::DEFAULT_COMPACT_BUFFER,
            compact_keep_tokens: crate::api::compaction::DEFAULT_KEEP_TOKENS,
            tools: true,
            permissions: Vec::new(),
            reviewer: None,
            container: ContainerConfig::default(),
            config_error: None,
            config_warnings: Vec::new(),
            hooks: Vec::new(),
            mcp: Vec::new(),
            untrusted: None,
            extra_tools: Default::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// File config
// ---------------------------------------------------------------------------

/// File-level config (from `.syl/config.toml`). Unknown top-level keys are
/// tolerated (a newer Syl's settings, say) but reported, so `[[permissions]]`
/// for `[[permission]]` doesn't quietly mean no rules. Inside the tables
/// that control safety, unknown keys are errors.
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    model: Option<String>,
    rate_limit: Option<u64>,
    theme: Option<String>,
    image: Option<String>,
    context_window: Option<u64>,
    compact_buffer: Option<f64>,
    compact_keep_tokens: Option<u64>,
    #[serde(default)]
    permission: Vec<crate::permissions::Rule>,
    reviewer: Option<ReviewerConfig>,
    #[serde(default)]
    container: ContainerConfig,
    #[serde(default)]
    hook: Vec<crate::hooks::Hook>,
    #[serde(default)]
    mcp: Vec<crate::mcp::McpServerConfig>,
}

const KNOWN_KEYS: &[&str] = &[
    "model", "rate_limit", "theme", "image", "context_window", "compact_buffer", "compact_keep_tokens",
    "permission", "reviewer", "container", "hook", "mcp",
];

/// `[[mcp]]` servers from `~/.syl/config.toml`, the one place a server may
/// be set to run on the host.
#[derive(Debug, Default, serde::Deserialize)]
struct UserConfig {
    #[serde(default)]
    mcp: Vec<crate::mcp::McpServerConfig>,
}

fn load_user_mcp(errors: &mut Vec<String>) -> Vec<crate::mcp::McpServerConfig> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let path = std::path::PathBuf::from(home).join(".syl/config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let table = match toml::from_str::<toml::Table>(&text) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("{}: {e}", path.display()));
            return Vec::new();
        }
    };
    // Only the mcp tables matter here; the rest of the file is the user's.
    let mut only_mcp = toml::Table::new();
    if let Some(m) = table.get("mcp") {
        only_mcp.insert("mcp".into(), m.clone());
    }
    match toml::Value::Table(only_mcp).try_into::<UserConfig>() {
        Ok(u) => u.mcp,
        Err(e) => {
            errors.push(format!("{} [[mcp]]: {e}", path.display()));
            Vec::new()
        }
    }
}

/// Load the TOML config file. Missing → defaults. Unparseable → defaults
/// and the error, which the caller must surface. Unknown top-level keys
/// come back as warnings.
fn load_toml_config(path: &Path) -> (FileConfig, Option<String>, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (FileConfig::default(), None, Vec::new());
    };
    let warnings = match toml::from_str::<toml::Table>(&text) {
        Ok(table) => table
            .keys()
            .filter(|k| !KNOWN_KEYS.contains(&k.as_str()))
            .map(|k| {
                let hint = if k == "permissions" { " (did you mean [[permission]]?)" } else { "" };
                format!("{}: unknown setting `{k}` is ignored{hint}", path.display())
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    match toml::from_str(&text) {
        Ok(c) => (c, None, warnings),
        Err(e) => (FileConfig::default(), Some(format!("{}: {e}", path.display())), warnings),
    }
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
    pub image: Option<String>,
    /// `--resume [id]` or `--continue`: Some("") means the most recent
    /// session, otherwise an id, id prefix, list position or name.
    pub resume: Option<String>,
    pub fresh: bool,
    pub no_banner: bool,
    pub light: bool,
    pub help: bool,
    /// `-p` / `--print [prompt]`: run one turn without the terminal UI and
    /// print the answer. Some("") means read the prompt from stdin.
    pub print: Option<String>,
    /// `--output json`: with `-p`, stream every agent event as a JSON line.
    pub output_json: bool,
    /// `--plan`: with `-p`, run the turn in plan mode.
    pub plan: bool,
    /// `syl acp`: serve the Agent Client Protocol on stdio.
    pub acp: bool,
    /// Arguments that are not flags, in order (the `-p` prompt, `acp`).
    pub positional: Vec<String>,
    /// Problems with the arguments, to report before doing anything.
    pub errors: Vec<String>,
}

/// Flags the parser knows, so `-p` can tell a prompt that starts with `-`
/// from the next flag.
const FLAGS: &[&str] = &[
    "--model", "--timeout", "--thinking", "--rate-limit", "--image", "--continue", "-c", "--resume", "--fresh",
    "--no-banner", "--light", "--help", "-h", "-p", "--print", "--output", "--plan",
];

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
            "--image" => {
                if i + 1 < args.len() {
                    out.image = Some(args[i + 1].to_string());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--continue" | "-c" => {
                out.resume = Some(String::new());
                i += 1;
            }
            "--resume" => {
                match args.get(i + 1) {
                    Some(q) if !q.starts_with('-') => {
                        out.resume = Some(q.to_string());
                        i += 2;
                    }
                    _ => {
                        out.resume = Some(String::new());
                        i += 1;
                    }
                }
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
            "--help" | "-h" => {
                out.help = true;
                i += 1;
            }
            "-p" | "--print" => {
                // The next argument is the prompt unless it is one of our
                // flags (so "-1 is the answer" is a prompt).
                match args.get(i + 1) {
                    Some(q) if !FLAGS.contains(q) => {
                        out.print = Some(if *q == "-" { String::new() } else { q.to_string() });
                        i += 2;
                    }
                    _ => {
                        out.print = Some(String::new());
                        i += 1;
                    }
                }
            }
            "--output" => {
                match args.get(i + 1).map(|v| v.to_ascii_lowercase()) {
                    Some(v) if v == "json" => out.output_json = true,
                    Some(v) if v == "text" => out.output_json = false,
                    Some(v) if !FLAGS.contains(&v.as_str()) => {
                        out.errors.push(format!("--output takes json or text, not {v:?}"))
                    }
                    _ => {
                        out.errors.push("--output needs a value: json or text".to_string());
                        i += 1;
                        continue;
                    }
                }
                i += 2;
            }
            "--plan" => {
                out.plan = true;
                i += 1;
            }
            other => {
                if other.starts_with('-') && other != "-" {
                    out.errors.push(format!("unknown option {other}"));
                } else {
                    out.positional.push(other.to_string());
                }
                i += 1;
            }
        }
    }
    if out.positional.first().map(String::as_str) == Some("acp") {
        out.acp = true;
        out.positional.remove(0);
    }
    // `syl -p --plan "explain x"`: the prompt came after other flags.
    if matches!(&out.print, Some(p) if p.is_empty()) && !out.positional.is_empty() {
        out.print = Some(std::mem::take(&mut out.positional).join(" "));
    }
    out
}

// ---------------------------------------------------------------------------
// build_config
// ---------------------------------------------------------------------------

/// Build an `AppConfig` from CLI args, environment, and the config file.
/// Priority: CLI flag > env var > config file > default.
impl AppConfig {
    /// Trust the project's held-back hooks and MCP servers: remember it, and
    /// put them in effect.
    pub fn trust_project(&mut self) -> anyhow::Result<()> {
        if let Some(ext) = self.untrusted.take() {
            let saved = crate::trust::trust(&ext);
            self.hooks = ext.hooks.clone();
            self.mcp.extend(ext.mcp.clone());
            saved?;
        }
        Ok(())
    }

    /// The notice for hooks and servers that are held back.
    pub fn untrusted_notice(&self) -> Option<String> {
        let ext = self.untrusted.as_ref()?;
        Some(format!(
            "This project's .syl/config.toml has hooks or MCP servers that aren't trusted yet, so they don't run:\n  {}\nRun `syl` in this project and answer yes to trust them.",
            ext.describe().join("\n  ")
        ))
    }
}

pub fn build_config(cli: &CliArgs, syl_dir: &Path) -> AppConfig {
    let toml_path = syl_dir.join("config.toml");
    let (file, config_error, config_warnings) = load_toml_config(&toml_path);
    let mut cfg = AppConfig { config_error, config_warnings, ..AppConfig::default() };

    // model
    if let Some(v) = cli
        .model
        .clone()
        .or_else(|| std::env::var("SYL_MODEL").ok())
        .or(file.model)
    {
        cfg.model = v;
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
    cfg.resume = cli.resume.is_some();
    cfg.fresh = cli.fresh;
    cfg.no_banner = cli.no_banner;
    cfg.light = cli.light || cfg.theme == "light";

    // context and compaction (config file only)
    cfg.context_window = file.context_window;
    if let Some(v) = file.compact_buffer {
        cfg.compact_buffer = v;
    }
    if let Some(v) = file.compact_keep_tokens {
        cfg.compact_keep_tokens = v;
    }
    cfg.permissions = file.permission;
    let mut user_errors = Vec::new();
    let mut mcp: Vec<crate::mcp::McpServerConfig> = Vec::new();
    for server in load_user_mcp(&mut user_errors) {
        if mcp.iter().any(|s| s.name == server.name) {
            user_errors.push(format!("MCP server {} is defined twice in ~/.syl/config.toml; the first is used", server.name));
        } else {
            mcp.push(server);
        }
    }
    cfg.config_warnings.extend(user_errors);
    // Run in the home directory, the project's config *is* ~/.syl/config.toml.
    let is_home = std::env::var_os("HOME").is_some_and(|h| {
        let user = std::path::PathBuf::from(h).join(".syl/config.toml");
        std::fs::canonicalize(&user).ok().is_some_and(|u| std::fs::canonicalize(&toml_path).ok() == Some(u))
    });
    let project_mcp = if is_home { Vec::new() } else { file.mcp };
    let mut kept_mcp: Vec<crate::mcp::McpServerConfig> = Vec::new();
    let mut seen_in_project: Vec<String> = Vec::new();
    for server in project_mcp {
        if seen_in_project.contains(&server.name) {
            cfg.config_warnings.push(format!("MCP server {} is defined twice in .syl/config.toml; the first is used", server.name));
            continue;
        }
        seen_in_project.push(server.name.clone());
        if server.url.is_some() {
            // Syl, on the host, would make the requests: a project could
            // point it at your localhost or a cloud metadata address.
            cfg.config_warnings.push(format!(
                "MCP server {} has a url; only ~/.syl/config.toml (or your editor) may add a remote server, \
                 since Syl makes those requests from your machine. It is left out.",
                server.name
            ));
        } else if server.host {
            cfg.config_warnings.push(format!(
                "MCP server {} asks to run on the host; only ~/.syl/config.toml may do that (a project's \
                 config comes with the project). It is left out.",
                server.name
            ));
        } else if mcp.iter().any(|s| s.name == server.name) {
            cfg.config_warnings.push(format!("MCP server {} is defined twice; the one in ~/.syl/config.toml is used", server.name));
        } else {
            kept_mcp.push(server);
        }
    }
    cfg.mcp = mcp;
    // In the home directory the file is the user's own; elsewhere the
    // project's hooks and servers wait to be trusted.
    let project_dir = syl_dir.parent().unwrap_or(syl_dir);
    let ext = crate::trust::ProjectExtensions::new(project_dir, file.hook, kept_mcp);
    if is_home || ext.is_empty() || crate::trust::is_trusted(&ext) {
        cfg.hooks = ext.hooks;
        cfg.mcp.extend(ext.mcp);
    } else {
        cfg.untrusted = Some(ext);
    }
    if cfg.config_error.is_some() {
        // The rules that were meant to apply are unknown: ask before every
        // tool call until the file is fixed, rather than allow everything.
        cfg.permissions = vec![crate::permissions::Rule {
            tool: "*".into(),
            pattern: "*".into(),
            action: crate::permissions::Action::Ask,
        }];
    }
    cfg.reviewer = file.reviewer;
    cfg.container = file.container;

    // API key from environment
    cfg.api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();

    cfg
}

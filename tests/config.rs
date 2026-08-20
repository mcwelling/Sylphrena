/// Tests for config.rs (Phase 4 mandatory gate).
use syl_rs::config::{build_config, parse_cli_args, CliArgs, DEFAULT_MODEL, DEFAULT_RATE_LIMIT};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// parse_cli_args
// ---------------------------------------------------------------------------

#[test]
fn parse_no_args_gives_defaults() {
    let cli = parse_cli_args(&[]);
    assert_eq!(cli, CliArgs::default());
    assert!(!cli.fresh);
    assert!(!cli.help);
}

#[test]
fn parse_model_flag() {
    let cli = parse_cli_args(&["--model", "claude-haiku-4-5"]);
    assert_eq!(cli.model.as_deref(), Some("claude-haiku-4-5"));
}

#[test]
fn parse_rate_limit_flag() {
    let cli = parse_cli_args(&["--rate-limit", "30000"]);
    assert_eq!(cli.rate_limit, Some(30_000));
}

#[test]
fn parse_fresh_flag() {
    let cli = parse_cli_args(&["--fresh"]);
    assert!(cli.fresh);
}

#[test]
fn parse_backend_flag() {
    let cli = parse_cli_args(&["--backend", "bedrock"]);
    assert_eq!(cli.backend.as_deref(), Some("bedrock"));
}

#[test]
fn parse_unknown_flag_ignored() {
    let cli = parse_cli_args(&["--unknown-thing", "value"]);
    // Should not panic and other fields stay default
    assert_eq!(cli.model, None);
}

#[test]
fn parse_multiple_flags() {
    let cli = parse_cli_args(&["--model", "claude-sonnet-4-6", "--fresh", "--light"]);
    assert_eq!(cli.model.as_deref(), Some("claude-sonnet-4-6"));
    assert!(cli.fresh);
    assert!(cli.light);
}

// ---------------------------------------------------------------------------
// build_config — priority: CLI > env > file > default
// ---------------------------------------------------------------------------

fn temp_syl_dir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn default_config_loads_without_error() {
    let tmp = temp_syl_dir();
    let cli = CliArgs::default();
    let cfg = build_config(&cli, tmp.path());
    assert_eq!(cfg.model, DEFAULT_MODEL);
    assert_eq!(cfg.rate_limit, DEFAULT_RATE_LIMIT);
}

#[test]
fn cli_flag_overrides_default() {
    let tmp = temp_syl_dir();
    let cli = parse_cli_args(&["--model", "claude-haiku-4-5", "--rate-limit", "20000"]);
    let cfg = build_config(&cli, tmp.path());
    assert_eq!(cfg.model, "claude-haiku-4-5");
    assert_eq!(cfg.rate_limit, 20_000);
}

#[test]
fn toml_file_overrides_default() {
    let tmp = temp_syl_dir();
    let toml_path = tmp.path().join("config.toml");
    std::fs::write(
        &toml_path,
        "model = \"claude-sonnet-4-6\"\nrate_limit = 40000\n",
    )
    .unwrap();
    let cli = CliArgs::default();
    let cfg = build_config(&cli, tmp.path());
    assert_eq!(cfg.model, "claude-sonnet-4-6");
    assert_eq!(cfg.rate_limit, 40_000);
}

#[test]
fn cli_overrides_toml_file() {
    let tmp = temp_syl_dir();
    std::fs::write(
        tmp.path().join("config.toml"),
        "model = \"claude-sonnet-4-6\"\n",
    )
    .unwrap();
    let cli = parse_cli_args(&["--model", "claude-haiku-4-5"]);
    let cfg = build_config(&cli, tmp.path());
    // CLI wins
    assert_eq!(cfg.model, "claude-haiku-4-5");
}

#[test]
fn unknown_key_in_toml_ignored() {
    let tmp = temp_syl_dir();
    // TOML with an unknown key mixed with a known one
    std::fs::write(
        tmp.path().join("config.toml"),
        "model = \"claude-haiku-4-5\"\nunknown_field = \"ignored\"\n",
    )
    .unwrap();
    let cli = CliArgs::default();
    let cfg = build_config(&cli, tmp.path());
    // Known field loaded, unknown silently ignored
    assert_eq!(cfg.model, "claude-haiku-4-5");
}

#[test]
fn missing_config_file_gives_defaults() {
    let tmp = temp_syl_dir();
    // No config.toml written
    let cli = CliArgs::default();
    let cfg = build_config(&cli, tmp.path());
    assert_eq!(cfg.model, DEFAULT_MODEL);
}

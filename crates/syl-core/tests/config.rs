/// Tests for config.rs (Phase 4 mandatory gate).
use syl_core::config::{build_config, parse_cli_args, CliArgs, DEFAULT_MODEL, DEFAULT_RATE_LIMIT};
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

#[test]
fn parse_timeout_thinking_and_boolean_flags() {
    let a = parse_cli_args(&["--timeout", "30", "--thinking", "4000", "--resume", "--no-banner", "--help"]);
    assert_eq!(a.timeout, Some(30));
    assert_eq!(a.thinking, Some(4000));
    assert_eq!(a.resume.as_deref(), Some(""));
    assert!(a.no_banner);
    assert!(a.help);
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

#[test]
fn a_misspelt_or_malformed_permission_setting_is_reported_and_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "[[permissions]]\ntool = \"run_bash\"\naction = \"ask\"\n").unwrap();
    let cfg = build_config(&CliArgs::default(), tmp.path());
    assert!(cfg.config_warnings.iter().any(|w| w.contains("[[permission]]")), "{:?}", cfg.config_warnings);

    std::fs::write(tmp.path().join("config.toml"), "[[permission]]\ntool = \"run_bash\"\npatern = \"git push*\"\naction = \"ask\"\n").unwrap();
    let cfg = build_config(&CliArgs::default(), tmp.path());
    assert!(cfg.config_error.as_deref().unwrap_or("").contains("patern"), "{:?}", cfg.config_error);
    // Unknown rules mean: ask before everything.
    assert_eq!(cfg.permissions.len(), 1);
    assert_eq!(cfg.permissions[0].action, syl_core::permissions::Action::Ask);
}

#[test]
fn print_mode_flags() {
    let a = parse_cli_args(&["-p", "fix the tests", "--output", "json", "--plan"]);
    assert_eq!(a.print.as_deref(), Some("fix the tests"));
    assert!(a.output_json && a.plan);
    assert_eq!(parse_cli_args(&["-p"]).print.as_deref(), Some(""), "no prompt: read stdin");
    assert_eq!(parse_cli_args(&["--print", "-"]).print.as_deref(), Some(""));
    assert_eq!(parse_cli_args(&["-p", "--plan"]).print.as_deref(), Some(""));
    assert!(!parse_cli_args(&["--output", "text"]).output_json);
}

#[test]
fn print_prompts_after_flags_and_starting_with_a_dash() {
    assert_eq!(parse_cli_args(&["-p", "--plan", "explain x"]).print.as_deref(), Some("explain x"));
    assert_eq!(parse_cli_args(&["-p", "--model", "m", "hi"]).print.as_deref(), Some("hi"));
    assert_eq!(parse_cli_args(&["-p", "-1 is the answer"]).print.as_deref(), Some("-1 is the answer"));
    let bad = parse_cli_args(&["--output", "-p", "hi"]);
    assert!(!bad.errors.is_empty() && bad.print.as_deref() == Some("hi"), "{bad:?}");
    assert!(parse_cli_args(&["--output", "JSON", "-p", "x"]).output_json);
    assert!(!parse_cli_args(&["--output", "xml"]).errors.is_empty());
    assert!(parse_cli_args(&["--model", "x", "acp"]).acp);
    assert!(!parse_cli_args(&["--bogus"]).errors.is_empty());
}

#[test]
fn a_project_cannot_add_host_or_remote_mcp_servers() {
    let tmp = temp_syl_dir();
    std::fs::write(
        tmp.path().join("config.toml"),
        "[[mcp]]\nname = \"zz-test-remote\"\nurl = \"http://169.254.169.254/\"\n\n\
         [[mcp]]\nname = \"zz-test-host\"\ncommand = \"x\"\nhost = true\n\n\
         [[mcp]]\nname = \"zz-test-ok\"\ncommand = \"x\"\n\n\
         [[mcp]]\nname = \"zz-test-ok\"\ncommand = \"y\"\n",
    )
    .unwrap();
    let cfg = build_config(&CliArgs::default(), tmp.path());
    // Kept, but held back until the project is trusted.
    assert!(!cfg.mcp.iter().any(|s| s.name.starts_with("zz-test-")));
    let ours: Vec<(&str, Option<&str>)> = cfg
        .untrusted
        .as_ref()
        .expect("held back")
        .mcp
        .iter()
        .filter(|s| s.name.starts_with("zz-test-"))
        .map(|s| (s.name.as_str(), s.command.as_deref()))
        .collect();
    assert_eq!(ours, [("zz-test-ok", Some("x"))]);
    let warnings = cfg.config_warnings.join("\n");
    assert!(warnings.contains("zz-test-remote has a url"), "{warnings}");
    assert!(warnings.contains("zz-test-host asks to run on the host"), "{warnings}");
    assert!(warnings.contains("zz-test-ok is defined twice"), "{warnings}");
}

#[test]
fn a_projects_hooks_wait_until_it_is_trusted() {
    let tmp = temp_syl_dir();
    let syl = tmp.path().join(".syl");
    std::fs::create_dir_all(&syl).unwrap();
    std::fs::write(syl.join("config.toml"), "[[hook]]\nevent = \"session_start\"\ncommand = \"curl x | sh\"\n").unwrap();
    let cfg = build_config(&CliArgs::default(), &syl);
    assert!(cfg.hooks.is_empty());
    let held = cfg.untrusted.as_ref().expect("held back");
    assert_eq!(held.hooks.len(), 1);
    assert!(cfg.untrusted_notice().unwrap().contains("curl x | sh"));
}

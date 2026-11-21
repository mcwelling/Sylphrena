//! Regression test: `run_bash` must not deadlock when a command writes more
//! than a pipe buffer (~64 KB) to stderr before closing stdout.
//!
//! Uses a stand-in `podman` script on PATH that drops `exec <name>` and runs
//! the rest locally, so no real container is needed. Lives in its own test
//! binary because it modifies PATH for the process.
use std::os::unix::fs::PermissionsExt;

use serde_json::json;
use syl_rs::config::AppConfig;
use syl_rs::tools::bash::run_bash;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn large_stderr_does_not_deadlock() {
    let bin = tempfile::tempdir().unwrap();
    let fake = bin.path().join("podman");
    // podman exec <name> /bin/bash -c <cmd>  →  /bin/bash -c <cmd>
    std::fs::write(&fake, "#!/bin/sh\nshift 2\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
    std::env::set_var("PATH", path);

    let mut config = AppConfig::default();
    config.timeout_ms = 10_000;

    // 256 KB to stderr first, then a marker on stdout.
    let cmd = "head -c 262144 /dev/zero | tr '\\0' x >&2; echo done";
    let started = std::time::Instant::now();
    let (out, is_err) =
        run_bash(&json!({ "command": cmd }), &config, Default::default(), CancellationToken::new()).await;

    assert!(!is_err, "unexpected error: {out}");
    assert!(out.contains("done"), "{out}");
    assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
}

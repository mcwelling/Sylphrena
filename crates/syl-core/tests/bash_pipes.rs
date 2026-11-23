//! Regression test: `run_bash` must not deadlock when a command writes more
//! than a pipe buffer (~64 KB) to stderr before closing stdout.
mod common;

use serde_json::json;
use syl_core::config::AppConfig;
use syl_core::tools::bash::run_bash;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn large_stderr_does_not_deadlock() {
    let tmp = tempfile::tempdir().unwrap();
    let container = common::fake_container(tmp.path(), tmp.path());

    let config = AppConfig { timeout_ms: 10_000, ..Default::default() };

    // 256 KB to stderr first, then a marker on stdout.
    let cmd = "head -c 262144 /dev/zero | tr '\\0' x >&2; echo done";
    let started = std::time::Instant::now();
    let (out, is_err) =
        run_bash(&json!({ "command": cmd }), &config, &container, CancellationToken::new()).await;

    assert!(!is_err, "unexpected error: {out}");
    assert!(out.contains("done"), "{out}");
    assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
}

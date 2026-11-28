//! Checks against real podman, in a throwaway container for a temporary
//! project (its own `syl-agent-<hash>` name), removed at the end. Opt in:
//!
//! ```sh
//! SYL_REAL_PODMAN=1 cargo test -p syl-core --test real_podman -- --ignored --test-threads=1
//! ```
//!
//! Needs a running podman and the `syl-agent-ready:latest` image. HOME is
//! pointed at an empty directory for the test, so the container gets
//! neither `~/.ssh` nor the git identity; podman itself still gets the real
//! HOME, through a wrapper, to find its machine.
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use serde_json::json;
use syl_core::config::AppConfig;
use syl_core::container::{Container, ContainerOptions, ExecError, BAKED_IMAGE};
use syl_core::tools::bash::run_bash;
use tokio_util::sync::CancellationToken;

struct Throwaway {
    container: Container,
    _project: tempfile::TempDir,
}

/// The real HOME (read before it is changed) and the empty one the tests
/// run with, kept for the whole run.
fn homes() -> &'static (String, tempfile::TempDir) {
    static HOMES: std::sync::OnceLock<(String, tempfile::TempDir)> = std::sync::OnceLock::new();
    HOMES.get_or_init(|| {
        let real = std::env::var("HOME").unwrap();
        let empty = tempfile::tempdir().unwrap();
        // SAFETY: set once, before any test starts a container, and the
        // tests run one at a time (--test-threads=1).
        unsafe { std::env::set_var("HOME", empty.path()) };
        (real, empty)
    })
}

/// podman with the real HOME, to find its machine.
fn host_podman() -> std::process::Command {
    let mut c = std::process::Command::new("podman");
    c.env("HOME", &homes().0);
    c
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        self.container.remove(false);
    }
}

fn enabled() -> bool {
    if std::env::var("SYL_REAL_PODMAN").as_deref() != Ok("1") {
        eprintln!("skipped: set SYL_REAL_PODMAN=1");
        return false;
    }
    let ok = host_podman()
        .args(["image", "exists", BAKED_IMAGE])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: no {BAKED_IMAGE} image");
    }
    ok
}

fn throwaway(options: ContainerOptions) -> Throwaway {
    let (real_home, home) = homes();
    let project = tempfile::tempdir().unwrap();
    let podman = home.path().join("podman");
    let real = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v podman"]).output().unwrap().stdout)
        .unwrap();
    std::fs::write(&podman, format!("#!/bin/sh\nHOME='{real_home}' exec '{}' \"$@\"\n", real.trim())).unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();

    let container = syl_core::container::for_project(project.path()).with_podman(podman.clone()).with_options(options);
    assert!(container.name().starts_with("syl-agent-"));
    let t = Throwaway { container, _project: project };
    t.container.start_or_resume(false, &|m| eprintln!("{m}")).unwrap();
    t
}

fn podman(c: &Container, args: &[&str]) -> String {
    let out = host_podman().args(args).arg(c.name()).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn sh(c: &Container, cmd: &str) -> String {
    c.exec(cmd, Duration::from_secs(20), &CancellationToken::new()).await.unwrap().stdout.trim().to_string()
}

/// Processes left from a test command, found by a marker in their command
/// line.
async fn left(c: &Container, marker: &str) -> String {
    sh(c, &format!("ps -eo pid,args | grep -F -- '{marker}' | grep -v grep || true")).await
}

#[tokio::test]
#[ignore]
async fn auto_size_jobs_and_label() {
    if !enabled() {
        return;
    }
    let t = throwaway(ContainerOptions::default());
    let c = &t.container;
    let res = c.resources();
    eprintln!("resources: {res:?}");

    let info = host_podman()
        .args(["info", "--format", "{{.Host.MemTotal}}"])
        .output()
        .unwrap();
    let vm_mb: u64 = String::from_utf8_lossy(&info.stdout).trim().parse::<u64>().unwrap() / (1024 * 1024);
    assert_eq!(res.memory_mb, vm_mb * 3 / 4, "three quarters of {vm_mb} MB");

    let limit: u64 = podman(c, &["inspect", "--format", "{{.HostConfig.Memory}}"]).parse().unwrap();
    assert_eq!(limit, res.memory_mb * 1024 * 1024);
    let label = podman(c, &["inspect", "--format", "{{index .Config.Labels \"syl.options\"}}"]);
    assert_eq!(label, format!("memory={}m;cpus=;network=", res.memory_mb));

    let env = sh(c, "echo $CARGO_BUILD_JOBS $MAKEFLAGS $CMAKE_BUILD_PARALLEL_LEVEL").await;
    let n = res.jobs;
    assert_eq!(env, format!("{n} -j{n} {n}"));
    // The cgroup's limit, as seen from inside.
    let max = sh(c, "cat /sys/fs/cgroup/memory.max").await;
    assert_eq!(max, (res.memory_mb * 1024 * 1024).to_string());
    // Nothing has been killed, so the first read is only a baseline.
    assert_eq!(c.new_oom_kills().await, 0);
}

#[tokio::test]
#[ignore]
async fn a_timeout_stops_everything_the_command_started() {
    if !enabled() {
        return;
    }
    let t = throwaway(ContainerOptions::default());
    let c = &t.container;
    // A child in its own session, a background job and the foreground.
    let cmd = "setsid sleep 301 & (sleep 302 &) ; sleep 303";
    let started = Instant::now();
    let err = c.exec(cmd, Duration::from_secs(2), &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, ExecError::TimedOut(_)), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    for m in ["sleep 301", "sleep 302", "sleep 303"] {
        assert_eq!(left(c, m).await, "", "{m} still running");
    }
}

#[tokio::test]
#[ignore]
async fn a_cancel_stops_everything_too_and_a_finished_command_keeps_its_server() {
    if !enabled() {
        return;
    }
    let t = throwaway(ContainerOptions::default());
    let c = &t.container;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        stop.cancel();
    });
    let err = c
        .exec_argv(&["sh", "-c", "setsid sleep 311 & sleep 312"], None, Duration::from_secs(60), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");
    assert_eq!(left(c, "sleep 311").await, "");
    assert_eq!(left(c, "sleep 312").await, "");

    // Started on purpose by a command that finished: left running.
    sh(c, "setsid sleep 321 > /dev/null 2>&1 < /dev/null &").await;
    assert!(left(c, "sleep 321").await.contains("sleep 321"));
}

#[tokio::test]
#[ignore]
async fn run_bash_reports_an_out_of_memory_kill() {
    if !enabled() {
        return;
    }
    let t = throwaway(ContainerOptions { memory: "256m".into(), ..Default::default() });
    let c = &t.container;
    assert_eq!(c.resources().memory_mb, 256);
    assert_eq!(c.resources().jobs, 1);
    let config = AppConfig { timeout_ms: 60_000, ..Default::default() };
    // A child allocates more than the limit; the shell reports it the way
    // cargo would.
    let cmd = "python3 -c 'b = bytearray(1024 * 1024 * 1024); print(len(b))' || { echo 'child killed' >&2; exit 101; }";
    let (out, is_err) = run_bash(&json!({ "command": cmd }), &config, c, None, "oom", CancellationToken::new()).await;
    eprintln!("{out}");
    assert!(is_err);
    assert!(out.contains("ran out of memory during this command"), "{out}");
    assert!(out.contains("limit 256 MB, 1 build job"), "{out}");

    // An ordinary failure afterwards says nothing about memory.
    let (out, _) = run_bash(&json!({ "command": "exit 3" }), &config, c, None, "plain", CancellationToken::new()).await;
    assert!(!out.contains("memory"), "{out}");
}

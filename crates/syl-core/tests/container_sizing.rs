//! Container sizing: build-job variables reach commands, a timed-out or
//! cancelled command's processes are all stopped, and processes the memory
//! limit kills are reported.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use syl_core::config::AppConfig;
use syl_core::container::{Container, ExecError};
use syl_core::tools::bash::run_bash;
use tokio_util::sync::CancellationToken;

/// A stand-in podman that logs every call to `calls.log` and answers:
/// - `info`: a 4 GiB VM with 4 CPUs
/// - `exec ... memory.events`: `oom_kill N`, N one more on each call
/// - `exec ... SYL_EXEC=` (the kill script): nothing
/// - other `exec`s: runs the command on the host, dropping `timeout N`
fn logging_podman(dir: &Path) -> PathBuf {
    let fake = dir.join("podman");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
log="{dir}/calls.log"
printf '%s\n' "$*" >> "$log"
case "$1" in
  info) echo "4294967296 4" ;;
  exec)
    case "$*" in
      *memory.events*)
        n=$(cat "{dir}/oom" 2>/dev/null || echo 0)
        echo "oom_kill $n"
        echo $((n + 1)) > "{dir}/oom"
        exit 0 ;;
      *grep\ -qxF\ \'SYL_EXEC=*) exit 0 ;;
    esac
    shift
    while :; do
      case "$1" in
        --env) shift 2 ;;
        --workdir) cd "$2" || exit 125; shift 2 ;;
        -i) shift ;;
        *) break ;;
      esac
    done
    shift
    if [ "$1" = timeout ]; then shift 4; fi
    exec "$@"
    ;;
  inspect) echo true ;;
  *) exit 0 ;;
esac
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

fn container(dir: &Path) -> Container {
    let real = dir.canonicalize().unwrap();
    Container::for_project(dir)
        .with_podman(logging_podman(&real))
        .with_workdir(real.to_string_lossy().into_owned())
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.canonicalize().unwrap().join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// The `SYL_EXEC` tag a call carried in its environment.
fn tag_of(call: &str) -> Option<&str> {
    call.split_whitespace().find_map(|w| w.strip_prefix("SYL_EXEC="))
}

#[tokio::test]
async fn commands_get_build_jobs_that_fit_the_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let c = container(tmp.path());
    let res = c.resources();
    // 3 GB of a 4 GiB VM; 1 GB a job.
    assert_eq!((res.memory_mb, res.cpus, res.jobs), (3072, 4, 3));

    let out = c.exec("echo ok", Duration::from_secs(5), &CancellationToken::new()).await.unwrap();
    assert_eq!(out.stdout.trim(), "ok");
    let exec = calls(tmp.path()).into_iter().find(|l| l.starts_with("exec")).unwrap();
    for var in ["CARGO_BUILD_JOBS=3", "MAKEFLAGS=-j3", "CMAKE_BUILD_PARALLEL_LEVEL=3"] {
        assert!(exec.contains(&format!("--env {var}")), "{exec}");
    }
    assert!(tag_of(&exec).is_some(), "{exec}");
    // A finished command's processes are left alone (it may have started
    // a server on purpose).
    assert!(!calls(tmp.path()).iter().any(|l| l.contains("grep -qxF")));
}

#[tokio::test]
async fn a_timed_out_command_has_its_processes_killed() {
    let tmp = tempfile::tempdir().unwrap();
    let c = container(tmp.path());
    let err = c.exec("sleep 5", Duration::from_millis(300), &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, ExecError::TimedOut(_)), "{err}");

    let calls = calls(tmp.path());
    let tag = calls.iter().find(|l| l.contains("sleep 5")).and_then(|l| tag_of(l)).unwrap().to_string();
    let kill = calls.iter().find(|l| l.contains("grep -qxF")).expect("a kill call");
    assert!(kill.contains(&format!("'SYL_EXEC={tag}'")), "{kill}");
    // The kill call itself isn't tagged, so it can't kill itself.
    assert!(!kill.contains("--env SYL_EXEC"), "{kill}");
}

#[tokio::test]
async fn a_cancelled_command_has_its_processes_killed_too() {
    let tmp = tempfile::tempdir().unwrap();
    let c = container(tmp.path());
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.cancel();
    });
    let err = c
        .exec_argv(&["sleep", "5"], None, Duration::from_secs(10), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");

    let calls = calls(tmp.path());
    let tag = calls.iter().find(|l| l.contains("sleep 5")).and_then(|l| tag_of(l)).unwrap().to_string();
    assert!(calls.iter().any(|l| l.contains(&format!("'SYL_EXEC={tag}'"))), "{calls:?}");
}

#[tokio::test]
async fn run_bash_says_when_the_memory_limit_killed_something() {
    let tmp = tempfile::tempdir().unwrap();
    let c = container(tmp.path());
    let config = AppConfig { timeout_ms: 5_000, ..Default::default() };
    let input = json!({ "command": "echo 'signal: 9, SIGKILL' >&2; exit 101" });

    // The first count only sets the baseline: kills from before aren't
    // this command's.
    let (out, is_err) = run_bash(&input, &config, &c, None, "a", CancellationToken::new()).await;
    assert!(is_err);
    assert!(!out.contains("out of memory"), "{out}");

    // The stand-in counts one more kill on each read.
    let (out, is_err) = run_bash(&input, &config, &c, None, "b", CancellationToken::new()).await;
    assert!(is_err);
    assert!(out.contains("SIGKILL"), "{out}");
    assert!(out.contains("ran out of memory during this command and 1 process was killed"), "{out}");
    assert!(out.contains("limit 3.0 GB, 3 build jobs"), "{out}");
}

#[tokio::test]
async fn a_command_that_succeeds_is_not_checked_for_memory_kills() {
    let tmp = tempfile::tempdir().unwrap();
    let c = container(tmp.path());
    let config = AppConfig { timeout_ms: 5_000, ..Default::default() };
    let (out, is_err) = run_bash(&json!({ "command": "echo fine" }), &config, &c, None, "a", CancellationToken::new()).await;
    assert!(!is_err, "{out}");
    assert!(!calls(tmp.path()).iter().any(|l| l.contains("memory.events")));
}

#[tokio::test]
async fn a_timeout_says_the_command_was_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let c = container(tmp.path());
    let config = AppConfig { timeout_ms: 300, ..Default::default() };
    let (out, is_err) = run_bash(&json!({ "command": "sleep 5" }), &config, &c, None, "a", CancellationToken::new()).await;
    assert!(is_err);
    assert!(out.contains("timed out") && out.contains("were stopped"), "{out}");
}

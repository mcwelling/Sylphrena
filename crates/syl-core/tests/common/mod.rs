//! Shared test helpers.
#![allow(dead_code)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use syl_core::container::Container;

/// A stand-in `podman` that runs `exec` commands on the host and reports the
/// container as running, so tools work without a real container:
///
/// - `podman exec [-i] [--env K=V]... [--workdir D] <name> <argv...>` runs
///   `<argv...>` locally (in D)
/// - `podman inspect ...` prints `true`
/// - anything else succeeds silently
pub fn fake_podman(dir: &Path) -> std::path::PathBuf {
    let fake = dir.join("podman");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
case "$1" in
  exec)
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
    # The real container bounds commands with `timeout -s KILL N`; macOS
    # has no timeout(1), so drop it here.
    if [ "$1" = timeout ]; then shift 4; fi
    exec "$@"
    ;;
  inspect) echo true ;;
  *) exit 0 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

/// A container for `project_dir` backed by `fake_podman`, whose workspace
/// is the project directory itself (commands run on the host).
pub fn fake_container(project_dir: &Path, bin_dir: &Path) -> Container {
    let dir = project_dir.canonicalize().unwrap_or_else(|_| project_dir.to_path_buf());
    syl_core::container::for_project(project_dir)
        .with_podman(fake_podman(bin_dir))
        .with_workdir(dir.to_string_lossy().into_owned())
}

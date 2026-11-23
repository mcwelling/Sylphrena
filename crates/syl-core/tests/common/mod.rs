//! Shared test helpers.
#![allow(dead_code)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use syl_core::container::Container;

/// A stand-in `podman` that runs `exec` commands on the host and reports the
/// container as running, so tools work without a real container:
///
/// - `podman exec [--env K=V]... <name> <argv...>` runs `<argv...>` locally
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
    while [ "$1" = "--env" ]; do shift 2; done
    shift
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

/// A container for `project_dir` backed by `fake_podman`.
pub fn fake_container(project_dir: &Path, bin_dir: &Path) -> Container {
    Container::for_project(project_dir).with_podman(fake_podman(bin_dir))
}

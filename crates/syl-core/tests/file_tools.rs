//! The file tools against a stand-in container whose workspace is a temp
//! directory on the host.
mod common;

use serde_json::json;
use syl_core::config::AppConfig;
use syl_core::container::Container;
use syl_core::tools::fs;
use tokio_util::sync::CancellationToken;

struct Env {
    tmp: tempfile::TempDir,
    container: Container,
    config: AppConfig,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let container = common::fake_container(&project, &bin);
    Env { tmp, container, config: AppConfig { timeout_ms: 10_000, ..Default::default() } }
}

impl Env {
    fn file(&self, rel: &str) -> std::path::PathBuf {
        self.tmp.path().join("project").join(rel)
    }
    fn write(&self, rel: &str, text: &str) {
        let p = self.file(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.file(rel)).unwrap()
    }
}

fn c() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test]
async fn edit_replaces_a_unique_string_and_returns_a_diff() {
    let e = env();
    e.write("src/lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let (out, is_err) = fs::edit_file(
        &json!({"path": "src/lib.rs", "old_string": "fn b() {}", "new_string": "fn b() { todo!() }"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(!is_err, "{out}");
    assert_eq!(e.read("src/lib.rs"), "fn a() {}\nfn b() { todo!() }\nfn c() {}\n");
    assert!(out.starts_with("Edited "), "{out}");
    assert!(out.contains("--- a/src/lib.rs") && out.contains("-fn b() {}\n+fn b() { todo!() }"), "{out}");
}

#[tokio::test]
async fn edit_refuses_ambiguous_missing_and_noop_changes() {
    let e = env();
    e.write("x.txt", "same\nsame\n  indented\n");
    let edit = |old: &str, new: &str, all: bool| {
        json!({"path": "x.txt", "old_string": old, "new_string": new, "replace_all": all})
    };
    let (out, is_err) = fs::edit_file(&edit("same", "other", false), &e.config, &e.container, &c()).await;
    assert!(is_err && out.contains("occurs 2 times"), "{out}");
    let (out, is_err) = fs::edit_file(&edit("missing", "x", false), &e.config, &e.container, &c()).await;
    assert!(is_err && out.contains("not found"), "{out}");
    let (out, is_err) = fs::edit_file(&edit("indented\n  ", "x", false), &e.config, &e.container, &c()).await;
    assert!(is_err && out.contains("whitespace"), "{out}");
    let (out, is_err) = fs::edit_file(&edit("same", "same", false), &e.config, &e.container, &c()).await;
    assert!(is_err && out.contains("nothing to change"), "{out}");
    assert_eq!(e.read("x.txt"), "same\nsame\n  indented\n", "nothing was written");

    let (out, is_err) = fs::edit_file(&edit("same", "other", true), &e.config, &e.container, &c()).await;
    assert!(!is_err && out.contains("2 replacements"), "{out}");
    assert_eq!(e.read("x.txt"), "other\nother\n  indented\n");

    let (out, is_err) = fs::edit_file(
        &json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(is_err && out.contains("write_file"), "{out}");
}

#[tokio::test]
async fn write_creates_directories_and_diffs_against_the_old_file() {
    let e = env();
    let (out, is_err) = fs::write_file(
        &json!({"path": "deep/new/dir/f.md", "content": "# Title\n\nbody\n"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(!is_err && out.starts_with("Created ") && out.contains("+# Title"), "{out}");
    assert_eq!(e.read("deep/new/dir/f.md"), "# Title\n\nbody\n");

    let (out, _) = fs::write_file(
        &json!({"path": "deep/new/dir/f.md", "content": "# Title\n\nnew body\n"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(out.starts_with("Wrote ") && out.contains("-body\n+new body"), "{out}");
    let (out, _) = fs::write_file(
        &json!({"path": "deep/new/dir/f.md", "content": "# Title\n\nnew body\n"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(out.contains("nothing changed"), "{out}");
}

#[tokio::test]
async fn content_with_quotes_dollars_and_unicode_round_trips() {
    let e = env();
    let content = "echo \"$HOME\" '`rm -rf /`' \\n — ünïcødé ✓\n$(whoami)\n";
    let (out, is_err) =
        fs::write_file(&json!({"path": "weird.sh", "content": content}), &e.config, &e.container, &c()).await;
    assert!(!is_err, "{out}");
    assert_eq!(e.read("weird.sh"), content);
}

#[tokio::test]
async fn read_file_pages_by_line() {
    let e = env();
    let text: String = (1..=250).map(|i| format!("line {i}\n")).collect();
    e.write("big.txt", &text);
    let (out, is_err) =
        fs::read_file(&json!({"path": "big.txt", "start_line": 120, "end_line": 122}), &e.config, &e.container, &c())
            .await;
    assert!(!is_err, "{out}");
    assert!(out.contains("250 lines total"), "{out}");
    assert!(out.contains("  120: line 120") && out.contains("  122: line 122") && !out.contains("line 123"), "{out}");
    let (out, is_err) = fs::read_file(&json!({"path": "missing.txt"}), &e.config, &e.container, &c()).await;
    assert!(is_err && out.contains("not found"), "{out}");
}

#[tokio::test]
async fn grep_finds_matches_with_paths_relative_to_the_workspace() {
    let e = env();
    e.write("src/a.rs", "fn alpha() {}\nfn beta() {}\n");
    e.write("src/b.rs", "// Alpha in a comment\n");
    e.write("notes.md", "alpha\n");
    let (out, is_err) =
        fs::grep(&json!({"pattern": "alpha", "glob": "*.rs"}), &e.config, &e.container, &c()).await;
    assert!(!is_err, "{out}");
    assert!(out.contains("src/a.rs:1:fn alpha() {}"), "{out}");
    assert!(!out.contains("notes.md") && !out.contains("b.rs"), "{out}");
    assert!(!out.contains(e.container.workdir()), "paths are relative: {out}");

    let (out, _) =
        fs::grep(&json!({"pattern": "alpha", "ignore_case": true, "files_only": true}), &e.config, &e.container, &c())
            .await;
    assert_eq!(out.lines().count(), 3, "{out}");
    let (out, is_err) = fs::grep(&json!({"pattern": "zzz-nothing"}), &e.config, &e.container, &c()).await;
    assert!(!is_err && out == "No matches.", "{out}");
}

#[tokio::test]
async fn glob_matches_across_directories() {
    // The container's bash has globstar; macOS's /bin/bash 3.2 doesn't.
    if !std::process::Command::new("bash").args(["-c", "shopt -s globstar"]).status().unwrap().success() {
        eprintln!("skipping: this bash has no globstar");
        return;
    }
    let e = env();
    e.write("src/a.rs", "");
    e.write("src/deep/b.rs", "");
    e.write("README.md", "");
    let (out, is_err) = fs::glob(&json!({"pattern": "**/*.rs"}), &e.config, &e.container, &c()).await;
    assert!(!is_err, "{out}");
    assert_eq!(out.lines().collect::<Vec<_>>(), ["src/a.rs", "src/deep/b.rs"]);
}

#[tokio::test]
async fn ls_lists_a_directory() {
    let e = env();
    e.write("src/a.rs", "");
    let (out, is_err) = fs::ls(&json!({"path": "src"}), &e.config, &e.container, &c()).await;
    assert!(!is_err && out.contains("a.rs"), "{out}");
    let (out, is_err) = fs::ls(&json!({"path": "nope"}), &e.config, &e.container, &c()).await;
    assert!(is_err, "{out}");
}

// ---- review regressions ----

#[tokio::test]
async fn multi_line_edits_work_on_crlf_files() {
    let e = env();
    e.write("w.txt", "one\r\ntwo\r\nthree\r\n");
    let (out, is_err) = fs::edit_file(
        &json!({"path": "w.txt", "old_string": "one\ntwo", "new_string": "ONE\nTWO"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(!is_err, "{out}");
    assert_eq!(e.read("w.txt"), "ONE\r\nTWO\r\nthree\r\n");
}

#[tokio::test]
async fn writes_keep_the_mode_and_go_through_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    e.write("run.sh", "#!/bin/sh\n");
    std::fs::set_permissions(e.file("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("run.sh", e.file("link.sh")).unwrap();
    let (out, is_err) =
        fs::write_file(&json!({"path": "link.sh", "content": "#!/bin/sh\necho hi\n"}), &e.config, &e.container, &c())
            .await;
    assert!(!is_err, "{out}");
    assert_eq!(e.read("run.sh"), "#!/bin/sh\necho hi\n");
    assert!(std::fs::symlink_metadata(e.file("link.sh")).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::metadata(e.file("run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
    let leftovers: Vec<_> = std::fs::read_dir(e.file(""))
        .unwrap()
        .filter_map(|d| d.ok())
        .filter(|d| d.file_name().to_string_lossy().starts_with(".syl-write"))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn binary_files_can_be_replaced_even_with_nothing() {
    let e = env();
    std::fs::write(e.file("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
    let (out, is_err) = fs::write_file(&json!({"path": "blob.bin", "content": ""}), &e.config, &e.container, &c()).await;
    assert!(!is_err && out.contains("Replaced binary"), "{out}");
    assert_eq!(std::fs::read(e.file("blob.bin")).unwrap(), b"");
    let (out, is_err) = fs::edit_file(
        &json!({"path": "blob.bin", "old_string": "a", "new_string": "b"}),
        &e.config,
        &e.container,
        &c(),
    )
    .await;
    assert!(!is_err || out.contains("not found"), "{out}");
}

#[tokio::test]
async fn grep_fallback_handles_path_globs_and_skips_private_dirs() {
    let e = env();
    e.write("crates/a/src/lib.rs", "fn needle() {}\n");
    e.write(".syl/sessions/x.jsonl", "needle\n");
    e.write("target/debug/x.d", "needle\n");
    let (out, is_err) =
        fs::grep(&json!({"pattern": "needle", "glob": "crates/**/*.rs"}), &e.config, &e.container, &c()).await;
    assert!(!is_err && out.contains("crates/a/src/lib.rs:1:"), "{out}");
    let (out, _) = fs::grep(&json!({"pattern": "needle", "files_only": true}), &e.config, &e.container, &c()).await;
    assert_eq!(out.trim(), "crates/a/src/lib.rs", "{out}");
}

#[tokio::test]
async fn read_file_reports_past_the_end_and_directories() {
    let e = env();
    e.write("short.txt", "a\nb");
    let (out, _) = fs::read_file(&json!({"path": "short.txt"}), &e.config, &e.container, &c()).await;
    assert!(out.contains("2 lines total, showing 1-2"), "{out}");
    let (out, _) =
        fs::read_file(&json!({"path": "short.txt", "start_line": 9}), &e.config, &e.container, &c()).await;
    assert!(out.contains("past the end"), "{out}");
    e.write("d/x", "");
    let (out, is_err) = fs::read_file(&json!({"path": "d"}), &e.config, &e.container, &c()).await;
    assert!(is_err && out.contains("directory"), "{out}");
}

//! Checkpoints: snapshots of the workspace in a shadow git repository.
//!
//! Before the first tool call in a turn that could change files, the
//! workspace is committed to `.syl/shadow.git` (a bare repository whose work
//! tree is the project), so the turn can be undone. The user's own `.git` is
//! never touched: the shadow repo has its own index and history, and
//! ignores `.git` and `.syl`.
//!
//! Git runs on the host, because `.syl` is read-only in the container. It
//! runs isolated from the user's git configuration (no global or system
//! config, no hooks), so a filter or hook configured elsewhere never runs
//! on Syl's behalf. Restoring writes only files git tracked in the shadow
//! repo; git refuses to write through symlinked directories.
//!
//! Restoring is two steps: `preview` shows what would change, and `restore`
//! first snapshots the current state (so the restore itself can be undone),
//! then makes the work tree match the checkpoint.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Paths never snapshotted, besides whatever `.gitignore` files say: Syl's
/// own state, the user's repository, and the usual large build outputs.
const DEFAULT_EXCLUDES: &str = "/.git/\n/.syl/\nnode_modules/\ntarget/\n.venv/\n__pycache__/\n";

/// The shadow repository's whole config, rewritten before every use. A
/// project can ship a `.syl/shadow.git` of its own (a cloned repo, say),
/// and its config could define `core.fsmonitor` or filter drivers, which
/// git would run on the host. With this file in place, and the user's
/// global and system config off, no command from anywhere else runs.
const SHADOW_CONFIG: &str = "[core]\n\trepositoryformatversion = 0\n\tbare = true\n\tfsmonitor = false\n\
\thooksPath = /dev/null\n\tautocrlf = false\n\tquotePath = false\n\tlongpaths = true\n";

#[derive(Debug, Clone)]
pub struct Checkpoints {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

/// A file a restore would change, and how.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// `A` (would be created), `D` (would be deleted) or `M` (would change),
    /// from the work tree's side.
    pub status: char,
    pub path: String,
}

impl Checkpoints {
    pub fn for_project(project_dir: &Path) -> Self {
        Checkpoints { git_dir: project_dir.join(".syl/shadow.git"), work_tree: project_dir.to_path_buf() }
    }

    fn git(&self) -> Command {
        let mut c = Command::new("git");
        // Nothing of the user's git environment applies here (GIT_DIR,
        // GIT_CONFIG_PARAMETERS, GIT_OBJECT_DIRECTORY, ...).
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                c.env_remove(k);
            }
        }
        c.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "core.autocrlf=false"])
            .args(["-c", "core.quotePath=false"])
            .args(["-c", "user.name=Syl", "-c", "user.email=syl@localhost", "-c", "commit.gpgsign=false"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work_tree);
        c
    }

    fn run(&self, args: &[&str]) -> anyhow::Result<Output> {
        let out = self.git().args(args).output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// Create the shadow repository if needed, and (every time) reset the
    /// files that decide what git runs and what it snapshots.
    fn init(&self) -> anyhow::Result<()> {
        if self.git_dir.join("HEAD").exists() {
            self.own()?;
            self.clear_stale_lock();
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir)?;
        let out = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&self.git_dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()?;
        if !out.status.success() {
            anyhow::bail!("git init: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        self.own()
    }

    fn own(&self) -> anyhow::Result<()> {
        std::fs::write(self.git_dir.join("config"), SHADOW_CONFIG)?;
        std::fs::create_dir_all(self.git_dir.join("info"))?;
        std::fs::write(self.git_dir.join("info/exclude"), DEFAULT_EXCLUDES)?;
        // Alternates would let objects come from another repository.
        let _ = std::fs::remove_file(self.git_dir.join("objects/info/alternates"));
        Ok(())
    }

    /// A lock left by a crashed git would make every snapshot fail. One
    /// older than a minute isn't held by a git that is still running.
    fn clear_stale_lock(&self) {
        let lock = self.git_dir.join("index.lock");
        let stale = std::fs::metadata(&lock)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(60));
        if stale {
            tracing::warn!("removing a stale {}", lock.display());
            let _ = std::fs::remove_file(lock);
        }
    }

    /// Directories that are git repositories of their own: a snapshot holds
    /// only their commit id, not their files.
    pub fn nested_repos(&self) -> Vec<String> {
        let Ok(out) = self.run(&["ls-files", "--stage"]) else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.starts_with("160000 "))
            .filter_map(|l| l.split_once('\t').map(|(_, p)| p.to_string()))
            .collect()
    }

    /// Paths `.gitignore` hides right now, directories collapsed. Snapshots
    /// don't hold these, so nothing can put them back.
    pub fn ignored_paths(&self) -> anyhow::Result<Vec<String>> {
        self.init()?;
        let out = self.run(&["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"])?;
        Ok(out.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
    }

    fn head(&self) -> Option<String> {
        let out = self.git().args(["rev-parse", "--verify", "--quiet", "HEAD"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Commit the work tree as it is now. Returns the commit, which is the
    /// previous one when nothing changed since.
    pub fn snapshot(&self, message: &str) -> anyhow::Result<String> {
        self.init()?;
        // No --force: .gitignore files and the excludes above apply, and
        // .syl (this repository included) must never be added.
        self.run(&["add", "--all", "--", "."])?;
        let changed = match self.head() {
            None => true,
            Some(_) => !self.git().args(["diff", "--cached", "--quiet"]).status()?.success(),
        };
        if changed {
            self.run(&["commit", "--quiet", "--allow-empty", "--no-verify", "-m", message])?;
        }
        self.head().ok_or_else(|| anyhow::anyhow!("no checkpoint was recorded"))
    }

    /// What restoring `commit` would change in the work tree as it is now.
    pub fn preview(&self, commit: &str) -> anyhow::Result<Vec<Change>> {
        self.init()?;
        // Stage the current state so new files count, without committing.
        self.run(&["add", "--all", "--", "."])?;
        let out = self.run(&["diff", "--cached", "--name-status", "--no-renames", commit])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (status, path) = l.split_once('\t')?;
                // diff <commit> vs the index: A = new since the checkpoint
                // (a restore deletes it), D = gone since (a restore brings it back).
                let status = match status.chars().next()? {
                    'A' => 'D',
                    'D' => 'A',
                    _ => 'M',
                };
                Some(Change { status, path: path.to_string() })
            })
            .collect())
    }

    /// Make the work tree match `commit`. The current state is snapshotted
    /// first; its commit is returned, so the restore can be undone.
    pub fn restore(&self, commit: &str, why: &str) -> anyhow::Result<String> {
        // Files the restore will delete: present now, absent at the checkpoint.
        let doomed: Vec<String> = self.preview(commit)?.into_iter().filter(|c| c.status == 'D').map(|c| c.path).collect();
        let safety = self.snapshot(&format!("before restoring {}: {why}", short(commit)))?;
        // Index and work tree to the checkpoint: tracked files are rewritten,
        // files added since are removed, ignored files are left alone.
        self.run(&["read-tree", "--reset", "-u", commit])?;
        // A file the checkpoint's .gitignore hid (an .env, say) wasn't in the
        // checkpoint, but it isn't new either: if the turn changed .gitignore,
        // the safety snapshot caught it and read-tree just deleted it. Put
        // those back from the safety snapshot, outside the index.
        if !doomed.is_empty() {
            let mut check = self.git();
            check.args(["check-ignore", "--no-index", "--stdin", "-z"]);
            check.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
            let mut child = check.spawn()?;
            {
                use std::io::Write;
                let mut stdin = child.stdin.take().expect("piped");
                for p in &doomed {
                    stdin.write_all(p.as_bytes())?;
                    stdin.write_all(b"\0")?;
                }
            }
            let out = child.wait_with_output()?;
            let ignored: Vec<String> = out
                .stdout
                .split(|&b| b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            for path in ignored {
                self.run(&["checkout", &safety, "--", &path])?;
                self.run(&["rm", "--cached", "--quiet", "--", &path])?;
            }
        }
        // Record the restored state as the newest snapshot.
        self.run(&["commit", "--quiet", "--allow-empty", "--no-verify", "-m", &format!("restored {}", short(commit))])?;
        Ok(safety)
    }

    /// Whether the work tree differs from `commit`.
    pub fn changed_since(&self, commit: &str) -> anyhow::Result<bool> {
        Ok(!self.preview(commit)?.is_empty())
    }
}

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn rm_rf_is_undone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, "src/a.rs", "fn a() {}\n");
        write(dir, "README.md", "hi\n");
        write(dir, ".syl/sessions/x.jsonl", "{}\n");
        let cp = Checkpoints::for_project(dir);
        let before = cp.snapshot("before turn 1").unwrap();

        // The turn deletes everything and adds a stray file.
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        std::fs::remove_file(dir.join("README.md")).unwrap();
        write(dir, "junk.txt", "junk\n");

        let changes = cp.preview(&before).unwrap();
        let mut seen: Vec<(char, &str)> = changes.iter().map(|c| (c.status, c.path.as_str())).collect();
        seen.sort();
        assert_eq!(seen, [('A', "README.md"), ('A', "src/a.rs"), ('D', "junk.txt")]);

        let safety = cp.restore(&before, "undo").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("src/a.rs")).unwrap(), "fn a() {}\n");
        assert!(!dir.join("junk.txt").exists());
        assert!(dir.join(".syl/sessions/x.jsonl").exists(), ".syl is never touched");
        assert!(!cp.changed_since(&before).unwrap());

        // The restore itself can be undone.
        cp.restore(&safety, "redo").unwrap();
        assert!(dir.join("junk.txt").exists() && !dir.join("src").exists());
    }

    #[test]
    fn the_users_git_repo_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let ok = Command::new("git").args(["init", "--quiet"]).current_dir(dir).status().unwrap().success();
        assert!(ok);
        write(dir, "f.txt", "1\n");
        let cp = Checkpoints::for_project(dir);
        let c1 = cp.snapshot("one").unwrap();
        write(dir, "f.txt", "2\n");
        cp.restore(&c1, "undo").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "1\n");
        // The user's repo has no commits and nothing staged by us.
        let log = Command::new("git").args(["log", "--oneline"]).current_dir(dir).output().unwrap();
        assert!(!log.status.success() || log.stdout.is_empty());
        let staged = Command::new("git").args(["diff", "--cached", "--name-only"]).current_dir(dir).output().unwrap();
        assert!(staged.stdout.is_empty());
    }

    #[test]
    fn a_shadow_repo_shipped_with_the_project_cannot_run_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, "a.txt", "x\n");
        let cp = Checkpoints::for_project(dir);
        cp.snapshot("first").unwrap();
        // Someone plants a config with an fsmonitor and a filter driver.
        let pwned = dir.join("PWNED");
        let cmd = format!("touch {}", pwned.display());
        std::fs::write(
            dir.join(".syl/shadow.git/config"),
            format!("[core]\n\tbare = true\n\tfsmonitor = {cmd}\n[filter \"x\"]\n\tclean = {cmd}\n"),
        )
        .unwrap();
        write(dir, ".gitattributes", "* filter=x\n");
        write(dir, "a.txt", "y\n");
        cp.snapshot("second").unwrap();
        assert!(!pwned.exists(), "a planted command ran");
    }

    #[test]
    fn restore_keeps_ignored_files_when_the_turn_changed_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, ".gitignore", ".env\n");
        write(dir, ".env", "SECRET=1\n");
        write(dir, "a.txt", "x\n");
        let cp = Checkpoints::for_project(dir);
        let before = cp.snapshot("before").unwrap();
        std::fs::remove_file(dir.join(".gitignore")).unwrap();
        write(dir, "a.txt", "changed\n");
        cp.restore(&before, "undo").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(".env")).unwrap(), "SECRET=1\n");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "x\n");
        assert!(dir.join(".gitignore").exists());
    }

    #[test]
    fn nested_repositories_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, "vendor/lib/x.rs", "x\n");
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.join("vendor/lib"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
        };
        git(&["init", "--quiet"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "x"]);
        let cp = Checkpoints::for_project(dir);
        cp.snapshot("s").unwrap();
        assert_eq!(cp.nested_repos(), ["vendor/lib"]);
    }

    #[test]
    fn snapshotting_twice_without_changes_reuses_the_commit() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a", "x");
        let cp = Checkpoints::for_project(tmp.path());
        assert_eq!(cp.snapshot("1").unwrap(), cp.snapshot("2").unwrap());
    }
}

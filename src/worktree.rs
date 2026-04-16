use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::command::{run_built_command, run_built_command_output, run_command};
use crate::error::AmError;

/// Resolve the `git` binary path, respecting the `AM_GIT_BIN` env override.
fn git_bin() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AM_GIT_BIN") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
        // If AM_GIT_BIN is a binary name like "git", try to locate it on PATH.
        if let Ok(found) = which::which(&path) {
            return Ok(found);
        }
        return Err(AmError::WorktreeError(format!(
            "git binary not found (AM_GIT_BIN is set to {path} but was not found)"
        ))
        .into());
    }
    which::which("git")
        .map_err(|_| AmError::WorktreeError("git not found on PATH".to_string()).into())
}

/// Run a `git` subcommand with the given args in the given directory.
fn run_git(bin: &Path, repo_root: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("-C").arg(repo_root).arg("--no-pager").args(args);
    run_built_command(cmd, AmError::WorktreeError)
}

/// Run a `git` subcommand and return stdout.
fn run_git_output(bin: &Path, repo_root: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("-C").arg(repo_root).arg("--no-pager").args(args);
    run_built_command_output(cmd, AmError::WorktreeError)
}

/// Returns true if the branch `am/<slug>` exists in the repo at `repo_root`.
fn branch_exists(bin: &Path, slug: &str, repo_root: &Path) -> bool {
    let branch_ref = format!("refs/heads/am/{slug}");
    run_git_output(bin, repo_root, &["rev-parse", "--verify", &branch_ref]).is_ok()
}

/// Create a git worktree for `slug` at `<repo-root>/.am/worktrees/<slug>`.
/// Creates branch `am/<slug>` off HEAD. Errors with `SlugAlreadyExists` if
/// the branch already exists.
pub fn create_git_worktree(slug: &str, repo_root: &Path) -> Result<PathBuf> {
    let bin = git_bin()?;

    // Check for unborn HEAD (no commits yet) before anything else
    if run_git_output(&bin, repo_root, &["rev-parse", "HEAD"]).is_err() {
        return Err(AmError::WorktreeError(
            "repository has no commits yet — make an initial commit before running 'am start'"
                .to_string(),
        )
        .into());
    }

    if branch_exists(&bin, slug, repo_root) {
        return Err(AmError::SlugAlreadyExists(slug.to_string()).into());
    }

    // Ensure parent directory exists
    let worktree_path = repo_root.join(".am").join("worktrees").join(slug);
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let branch_name = format!("am/{slug}");
    // `git worktree add -b <branch> <path>` creates branch off HEAD and checks it out
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("-C")
        .arg(repo_root)
        .arg("--no-pager")
        .args(["worktree", "add", "-b"])
        .arg(&branch_name)
        .arg(&worktree_path);
    run_built_command(cmd, AmError::WorktreeError)?;

    Ok(worktree_path)
}

/// Resolve the `jj` binary path, respecting the `AM_JJ_BIN` env override.
fn jj_bin() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AM_JJ_BIN") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
        if let Ok(found) = which::which(&path) {
            return Ok(found);
        }
        return Err(AmError::WorktreeError(format!(
            "jj binary not found (AM_JJ_BIN is set to {path} but was not found)"
        ))
        .into());
    }
    which::which("jj").map_err(|_| {
        AmError::WorktreeError(
            "jj not found on PATH — install from https://jj-vcs.github.io/jj/".to_string(),
        )
        .into()
    })
}

/// Run a `jj` subcommand with the given args, returning an error on non-zero exit.
fn run_jj(bin: &Path, args: &[&str]) -> Result<()> {
    run_command(&bin.to_string_lossy(), args, AmError::WorktreeError)
}

/// Create a jj workspace for `slug` at `<repo-root>/.am/worktrees/<slug>`.
pub fn create_jj_workspace(slug: &str, repo_root: &Path) -> Result<PathBuf> {
    let bin = jj_bin()?;
    let worktree_path = repo_root.join(".am").join("worktrees").join(slug);
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_str = worktree_path.to_string_lossy();
    run_jj(&bin, &["workspace", "add", &path_str, "--name", slug])?;
    Ok(worktree_path)
}

/// Remove the jj workspace for `slug` and delete the workspace directory.
pub fn remove_jj_workspace(slug: &str, repo_root: &Path) -> Result<()> {
    let bin = jj_bin()?;
    run_jj(&bin, &["workspace", "forget", slug])?;
    let worktree_path = repo_root.join(".am").join("worktrees").join(slug);
    if worktree_path.exists() {
        std::fs::remove_dir_all(&worktree_path)
            .map_err(|e| AmError::WorktreeError(format!("failed to remove directory: {e}")))?;
    }
    Ok(())
}

/// Returns true if the git worktree at `worktree_path` has any uncommitted changes
/// (staged, unstaged, or untracked). Returns false if the path doesn't exist or
/// any error occurs — callers use this for a best-effort warning only.
pub fn git_worktree_has_changes(worktree_path: &Path) -> bool {
    let Ok(bin) = git_bin() else { return false };
    // `git status --porcelain` prints nothing if clean, lines if dirty
    let output = std::process::Command::new(bin)
        .arg("-C")
        .arg(worktree_path)
        .args(["--no-pager", "status", "--porcelain", "-uall"])
        .output();
    match output {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        _ => false,
    }
}

/// Remove the git worktree for `slug` and delete the `am/<slug>` branch.
pub fn remove_git_worktree(slug: &str, repo_root: &Path) -> Result<()> {
    let bin = git_bin()?;

    // Remove the directory first — once it's gone git treats the worktree as
    // invalid, which lets prune succeed without special flags.
    let worktree_path = repo_root.join(".am").join("worktrees").join(slug);
    if worktree_path.exists() {
        std::fs::remove_dir_all(&worktree_path)
            .map_err(|e| AmError::WorktreeError(format!("failed to remove directory: {e}")))?;
    }

    // Prune stale worktree registration
    let _ = run_git(&bin, repo_root, &["worktree", "prune"]);

    // Delete the branch
    let branch_name = format!("am/{slug}");
    if branch_exists(&bin, slug, repo_root) {
        run_git(&bin, repo_root, &["branch", "-D", &branch_name])?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Init a repo and make an initial commit so HEAD exists.
    fn init_repo_with_commit(dir: &Path) {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("init")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "--allow-empty", "-m", "initial commit"])
            .output()
            .unwrap();
    }

    // ── jj helpers ────────────────────────────────────────────────────────────

    /// Create a fake `jj` script that logs its args and exits 0.
    fn fake_jj(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("jj");
        std::fs::write(&bin, "#!/bin/sh\necho \"$*\" >> \"$AM_JJ_LOG\"\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    fn read_jj_log(log: &Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    #[test]
    fn create_jj_workspace_runs_correct_command() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let bin = fake_jj(tmp.path());
        let log = tmp.path().join("jj.log");
        std::env::set_var("AM_JJ_BIN", &bin);
        std::env::set_var("AM_JJ_LOG", &log);

        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        create_jj_workspace("feat", &repo_root).unwrap();

        let out = read_jj_log(&log);
        assert!(out.contains("workspace"), "expected 'workspace': {out}");
        assert!(out.contains("add"), "expected 'add': {out}");
        assert!(out.contains("feat"), "expected slug 'feat': {out}");

        std::env::remove_var("AM_JJ_BIN");
        std::env::remove_var("AM_JJ_LOG");
    }

    #[test]
    fn create_jj_workspace_returns_correct_path() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let bin = fake_jj(tmp.path());
        let log = tmp.path().join("jj.log");
        std::env::set_var("AM_JJ_BIN", &bin);
        std::env::set_var("AM_JJ_LOG", &log);

        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let path = create_jj_workspace("feat", &repo_root).unwrap();

        assert_eq!(path, repo_root.join(".am").join("worktrees").join("feat"));

        std::env::remove_var("AM_JJ_BIN");
        std::env::remove_var("AM_JJ_LOG");
    }

    #[test]
    fn remove_jj_workspace_calls_forget_and_removes_directory() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let bin = fake_jj(tmp.path());
        let log = tmp.path().join("jj.log");
        std::env::set_var("AM_JJ_BIN", &bin);
        std::env::set_var("AM_JJ_LOG", &log);

        let repo_root = tmp.path().join("repo");
        let worktree_path = repo_root.join(".am").join("worktrees").join("feat");
        std::fs::create_dir_all(&worktree_path).unwrap();

        remove_jj_workspace("feat", &repo_root).unwrap();

        let out = read_jj_log(&log);
        assert!(out.contains("workspace"), "expected 'workspace': {out}");
        assert!(out.contains("forget"), "expected 'forget': {out}");
        assert!(out.contains("feat"), "expected slug 'feat': {out}");
        assert!(
            !worktree_path.exists(),
            "worktree directory should be removed"
        );

        std::env::remove_var("AM_JJ_BIN");
        std::env::remove_var("AM_JJ_LOG");
    }

    #[test]
    fn remove_jj_workspace_succeeds_when_directory_already_gone() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let bin = fake_jj(tmp.path());
        let log = tmp.path().join("jj.log");
        std::env::set_var("AM_JJ_BIN", &bin);
        std::env::set_var("AM_JJ_LOG", &log);

        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        // Directory does not exist — should not error
        remove_jj_workspace("feat", &repo_root).unwrap();

        std::env::remove_var("AM_JJ_BIN");
        std::env::remove_var("AM_JJ_LOG");
    }

    // ── git helpers ───────────────────────────────────────────────────────────

    #[test]
    fn create_git_worktree_errors_on_unborn_branch() {
        let tmp = TempDir::new().unwrap();
        // Init repo but make NO initial commit — HEAD is unborn
        std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .output()
            .unwrap();

        let err = create_git_worktree("feat", tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("no commits yet"),
            "expected helpful unborn-branch message, got: {err}"
        );
    }

    #[test]
    fn create_git_worktree_creates_branch_and_directory() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        let worktree_path = create_git_worktree("feat", tmp.path()).unwrap();

        assert!(worktree_path.exists(), "worktree directory should exist");
        assert_eq!(
            worktree_path,
            tmp.path().join(".am").join("worktrees").join("feat")
        );

        // Branch should exist
        let bin = git_bin().unwrap();
        assert!(branch_exists(&bin, "feat", tmp.path()));
    }

    #[test]
    fn create_git_worktree_supports_non_utf8_repo_paths() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join(OsString::from_vec(b"repo-\xFF".to_vec()));
        std::fs::create_dir_all(&repo_root).unwrap();
        init_repo_with_commit(&repo_root);

        let worktree_path = create_git_worktree("feat", &repo_root).unwrap();

        assert!(worktree_path.exists(), "worktree directory should exist");
        assert_eq!(
            worktree_path,
            repo_root.join(".am").join("worktrees").join("feat")
        );

        let bin = git_bin().unwrap();
        assert!(branch_exists(&bin, "feat", &repo_root));
    }

    #[test]
    fn create_git_worktree_duplicate_slug_errors() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        create_git_worktree("feat", tmp.path()).unwrap();
        let err = create_git_worktree("feat", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("feat"));
    }

    #[test]
    fn create_git_worktree_succeeds_when_worktrees_dir_already_exists() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        // Pre-create the worktrees directory (as would happen after a prior session)
        std::fs::create_dir_all(tmp.path().join(".am").join("worktrees")).unwrap();

        let worktree_path = create_git_worktree("feat", tmp.path()).unwrap();
        assert!(worktree_path.exists(), "worktree directory should exist");
    }

    #[test]
    fn remove_git_worktree_removes_directory_and_branch() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        let worktree_path = create_git_worktree("feat", tmp.path()).unwrap();
        assert!(worktree_path.exists());

        remove_git_worktree("feat", tmp.path()).unwrap();

        assert!(!worktree_path.exists(), "worktree directory should be gone");
        let bin = git_bin().unwrap();
        assert!(
            !branch_exists(&bin, "feat", tmp.path()),
            "branch should be deleted"
        );
    }

    #[test]
    fn remove_git_worktree_succeeds_when_branch_already_gone() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());

        create_git_worktree("feat", tmp.path()).unwrap();

        // Detach the worktree so git allows branch deletion, then delete the branch.
        let bin = git_bin().unwrap();
        let worktree_path = tmp.path().join(".am").join("worktrees").join("feat");
        std::fs::remove_dir_all(&worktree_path).unwrap();
        let _ = run_git(&bin, tmp.path(), &["worktree", "prune"]);
        run_git(&bin, tmp.path(), &["branch", "-D", "am/feat"]).unwrap();

        // remove_git_worktree should succeed even though the branch is already gone
        remove_git_worktree("feat", tmp.path()).unwrap();
    }

    // ── git_worktree_has_changes ───────────────────────────────────────────────

    #[test]
    fn git_worktree_has_changes_returns_false_for_clean_worktree() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        let worktree_path = create_git_worktree("feat", tmp.path()).unwrap();

        assert!(!git_worktree_has_changes(&worktree_path));
    }

    #[test]
    fn git_worktree_has_changes_returns_true_when_file_modified() {
        let tmp = TempDir::new().unwrap();
        init_repo_with_commit(tmp.path());
        let worktree_path = create_git_worktree("feat", tmp.path()).unwrap();

        std::fs::write(worktree_path.join("dirty.txt"), "uncommitted").unwrap();

        assert!(git_worktree_has_changes(&worktree_path));
    }

    #[test]
    fn git_worktree_has_changes_returns_false_for_nonexistent_path() {
        let tmp = TempDir::new().unwrap();
        assert!(!git_worktree_has_changes(&tmp.path().join("no-such-dir")));
    }
}

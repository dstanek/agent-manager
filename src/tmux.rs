use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::command;
use crate::config::SplitDirection;
use crate::error::AmError;

// Path handling strategy (preserve type safety as long as possible):
// - Keep as Path/PathBuf in internal code
// - Use &Path in function parameters (not &str)
// - Convert to String only at boundaries (Command args, logging, display)
// - Prefer .to_string_lossy() for command arguments (handles UTF-8 gracefully)
// - Use .display() for logging/error messages (implements Display trait)

/// Returns the tmux binary path, respecting the `AM_TMUX_BIN` env override.
fn tmux_bin() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AM_TMUX_BIN") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
        // If AM_TMUX_BIN is a binary name like "tmux", try to locate it on PATH.
        if let Ok(found) = which::which(&path) {
            return Ok(found);
        }
        return Err(AmError::TmuxError(format!(
            "tmux binary not found (AM_TMUX_BIN is set to {path} but was not found)"
        ))
        .into());
    }
    which::which("tmux")
        .map_err(|_| AmError::TmuxError("tmux not found on PATH".to_string()).into())
}

/// Locate tmux without requiring it, for readiness reporting.
///
/// `am` works outside tmux — it launches the container directly — so callers that
/// only want to *know* whether tmux is available should not have to handle an error.
pub fn find_tmux() -> Option<PathBuf> {
    tmux_bin().ok()
}

fn run_tmux(bin: &Path, args: &[&str]) -> Result<()> {
    command::run_command(&bin.to_string_lossy(), args, AmError::TmuxError)
}

/// Returns `true` if the `$TMUX` environment variable is set (i.e. we are
/// running inside a tmux session).
pub fn is_in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// `tmux new-window -n <window_name> -c <working_dir>`
pub fn create_window(window_name: &str, working_dir: &Path) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(
        &bin,
        &[
            "new-window",
            "-n",
            window_name,
            "-c",
            &working_dir.to_string_lossy(),
        ],
    )
}

/// Split an existing window.
/// Horizontal: `tmux split-window -h -p <new_pane_percent> -c <working_dir> -t <window_name> [shell_cmd]`
/// Vertical:   `tmux split-window -v -p <new_pane_percent> -c <working_dir> -t <window_name> [shell_cmd]`
///
/// `new_pane_percent` is the percentage of the window given to the **new** pane (1–99).
/// The caller is responsible for mapping the logical agent/shell pane assignment to the
/// correct percentage (e.g. if the agent is in the new pane, pass `split_percent` directly;
/// if the agent is in the original pane, pass `100 - split_percent`).
///
/// `shell_cmd` — when `Some`, tmux runs this command (via `$SHELL -c`) in the new pane
/// instead of an interactive shell.
pub fn split_window(
    window_name: &str,
    working_dir: &Path,
    direction: &SplitDirection,
    new_pane_percent: u8,
    shell_cmd: Option<&str>,
) -> Result<()> {
    let bin = tmux_bin()?;
    let flag = match direction {
        SplitDirection::Horizontal => "-h",
        SplitDirection::Vertical => "-v",
    };
    let percent = new_pane_percent.to_string();
    let wd = working_dir.to_string_lossy();
    let mut args = vec![
        "split-window",
        flag,
        "-p",
        &percent,
        "-c",
        &wd,
        "-t",
        window_name,
    ];
    if let Some(cmd) = shell_cmd {
        args.push(cmd);
    }
    run_tmux(&bin, &args)
}

/// `tmux select-pane -t <target>`
pub fn select_pane(target: &str) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(&bin, &["select-pane", "-t", target])
}

/// `tmux select-window -t <window_name>`
pub fn select_window(window_name: &str) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(&bin, &["select-window", "-t", window_name])
}

/// `tmux send-keys -t <pane_target> "<keys>" Enter`
pub fn send_keys(pane_target: &str, keys: &str) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(&bin, &["send-keys", "-t", pane_target, keys, "Enter"])
}

/// `tmux kill-window -t <window_name>`
pub fn kill_window(window_name: &str) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(&bin, &["kill-window", "-t", window_name])
}

/// `tmux kill-pane -t <target>`
pub fn kill_pane(target: &str) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(&bin, &["kill-pane", "-t", target])
}

/// Rename a tmux window.
/// If `target` is `None`, renames the current window.
/// `tmux rename-window [-t <target>] <new_name>`
pub fn rename_window(target: Option<&str>, new_name: &str) -> Result<()> {
    let bin = tmux_bin()?;
    match target {
        Some(t) => run_tmux(&bin, &["rename-window", "-t", t, new_name]),
        None => run_tmux(&bin, &["rename-window", new_name]),
    }
}

/// Returns the pane target string `"<window_name>.<index>"`.
pub fn get_pane_id(window_name: &str, index: usize) -> String {
    format!("{window_name}.{index}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Serialize tests that mutate AM_TMUX_BIN / MOCK_TMUX_LOG env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Write a mock tmux script that appends its args to `$MOCK_TMUX_LOG`.
    fn make_mock_tmux(dir: &Path) -> std::path::PathBuf {
        let script = dir.join("mock_tmux");
        std::fs::write(&script, "#!/bin/sh\necho \"$*\" >> \"$MOCK_TMUX_LOG\"\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    struct MockTmux {
        _tmp: TempDir,
        log: std::path::PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl MockTmux {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap();
            let tmp = TempDir::new().unwrap();
            let log = tmp.path().join("tmux.log");
            let bin = make_mock_tmux(tmp.path());
            std::env::set_var("AM_TMUX_BIN", &bin);
            std::env::set_var("MOCK_TMUX_LOG", &log);
            Self {
                _tmp: tmp,
                log,
                _guard: guard,
            }
        }

        fn captured(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    impl Drop for MockTmux {
        fn drop(&mut self) {
            std::env::remove_var("AM_TMUX_BIN");
            std::env::remove_var("MOCK_TMUX_LOG");
        }
    }

    // ── is_in_tmux ────────────────────────────────────────────────────────────

    #[test]
    fn is_in_tmux_true_when_tmux_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TMUX", "/tmp/tmux-1000/default,12345,0");
        assert!(is_in_tmux());
        std::env::remove_var("TMUX");
    }

    #[test]
    fn is_in_tmux_false_when_tmux_not_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TMUX");
        assert!(!is_in_tmux());
    }

    // ── get_pane_id ───────────────────────────────────────────────────────────

    #[test]
    fn get_pane_id_formats_correctly() {
        assert_eq!(get_pane_id("am-feat", 0), "am-feat.0");
        assert_eq!(get_pane_id("am-feat", 1), "am-feat.1");
        assert_eq!(get_pane_id("am-my-session", 2), "am-my-session.2");
    }

    // ── command-building tests ────────────────────────────────────────────────

    #[test]
    fn create_window_sends_correct_command() {
        let mock = MockTmux::new();
        create_window("am-feat", Path::new("/tmp/worktree")).unwrap();
        let out = mock.captured();
        assert!(
            out.contains("new-window"),
            "expected new-window, got: {out}"
        );
        assert!(out.contains("-n"), "expected -n flag");
        assert!(out.contains("am-feat"));
        assert!(out.contains("/tmp/worktree"));
    }

    #[test]
    fn split_window_horizontal_sends_correct_command() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Horizontal,
            50,
            None,
        )
        .unwrap();
        let out = mock.captured();
        assert!(out.contains("split-window"));
        assert!(out.contains("-h"));
        assert!(out.contains("am-feat"));
    }

    #[test]
    fn split_window_vertical_sends_correct_command() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Vertical,
            50,
            None,
        )
        .unwrap();
        let out = mock.captured();
        assert!(out.contains("split-window"));
        assert!(out.contains("-v"));
    }

    #[test]
    fn split_window_passes_percent_flag() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Horizontal,
            30,
            None,
        )
        .unwrap();
        let out = mock.captured();
        assert!(out.contains("-p"), "expected -p flag, got: {out}");
        assert!(out.contains("30"), "expected percent value 30, got: {out}");
    }

    #[test]
    fn split_window_with_shell_cmd_appends_command() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Horizontal,
            50,
            Some("podman run --rm -it myimage"),
        )
        .unwrap();
        let out = mock.captured();
        assert!(out.contains("split-window"));
        assert!(
            out.contains("podman run --rm -it myimage"),
            "expected shell command in output, got: {out}"
        );
    }

    #[test]
    fn select_pane_sends_correct_command() {
        let mock = MockTmux::new();
        select_pane("am-feat.0").unwrap();
        let out = mock.captured();
        assert!(out.contains("select-pane"));
        assert!(out.contains("am-feat.0"));
    }

    #[test]
    fn select_window_sends_correct_command() {
        let mock = MockTmux::new();
        select_window("am-feat").unwrap();
        let out = mock.captured();
        assert!(out.contains("select-window"));
        assert!(out.contains("am-feat"));
    }

    #[test]
    fn send_keys_sends_correct_command() {
        let mock = MockTmux::new();
        send_keys("am-feat.0", "claude").unwrap();
        let out = mock.captured();
        assert!(out.contains("send-keys"));
        assert!(out.contains("am-feat.0"));
        assert!(out.contains("claude"));
        assert!(out.contains("Enter"));
    }

    #[test]
    fn kill_window_sends_correct_command() {
        let mock = MockTmux::new();
        kill_window("am-feat").unwrap();
        let out = mock.captured();
        assert!(out.contains("kill-window"));
        assert!(out.contains("am-feat"));
    }

    #[test]
    fn kill_pane_sends_correct_command() {
        let mock = MockTmux::new();
        kill_pane("am-feat.1").unwrap();
        let out = mock.captured();
        assert!(out.contains("kill-pane"));
        assert!(out.contains("am-feat.1"));
    }

    #[test]
    fn rename_window_without_target_omits_t_flag() {
        let mock = MockTmux::new();
        rename_window(None, "new-name").unwrap();
        let out = mock.captured();
        assert!(out.contains("rename-window"));
        assert!(out.contains("new-name"));
        assert!(!out.contains("-t"));
    }

    #[test]
    fn rename_window_with_target_passes_t_flag() {
        let mock = MockTmux::new();
        rename_window(Some("am-feat"), "old-name").unwrap();
        let out = mock.captured();
        assert!(out.contains("rename-window"));
        assert!(out.contains("-t"));
        assert!(out.contains("am-feat"));
        assert!(out.contains("old-name"));
    }
}

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

fn run_tmux_output(bin: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args);
    command::run_built_command_output(cmd, AmError::TmuxError)
}

/// Returns `true` if the `$TMUX` environment variable is set (i.e. we are
/// running inside a tmux session).
pub fn is_in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Create a window with a human-facing title and return its stable tmux ID.
pub fn create_window(window_name: &str, working_dir: &Path) -> Result<String> {
    let bin = tmux_bin()?;
    let id = run_tmux_output(
        &bin,
        &[
            "new-window",
            "-P",
            "-F",
            "#{window_id}",
            "-n",
            window_name,
            "-c",
            &working_dir.to_string_lossy(),
        ],
    )?;
    if id.is_empty() {
        return Err(AmError::TmuxError("tmux did not return a window ID".to_string()).into());
    }
    Ok(id)
}

/// Split an existing window.
/// Horizontal: `tmux split-window -h [-b] -p <new_pane_percent> -c <working_dir> -t <window_name> [shell_cmd]`
/// Vertical:   `tmux split-window -v [-b] -p <new_pane_percent> -c <working_dir> -t <window_name> [shell_cmd]`
///
/// `new_pane_percent` is the percentage of the window given to the **new** pane (1–99).
///
/// `before` adds `-b`, placing the new pane left of (horizontal) or above (vertical) the
/// existing one, which also makes it pane index 0. This is what lets a caller put the new
/// pane on either side without giving up the ability to run a command in it.
///
/// `shell_cmd` — when `Some`, tmux runs this command (via `$SHELL -c`) in the new pane
/// instead of an interactive shell.
pub fn split_window(
    window_name: &str,
    working_dir: &Path,
    direction: &SplitDirection,
    new_pane_percent: u8,
    before: bool,
    shell_cmd: Option<&str>,
) -> Result<()> {
    let bin = tmux_bin()?;
    let flag = match direction {
        SplitDirection::Horizontal => "-h",
        SplitDirection::Vertical => "-v",
    };
    let percent = new_pane_percent.to_string();
    let wd = working_dir.to_string_lossy();
    let mut args = vec!["split-window", flag];
    if before {
        args.push("-b");
    }
    args.extend_from_slice(&["-p", &percent, "-c", &wd, "-t", window_name]);
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
///
/// This *types* `keys` into whatever shell is already running interactively in the pane —
/// each byte arrives at the pty as if a user typed it, and `Enter` is sent as a final,
/// separate keystroke. Typing multi-line content this way is fragile and not verified safe:
/// delivery depends on the pane's pty mode and on the pane shell's startup timing, neither of
/// which this function controls. One specific, plausible-but-unconfirmed hazard is that a
/// freshly split pane's shell can still be sourcing rc files when the keys arrive — there is
/// a real multi-hundred-millisecond window between the pane appearing and its first
/// interactive prompt — and what a shell does with input arriving during that window is not
/// something this code has verified either way. Use `respawn_pane` instead for any command
/// that can contain a newline — it delivers the whole string as one argv element via
/// `$SHELL -c`, the same way `split_window`'s `shell_cmd` does, so embedded newlines survive
/// intact regardless of pty mode or shell startup timing.
///
/// Rejecting embedded newlines here (rather than trusting callers to remember) is deliberate:
/// the bug this guards against was a caller reusing `send_keys` for a multi-line script; the
/// exact mechanism by which that failed was never pinned down, but routing multi-line content
/// through `send_keys` is unsound regardless, so it is rejected outright rather than merely
/// discouraged.
pub fn send_keys(pane_target: &str, keys: &str) -> Result<()> {
    if keys.contains('\n') {
        return Err(AmError::TmuxError(format!(
            "send_keys keys must not contain embedded newlines (typing multi-line content \
             into the pane's live shell is fragile and not verified safe) — use \
             respawn_pane instead: {keys:?}"
        ))
        .into());
    }
    let bin = tmux_bin()?;
    run_tmux(&bin, &["send-keys", "-t", pane_target, keys, "Enter"])
}

/// `tmux respawn-pane -k -t <pane_target> <shell_cmd>`
///
/// Kills whatever is currently running in the pane and starts `shell_cmd` there via
/// `$SHELL -c` — the same delivery mechanism `split_window`'s `shell_cmd` parameter uses,
/// and the pane-reuse counterpart to it: `split_window` execs a command into a *freshly
/// created* pane, `respawn_pane` execs one into a pane that already exists. `shell_cmd`
/// arrives at tmux as a single argv element, so embedded newlines (e.g. a multi-line
/// `sh -c` probe script) survive intact regardless of pty mode or the pane shell's startup
/// timing — unlike `send_keys`, which types the string in keystroke by keystroke and is not
/// verified safe for multi-line content (see its doc comment).
pub fn respawn_pane(pane_target: &str, shell_cmd: &str) -> Result<()> {
    let bin = tmux_bin()?;
    run_tmux(&bin, &["respawn-pane", "-k", "-t", pane_target, shell_cmd])
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

/// Returns the pane target string `"<window_target>.<index>"`.
pub fn get_pane_id(window_target: &str, index: usize) -> String {
    format!("{window_target}.{index}")
}

/// The name of the foreground process currently running in `pane_target`
/// (`tmux display-message -p -t <target> '#{pane_current_command}'`).
///
/// Used by `am attach` to tell whether an agent (or, for a container session, the
/// container runtime) is still running in a pane before deciding whether to relaunch
/// anything into it — see the module doc on `cmd_attach` for the safety bias this feeds.
pub fn pane_current_command(pane_target: &str) -> Result<String> {
    let bin = tmux_bin()?;
    run_tmux_output(
        &bin,
        &[
            "display-message",
            "-p",
            "-t",
            pane_target,
            "#{pane_current_command}",
        ],
    )
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
    ///
    /// `new-window` returns a fixed window ID, matching real tmux's `-P -F '#{window_id}'`
    /// output. `display-message` (used by `pane_current_command`) echoes `$MOCK_TMUX_PANE_CMD`,
    /// defaulting to `bash`, so tests can simulate whatever the pane's foreground process is
    /// without a real tmux server.
    fn make_mock_tmux(dir: &Path) -> std::path::PathBuf {
        let script = dir.join("mock_tmux");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             echo \"$*\" >> \"$MOCK_TMUX_LOG\"\n\
             if [ \"$1\" = \"new-window\" ]; then echo '@1'; fi\n\
             if [ \"$1\" = \"display-message\" ]; then echo \"${MOCK_TMUX_PANE_CMD:-bash}\"; fi\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    /// Write a mock tmux script that always fails, to exercise error paths.
    fn make_failing_mock_tmux(dir: &Path) -> std::path::PathBuf {
        let script = dir.join("mock_tmux_fail");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"tmux: no such pane\" >&2\nexit 1\n",
        )
        .unwrap();
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
        assert_eq!(create_window("am-feat", Path::new("/tmp/worktree")).unwrap(), "@1");
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
            false,
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
            false,
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
            false,
            None,
        )
        .unwrap();
        let out = mock.captured();
        assert!(out.contains("-p"), "expected -p flag, got: {out}");
        assert!(out.contains("30"), "expected percent value 30, got: {out}");
    }

    #[test]
    fn split_window_before_passes_b_flag() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Horizontal,
            50,
            true,
            Some("podman run --rm -it myimage"),
        )
        .unwrap();
        let out = mock.captured();
        assert!(out.contains("-b"), "expected -b flag, got: {out}");
        assert!(
            out.contains("podman run --rm -it myimage"),
            "expected shell command alongside -b, got: {out}"
        );
    }

    #[test]
    fn split_window_omits_b_flag_when_not_before() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Horizontal,
            50,
            false,
            None,
        )
        .unwrap();
        let out = mock.captured();
        assert!(!out.contains("-b"), "expected no -b flag, got: {out}");
    }

    #[test]
    fn split_window_with_shell_cmd_appends_command() {
        let mock = MockTmux::new();
        split_window(
            "am-feat",
            Path::new("/tmp/worktree"),
            &SplitDirection::Horizontal,
            50,
            false,
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

    /// Regression guard for the `am attach` bug where a multi-line container launch script
    /// was typed into a live shell via `send_keys` instead of delivered with `respawn_pane`.
    /// The exact mechanism by which that produced the observed failure was never pinned down
    /// (see `send_keys`'s doc comment), but typing multi-line content into a pane's live
    /// shell is unsound regardless of mechanism, so `send_keys` must refuse it outright
    /// rather than risk silently corrupting the command.
    #[test]
    fn send_keys_rejects_embedded_newlines() {
        let _mock = MockTmux::new();
        let err = send_keys("am-feat.1", "sh -c 'first line\nsecond line'").unwrap_err();
        assert!(
            err.to_string().contains("newline"),
            "expected a newline-rejection error, got: {err}"
        );
    }

    #[test]
    fn respawn_pane_sends_correct_command() {
        let mock = MockTmux::new();
        respawn_pane("am-feat.1", "podman run --rm -it myimage").unwrap();
        let out = mock.captured();
        assert!(out.contains("respawn-pane"));
        assert!(out.contains("-k"));
        assert!(out.contains("-t"));
        assert!(out.contains("am-feat.1"));
        assert!(out.contains("podman run --rm -it myimage"));
    }

    /// The regression itself: a multi-line `sh -c '<script>'` command must reach tmux as one
    /// intact argv element, newlines preserved, rather than going through `send_keys`'s
    /// keystroke-by-keystroke delivery into a pane's live shell (which `send_keys` now
    /// refuses for exactly this reason — see its doc comment). `respawn_pane` hands it to
    /// `Command::args`, which never splits a `&str` on internal bytes regardless of content —
    /// this test pins that the whole script, newlines included, shows up verbatim in what
    /// tmux received.
    #[test]
    fn respawn_pane_preserves_embedded_newlines_as_one_argument() {
        let mock = MockTmux::new();
        let script = "sh -c 'export FOO=bar\nexec claude --resume'";
        respawn_pane("am-feat.1", script).unwrap();
        let out = mock.captured();
        assert!(
            out.contains(script),
            "expected the multi-line script intact in tmux's args, got: {out}"
        );
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

    // ── pane_current_command ─────────────────────────────────────────────────

    #[test]
    fn pane_current_command_sends_correct_command() {
        let mock = MockTmux::new();
        pane_current_command("am-feat.1").unwrap();
        let out = mock.captured();
        assert!(out.contains("display-message"));
        assert!(out.contains("-p"));
        assert!(out.contains("-t"));
        assert!(out.contains("am-feat.1"));
        assert!(out.contains("#{pane_current_command}"));
    }

    #[test]
    fn pane_current_command_returns_the_process_name() {
        let _mock = MockTmux::new();
        std::env::set_var("MOCK_TMUX_PANE_CMD", "claude");
        let result = pane_current_command("am-feat.1").unwrap();
        std::env::remove_var("MOCK_TMUX_PANE_CMD");
        assert_eq!(result, "claude");
    }

    #[test]
    fn pane_current_command_defaults_to_a_shell_in_the_mock() {
        let _mock = MockTmux::new();
        let result = pane_current_command("am-feat.1").unwrap();
        assert_eq!(result, "bash");
    }

    #[test]
    fn pane_current_command_propagates_tmux_failure() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let bin = make_failing_mock_tmux(tmp.path());
        std::env::set_var("AM_TMUX_BIN", &bin);

        let err = pane_current_command("am-gone.1").unwrap_err();
        std::env::remove_var("AM_TMUX_BIN");

        assert!(err.to_string().contains("no such pane"));
    }
}

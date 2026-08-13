use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, SystemTime};

use cucumber::{given, then, when, World};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const AM_BIN: &str = env!("CARGO_BIN_EXE_am");

/// Wall-clock cap on a single `run_am_pty` invocation. Real scenarios finish in well under a
/// second; this exists only to turn a hang into a fast, clearly-labeled test failure. The
/// mechanism is genuinely prone to hanging: `script` gives `am` a real pty, and a scripted
/// input line missing its trailing `\n` leaves the child blocked in canonical-mode `read()`
/// forever — it does not resolve on its own. See `wait_with_output_timeout` below and CI's own
/// `timeout-minutes` on the `test` job, which is the second line of defence.
const PTY_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Waits for `child` to exit, but no longer than `timeout` — unlike `Child::wait_with_output`,
/// which has no deadline. On timeout, kills the child by pid and panics with a message that
/// names the likely cause, so a hang fails *this one test* instead of blocking the suite (or,
/// without CI's own timeout, the whole 360-minute Actions default).
///
/// A dedicated crate (e.g. `wait-timeout`) would do this more directly, but pulling in a new
/// dev-dependency for one guard clause isn't worth it here: the wait itself has to run on a
/// separate thread anyway (this thread owns `child` and blocks on `recv_timeout` instead), and
/// once that's true, killing-by-pid is the only extra step needed — a handful of lines using
/// only what's already in scope (`std::process`, `std::sync::mpsc`, `std::thread`).
fn wait_with_output_timeout(child: Child, timeout: Duration) -> Output {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        // The receiver may already be gone (we already timed out and killed the child) by the
        // time this send happens — that's expected, not an error worth reporting.
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = handle.join();
            result.expect("wait for am (pty)")
        }
        Err(RecvTimeoutError::Timeout) => {
            // `child` itself was moved into the worker thread above, so pid + `kill` is the
            // only handle left on the process from this thread. The worker's own
            // `wait_with_output()` reaps it and returns immediately after, so joining here
            // is just to avoid leaking the thread, not to recover a result we still want.
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            let _ = handle.join();
            panic!(
                "am (pty) did not exit within {timeout:?} and was killed by the test \
                 harness's watchdog — likely a scripted input line missing its trailing \
                 newline, which wedges the child in canonical-mode read() forever"
            );
        }
        Err(RecvTimeoutError::Disconnected) => {
            panic!("pty-wait thread for am disconnected without sending a result")
        }
    }
}

// ── World ─────────────────────────────────────────────────────────────────────

/// Shared state threaded through every step in a scenario.
#[derive(Debug, World)]
pub struct AmWorld {
    /// Isolated temporary directory — serves as the project root.
    project_dir: TempDir,
    /// Isolated temporary directory for global state (XDG_STATE_HOME).
    state_dir: TempDir,
    /// Output from the most recent `am` invocation.
    last_output: Option<Output>,
    /// When Some, TMUX is mocked using this binary (+ log file).
    mock_tmux_bin: Option<PathBuf>,
    mock_tmux_log: Option<PathBuf>,
    /// When Some, the container runtime is mocked (+ log file).
    mock_podman_bin: Option<PathBuf>,
    mock_podman_log: Option<PathBuf>,
    /// Extra environment variables injected into the next `run_am` call.
    extra_env: Vec<(String, String)>,
    /// When Some, the `devcontainer` CLI is mocked (+ log file).
    mock_devcontainer_bin: Option<PathBuf>,
    mock_devcontainer_log: Option<PathBuf>,
    /// Touched by the mock CLI on a successful build; the mock runtime reports an image
    /// as present only once it exists, so "build" and "reuse" are distinguishable.
    mock_image_marker: Option<PathBuf>,
    /// The `devcontainer.metadata` label the mock runtime reports.
    mock_label_file: Option<PathBuf>,
    /// Isolated `$HOME` for `am setup` scenarios, so a run that writes
    /// `~/.config/am/config.toml` never touches the real developer's file. `None` means
    /// `HOME`/`XDG_CONFIG_HOME` are inherited unchanged, as every non-`setup` scenario needs.
    isolated_home: Option<TempDir>,
    /// mtime + full content snapshots taken by "I record the state of ..." steps, compared
    /// against by the matching "... is unchanged" steps.
    recorded_state: HashMap<&'static str, (SystemTime, String)>,
}

impl Default for AmWorld {
    fn default() -> Self {
        Self {
            project_dir: TempDir::new().expect("create temp dir"),
            state_dir: TempDir::new().expect("create state dir"),
            last_output: None,
            mock_tmux_bin: None,
            mock_tmux_log: None,
            mock_podman_bin: None,
            mock_podman_log: None,
            extra_env: Vec::new(),
            mock_devcontainer_bin: None,
            mock_devcontainer_log: None,
            mock_image_marker: None,
            mock_label_file: None,
            isolated_home: None,
            recorded_state: HashMap::new(),
        }
    }
}

impl AmWorld {
    fn project_path(&self) -> &Path {
        self.project_dir.path()
    }

    /// Path to the global sessions file: $state_dir/am/sessions.json
    fn global_sessions_path(&self) -> PathBuf {
        self.state_dir.path().join("am").join("sessions.json")
    }

    /// Set up an isolated `$HOME` for this scenario, creating it if it doesn't already
    /// exist. Idempotent so "Given an isolated home directory" can be combined freely with
    /// steps (like "a global config containing ...") that also need it.
    fn isolated_home_path(&mut self) -> PathBuf {
        if self.isolated_home.is_none() {
            self.isolated_home = Some(TempDir::new().expect("create isolated home dir"));
        }
        self.isolated_home.as_ref().unwrap().path().to_path_buf()
    }

    /// `$HOME/.config/am/config.toml` under the isolated home — where `am setup` reads and
    /// writes the global config when `XDG_CONFIG_HOME` is unset, which is `config::
    /// global_config_path`'s documented fallback.
    fn global_config_path(&mut self) -> PathBuf {
        self.isolated_home_path()
            .join(".config")
            .join("am")
            .join("config.toml")
    }

    /// Install a mock tmux binary that logs every invocation to a file.
    fn setup_mock_tmux(&mut self) {
        let bin = self.project_dir.path().join("mock_tmux");
        let log = self.project_dir.path().join("mock_tmux.log");
        fs::write(
            &bin,
            "#!/bin/sh\n\
             if [ -n \"$MOCK_TMUX_LOG\" ]; then\n\
                 echo \"$@\" >> \"$MOCK_TMUX_LOG\"\n\
             fi\n\
             if [ \"$1\" = \"new-window\" ]; then\n\
                 echo \"@1\"\n\
             fi\n\
             exit 0\n",
        )
        .expect("write mock_tmux");
        #[cfg(unix)]
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod mock_tmux");
        self.mock_tmux_bin = Some(bin);
        self.mock_tmux_log = Some(log);
    }

    /// Install a mock tmux that fails the **first** `select-window` call and
    /// succeeds on all subsequent calls. Used to exercise the window
    /// re-creation path in `am attach`.
    fn setup_mock_tmux_failing_select_window(&mut self) {
        let bin = self.project_dir.path().join("mock_tmux");
        let log = self.project_dir.path().join("mock_tmux.log");
        // A flag file is created on the first select-window call; its presence
        // makes subsequent select-window calls succeed.
        let flag = self
            .project_dir
            .path()
            .join("mock_tmux_select_window_failed");
        let flag_str = flag.to_string_lossy().into_owned();
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\n\
                 if [ -n \"$MOCK_TMUX_LOG\" ]; then\n\
                     echo \"$@\" >> \"$MOCK_TMUX_LOG\"\n\
                 fi\n\
                 if [ \"$1\" = \"new-window\" ]; then\n\
                     echo \"@1\"\n\
                 fi\n\
                 if [ \"$1\" = \"select-window\" ] && [ ! -f \"{flag_str}\" ]; then\n\
                     touch \"{flag_str}\"\n\
                     exit 1\n\
                 fi\n\
                 exit 0\n"
            ),
        )
        .expect("write mock_tmux");
        #[cfg(unix)]
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod mock_tmux");
        self.mock_tmux_bin = Some(bin);
        self.mock_tmux_log = Some(log);
    }

    /// Install a mock podman binary that logs every invocation to a file.
    fn setup_mock_podman(&mut self) {
        let bin = self.project_dir.path().join("mock_podman");
        let log = self.project_dir.path().join("mock_podman.log");
        fs::write(
            &bin,
            "#!/bin/sh\n\
             if [ -n \"$MOCK_PODMAN_LOG\" ]; then\n\
                 echo \"$@\" >> \"$MOCK_PODMAN_LOG\"\n\
             fi\n\
             exit 0\n",
        )
        .expect("write mock_podman");
        #[cfg(unix)]
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod mock_podman");
        self.mock_podman_bin = Some(bin);
        self.mock_podman_log = Some(log);
    }

    /// Install a mock container runtime that understands the two devcontainer queries:
    /// "does this image exist" and "what is its metadata label".
    ///
    /// The plain mock always exits 0, which would make every image look present and hide
    /// the build step entirely.
    fn setup_mock_devcontainer_runtime(&mut self) {
        let bin = self.project_dir.path().join("mock_podman_dc");
        let log = self.project_dir.path().join("mock_podman.log");
        let marker = self.project_dir.path().join("image-built.marker");
        let label = self.project_dir.path().join("metadata-label.json");
        fs::write(
            &label,
            r#"[{"entrypoint":"/usr/local/share/init.sh","remoteUser":"vscode"}]"#,
        )
        .expect("write label file");
        fs::write(
            &bin,
            "#!/bin/sh\n\
             if [ -n \"$MOCK_PODMAN_LOG\" ]; then\n\
                 echo \"$@\" >> \"$MOCK_PODMAN_LOG\"\n\
             fi\n\
             if [ \"$1\" = \"image\" ] && [ \"$2\" = \"inspect\" ]; then\n\
                 [ -f \"$MOCK_IMAGE_MARKER\" ] && exit 0\n\
                 exit 1\n\
             fi\n\
             if [ \"$1\" = \"inspect\" ]; then\n\
                 cat \"$MOCK_LABEL_FILE\" 2>/dev/null || echo '<no value>'\n\
                 exit 0\n\
             fi\n\
             exit 0\n",
        )
        .expect("write mock podman");
        #[cfg(unix)]
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod mock podman");
        self.mock_podman_bin = Some(bin);
        self.mock_podman_log = Some(log);
        self.mock_image_marker = Some(marker);
        self.mock_label_file = Some(label);
    }

    /// Install a mock `devcontainer` CLI. `succeed = false` reproduces the real failure
    /// shape: exit 1 with an error JSON on stdout.
    fn setup_mock_devcontainer_cli(&mut self, succeed: bool) {
        let bin = self.project_dir.path().join("mock_devcontainer");
        let log = self.project_dir.path().join("mock_devcontainer.log");
        let body = if succeed {
            "touch \"$MOCK_IMAGE_MARKER\"\n\
             echo '{\"outcome\":\"success\"}'\n\
             exit 0\n"
        } else {
            "echo '{\"outcome\":\"error\",\"message\":\"Command failed: podman pull nope\",\
             \"description\":\"An error occurred building the container.\"}'\n\
             exit 1\n"
        };
        fs::write(
            &bin,
            format!(
                // --version is answered before anything else and is not logged: `am
                // doctor` probes it, and treating a probe as a build would both create
                // the built-image marker and inflate the call count.
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then\n\
                     echo '0.88.0'\n\
                     exit 0\n\
                 fi\n\
                 if [ -n \"$MOCK_DEVCONTAINER_LOG\" ]; then\n\
                     echo \"$@\" >> \"$MOCK_DEVCONTAINER_LOG\"\n\
                 fi\n{body}"
            ),
        )
        .expect("write mock devcontainer");
        #[cfg(unix)]
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))
            .expect("chmod mock devcontainer");
        self.mock_devcontainer_bin = Some(bin);
        self.mock_devcontainer_log = Some(log);
    }

    /// Write a devcontainer config and commit it, so it appears in session worktrees.
    ///
    /// Committing matters: `git worktree add` checks out a branch, so an uncommitted
    /// config would simply not exist in the worktree am discovers against.
    fn write_devcontainer_config(&mut self, body: &str) {
        let dir = self.project_path().join(".devcontainer");
        fs::create_dir_all(&dir).expect("create .devcontainer");
        fs::write(dir.join("devcontainer.json"), body).expect("write devcontainer.json");
        let project = self.project_path().to_path_buf();
        run_git(&["add", "-A"], &project);
        run_git(
            &[
                "commit",
                "--no-verify",
                "-m",
                "chore: add devcontainer config",
            ],
            &project,
        );
    }

    /// Run the `am` binary with `args`, capturing stdout + stderr.
    fn run_am(&mut self, args: &[&str]) {
        let dir = self.project_path().to_path_buf();
        let mut cmd = Command::new(AM_BIN);
        cmd.args(args)
            .current_dir(&dir)
            .env("XDG_STATE_HOME", self.state_dir.path());

        // Isolate HOME (and clear any inherited XDG_CONFIG_HOME) so a scenario that writes
        // ~/.config/am/config.toml — namely `am setup` — never touches the real one.
        if let Some(ref home) = self.isolated_home {
            cmd.env("HOME", home.path()).env_remove("XDG_CONFIG_HOME");
        }

        // Container setup: use mock podman when available; disable otherwise.
        if let Some(ref podman_bin) = self.mock_podman_bin.clone() {
            cmd.env("AM_CONTAINER_ENABLED", "true")
                .env("AM_PODMAN_BIN", podman_bin)
                // A real image is not needed — the run command is sent to
                // mock tmux as keystrokes, never executed.
                .env("AM_CONTAINER_IMAGE", "test-image:latest");
            if let Some(ref log) = self.mock_podman_log.clone() {
                cmd.env("MOCK_PODMAN_LOG", log);
            }
        } else {
            cmd.env("AM_CONTAINER_ENABLED", "false");
        }

        // Devcontainer mocks, when the scenario set them up.
        if let Some(ref bin) = self.mock_devcontainer_bin.clone() {
            cmd.env("AM_DEVCONTAINER_BIN", bin);
            if let Some(ref log) = self.mock_devcontainer_log.clone() {
                cmd.env("MOCK_DEVCONTAINER_LOG", log);
            }
        }
        if let Some(ref marker) = self.mock_image_marker.clone() {
            cmd.env("MOCK_IMAGE_MARKER", marker);
        }
        if let Some(ref label) = self.mock_label_file.clone() {
            cmd.env("MOCK_LABEL_FILE", label);
        }

        // Tmux setup: mock when available; simulate no-tmux otherwise.
        if let Some(ref tmux_bin) = self.mock_tmux_bin.clone() {
            cmd.env("TMUX", "mock-session,0,0")
                .env("AM_TMUX_BIN", tmux_bin);
            if let Some(ref log) = self.mock_tmux_log.clone() {
                cmd.env("MOCK_TMUX_LOG", log);
            }
        } else {
            cmd.env_remove("TMUX");
        }

        for (k, v) in &self.extra_env.clone() {
            cmd.env(k, v);
        }
        self.extra_env.clear();

        let output = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn am: {e}"));
        self.last_output = Some(output);
    }

    /// Run `am` with a *real* pseudo-terminal wired to stdin, via the `script` utility —
    /// this is what makes `std::io::IsTerminal::is_terminal()` return `true` inside the
    /// child process, the same gate `cmd_setup` uses to decide whether to run interactively
    /// at all. `run_am_with_input`'s plain pipe cannot exercise this: a pipe is never a TTY,
    /// so every scenario driving it takes the `--yes` path and never reaches the interactive
    /// prompts (the layout menu, the customize sub-flow, or the write-target lines, which
    /// print only inside the interactive branch).
    ///
    /// `input` is fed to `script`'s own stdin, which the pty line discipline both delivers to
    /// the child *and* echoes back into the captured output. That echo lands wherever the
    /// kernel happens to flush it — observed in practice to be up front, before the child's
    /// own output — so callers must only ever assert with substring `contains`/`does not
    /// contain` checks, never on ordering or exact-position of the child's own prompt text
    /// relative to the echoed input.
    ///
    /// Every line of `input` must end in `\n`: the pty's canonical mode buffers a partial
    /// line until it sees a newline (or an explicit EOF character), so a redirected file or
    /// pipe that runs out of bytes without one leaves the child blocked forever rather than
    /// reading a short line at EOF the way a plain pipe would.
    fn run_am_pty(&mut self, args: &[&str], input: &str) {
        let dir = self.project_path().to_path_buf();
        // `script -c` runs its command through `sh -c`, so each arg needs shell quoting —
        // single-quoted, with embedded single quotes escaped the POSIX way.
        let quoted_args = args
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let command_line = format!("{AM_BIN} {quoted_args}");

        let mut cmd = Command::new("script");
        cmd.args(["-qec", &command_line, "/dev/null"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("XDG_STATE_HOME", self.state_dir.path());

        if let Some(ref home) = self.isolated_home {
            cmd.env("HOME", home.path()).env_remove("XDG_CONFIG_HOME");
        }

        if let Some(ref podman_bin) = self.mock_podman_bin.clone() {
            cmd.env("AM_CONTAINER_ENABLED", "true")
                .env("AM_PODMAN_BIN", podman_bin)
                .env("AM_CONTAINER_IMAGE", "test-image:latest");
            if let Some(ref log) = self.mock_podman_log.clone() {
                cmd.env("MOCK_PODMAN_LOG", log);
            }
        } else {
            cmd.env("AM_CONTAINER_ENABLED", "false");
        }

        if let Some(ref tmux_bin) = self.mock_tmux_bin.clone() {
            cmd.env("TMUX", "mock-session,0,0")
                .env("AM_TMUX_BIN", tmux_bin);
            if let Some(ref log) = self.mock_tmux_log.clone() {
                cmd.env("MOCK_TMUX_LOG", log);
            }
        } else {
            cmd.env_remove("TMUX");
        }

        for (k, v) in &self.extra_env.clone() {
            cmd.env(k, v);
        }
        self.extra_env.clear();

        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn 'script' (pty test harness): {e}"));

        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .expect("write to pty stdin");

        self.last_output = Some(wait_with_output_timeout(child, PTY_WAIT_TIMEOUT));
    }

    /// Like `run_am` but writes `input` to the process's stdin before waiting.
    fn run_am_with_input(&mut self, args: &[&str], input: &str) {
        let dir = self.project_path().to_path_buf();
        let mut cmd = Command::new(AM_BIN);
        cmd.args(args)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("XDG_STATE_HOME", self.state_dir.path())
            .env("AM_CONTAINER_ENABLED", "false");

        if let Some(ref home) = self.isolated_home {
            cmd.env("HOME", home.path()).env_remove("XDG_CONFIG_HOME");
        }

        if let Some(ref tmux_bin) = self.mock_tmux_bin.clone() {
            cmd.env("TMUX", "mock-session,0,0")
                .env("AM_TMUX_BIN", tmux_bin);
            if let Some(ref log) = self.mock_tmux_log.clone() {
                cmd.env("MOCK_TMUX_LOG", log);
            }
        } else {
            cmd.env_remove("TMUX");
        }

        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn am: {e}"));

        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .expect("write to stdin");

        self.last_output = Some(child.wait_with_output().expect("wait for am"));
    }

    fn last_output(&self) -> &Output {
        self.last_output
            .as_ref()
            .expect("no command has been run yet")
    }

    /// stdout + stderr concatenated for assertion convenience.
    fn combined_output(&self) -> String {
        let o = self.last_output();
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr),
        )
    }
}

// ── Given ─────────────────────────────────────────────────────────────────────

#[given("a git repository")]
async fn given_git_repo(world: &mut AmWorld) {
    let dir = world.project_path().to_path_buf();
    run_git(&["init"], &dir);
    run_git(&["config", "user.email", "test@example.com"], &dir);
    run_git(&["config", "user.name", "Test"], &dir);
    // git's auto-gc detaches into the background and its invoker does not wait for
    // it, so without an init to reap the orphan every scenario leaks a zombie.
    run_git(&["config", "gc.auto", "0"], &dir);
    // An initial commit is required so that `am start` can resolve HEAD.
    run_git(
        &["commit", "--allow-empty", "-m", "chore: initial commit"],
        &dir,
    );
}

#[given("a jj repository")]
async fn given_jj_repo(world: &mut AmWorld) {
    let dir = world.project_path().to_path_buf();
    let status = Command::new("jj")
        .args(["git", "init"])
        .current_dir(&dir)
        .status()
        .expect("failed to spawn jj");
    assert!(status.success(), "jj git init failed in {dir:?}");
}

#[given("no git repository")]
async fn given_no_git_repo(_world: &mut AmWorld) {
    // Default state — the temp dir has no .git. Step exists for readability.
}

#[given("am init has been run")]
async fn given_am_init(world: &mut AmWorld) {
    world.run_am(&["init"]);
    let output = world.last_output();
    assert!(
        output.status.success(),
        "setup: 'am init' failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[given(expr = "a session {string} has been started")]
async fn given_session_started(world: &mut AmWorld, slug: String) {
    world.run_am(&["start", &slug]);
    let output = world.last_output();
    assert!(
        output.status.success(),
        "setup step 'am start {slug}' failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[given("I am inside a tmux session")]
async fn given_in_tmux(world: &mut AmWorld) {
    world.setup_mock_tmux();
}

#[given("the tmux window no longer exists")]
async fn given_tmux_window_gone(world: &mut AmWorld) {
    world.setup_mock_tmux_failing_select_window();
}

#[given("I am using a mock container runtime")]
async fn given_mock_container(world: &mut AmWorld) {
    world.setup_mock_podman();
}

// ── `am setup` — isolated HOME / global config ───────────────────────────────

#[given("an isolated home directory")]
async fn given_isolated_home(world: &mut AmWorld) {
    world.isolated_home_path();
}

#[given(expr = "a global config containing {string}")]
async fn given_global_config(world: &mut AmWorld, body: String) {
    let path = world.global_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // Same unescaping as "a project config containing ...": Gherkin hands the string over
    // with its escapes intact.
    let content = body.replace("\\n", "\n").replace("\\\"", "\"");
    fs::write(&path, content).unwrap();
}

/// Satisfies `validate_agent_credentials` for claude: it only checks that
/// `$HOME/.claude` exists, never what's in it.
#[given("claude credentials are present")]
async fn given_claude_credentials(world: &mut AmWorld) {
    let home = world.isolated_home_path();
    fs::create_dir_all(home.join(".claude")).unwrap();
}

#[given("I record the state of the project config file")]
async fn given_record_project_config(world: &mut AmWorld) {
    let path = world.project_path().join(".am").join("config.toml");
    world.recorded_state.insert("project", snapshot(&path));
}

#[given("I record the state of the global config file")]
async fn given_record_global_config(world: &mut AmWorld) {
    let path = world.global_config_path();
    world.recorded_state.insert("global", snapshot(&path));
}

// ── When ──────────────────────────────────────────────────────────────────────

#[given("I am using a mock devcontainer CLI")]
async fn given_mock_devcontainer(world: &mut AmWorld) {
    world.setup_mock_devcontainer_runtime();
    world.setup_mock_devcontainer_cli(true);
}

#[given("I am using a mock devcontainer CLI that fails")]
async fn given_failing_devcontainer(world: &mut AmWorld) {
    world.setup_mock_devcontainer_runtime();
    world.setup_mock_devcontainer_cli(false);
}

#[given("the repo has a devcontainer config")]
async fn given_devcontainer_config(world: &mut AmWorld) {
    world.write_devcontainer_config(
        "{\n  // JSONC is normal in configs written for editors\n  \
         \"name\": \"test\",\n  \"image\": \"debian:bookworm\",\n}",
    );
}

#[given("the repo has a devcontainer config using docker compose")]
async fn given_compose_config(world: &mut AmWorld) {
    world.write_devcontainer_config(
        "{\"name\":\"test\",\"dockerComposeFile\":\"docker-compose.yml\",\"service\":\"app\"}",
    );
}

#[given("the repo has a devcontainer config with an initializeCommand")]
async fn given_initialize_command_config(world: &mut AmWorld) {
    world.write_devcontainer_config(
        "{\"name\":\"test\",\"image\":\"debian:bookworm\",\"initializeCommand\":\"./setup.sh\"}",
    );
}

#[when(expr = "I run {string}")]
async fn when_run(world: &mut AmWorld, cmd: String) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    // Strip leading "am" so feature files can be written naturally.
    let args: &[&str] = match parts.first() {
        Some(&"am") => &parts[1..],
        _ => &parts[..],
    };
    world.run_am(args);
}

#[given(expr = "the file {string} contains {string} without a trailing newline")]
async fn given_file_without_trailing_newline(
    world: &mut AmWorld,
    rel_path: String,
    content: String,
) {
    let path = world.project_path().join(&rel_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, &content).unwrap();
}

#[given(expr = "a project config containing {string}")]
async fn given_project_config(world: &mut AmWorld, body: String) {
    let dir = world.project_path().join(".am");
    fs::create_dir_all(&dir).unwrap();
    // Gherkin hands the string over with its escapes intact: newlines arrive as a
    // literal backslash-n, and the quotes TOML needs as backslash-quote. Both have to
    // be unescaped or the file is written with backslashes in it and fails to parse.
    let content = body.replace("\\n", "\n").replace("\\\"", "\"");
    fs::write(dir.join("config.toml"), content).unwrap();
}

#[given(expr = "I have set env {string} to {string}")]
async fn given_env_var(world: &mut AmWorld, key: String, value: String) {
    world.extra_env.push((key, value));
}

#[when(expr = "I run {string} with env {string} = {string}")]
async fn when_run_with_env(world: &mut AmWorld, cmd: String, key: String, value: String) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let args: &[&str] = match parts.first() {
        Some(&"am") => &parts[1..],
        _ => &parts[..],
    };
    world.extra_env.push((key, value));
    world.run_am(args);
}

#[when(expr = "I run {string} with input {string}")]
async fn when_run_with_input(world: &mut AmWorld, cmd: String, input: String) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let args: &[&str] = match parts.first() {
        Some(&"am") => &parts[1..],
        _ => &parts[..],
    };
    world.run_am_with_input(args, &format!("{input}\n"));
}

/// Drives `am` through a real pty (see `AmWorld::run_am_pty`) so the genuinely interactive
/// prompts — the ones gated on `stdin().is_terminal()` — actually run. `input`'s `\n` tokens
/// are unescaped the same way `given_project_config`'s are: Gherkin hands the string over
/// with literal backslash-n, one answer per prompt. A trailing newline is always appended so
/// the pty's canonical mode never blocks on an unterminated final line.
#[when(expr = "I run {string} through a pty with input {string}")]
async fn when_run_pty(world: &mut AmWorld, cmd: String, input: String) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let args: &[&str] = match parts.first() {
        Some(&"am") => &parts[1..],
        _ => &parts[..],
    };
    let unescaped = input.replace("\\n", "\n");
    world.run_am_pty(args, &format!("{unescaped}\n"));
}

// ── Then ──────────────────────────────────────────────────────────────────────

#[then("the command succeeds")]
async fn then_succeeds(world: &mut AmWorld) {
    let output = world.last_output();
    assert!(
        output.status.success(),
        "expected success\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[then("the command fails")]
async fn then_fails(world: &mut AmWorld) {
    let output = world.last_output();
    assert!(
        !output.status.success(),
        "expected failure but command exited {}",
        output.status,
    );
}

#[then(expr = "the output contains {string}")]
async fn then_output_contains(world: &mut AmWorld, text: String) {
    let combined = world.combined_output();
    assert!(
        combined.contains(&text),
        "expected output to contain {text:?}\ngot:\n{combined}",
    );
}

#[then(expr = "the output does not contain {string}")]
async fn then_output_not_contains(world: &mut AmWorld, text: String) {
    let combined = world.combined_output();
    assert!(
        !combined.contains(&text),
        "expected output to NOT contain {text:?}\ngot:\n{combined}",
    );
}

#[then(expr = "a worktree exists at {string}")]
async fn then_worktree_exists(world: &mut AmWorld, rel_path: String) {
    let path = world.project_path().join(&rel_path);
    assert!(path.exists(), "expected worktree at {path:?} to exist");
}

#[then(expr = "the worktree {string} does not exist")]
async fn then_worktree_gone(world: &mut AmWorld, rel_path: String) {
    let path = world.project_path().join(&rel_path);
    assert!(
        !path.exists(),
        "expected {path:?} to not exist, but it does"
    );
}

#[then(expr = "the session file contains {string}")]
async fn then_session_contains(world: &mut AmWorld, slug: String) {
    let sessions_path = world.global_sessions_path();
    let content = fs::read_to_string(&sessions_path)
        .unwrap_or_else(|_| panic!("sessions.json not found at {sessions_path:?}"));
    assert!(
        content.contains(&slug),
        "expected sessions.json to contain {slug:?}\ngot:\n{content}",
    );
}

#[then(expr = "the session file does not contain {string}")]
async fn then_session_not_contain(world: &mut AmWorld, slug: String) {
    let sessions_path = world.global_sessions_path();
    if !sessions_path.exists() {
        return; // no file → no session → assertion trivially satisfied
    }
    let content = fs::read_to_string(&sessions_path).expect("read sessions.json");
    assert!(
        !content.contains(&slug),
        "expected sessions.json to NOT contain {slug:?}\ngot:\n{content}",
    );
}

#[then(expr = "the file {string} exists")]
async fn then_file_exists(world: &mut AmWorld, rel_path: String) {
    let path = world.project_path().join(&rel_path);
    assert!(path.exists(), "expected file at {path:?} to exist");
}

#[then(expr = "the file {string} does not exist")]
async fn then_file_absent(world: &mut AmWorld, rel_path: String) {
    let path = world.project_path().join(&rel_path);
    assert!(!path.exists(), "expected no file at {path:?}");
}

#[then(expr = "the file {string} contains {string}")]
async fn then_file_contains(world: &mut AmWorld, rel_path: String, text: String) {
    let path = world.project_path().join(&rel_path);
    let content = fs::read_to_string(&path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    assert!(
        content.contains(&text),
        "expected {path:?} to contain {text:?}\ngot:\n{content}",
    );
}

#[then(expr = "the file {string} does not contain {string}")]
async fn then_file_does_not_contain(world: &mut AmWorld, rel_path: String, text: String) {
    let path = world.project_path().join(&rel_path);
    let content = fs::read_to_string(&path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    assert!(
        !content.contains(&text),
        "expected {path:?} to NOT contain {text:?}\ngot:\n{content}",
    );
}

// ── `am setup` — global config assertions ────────────────────────────────────

#[then("the global config file exists")]
async fn then_global_config_exists(world: &mut AmWorld) {
    let path = world.global_config_path();
    assert!(path.exists(), "expected global config at {path:?} to exist");
}

#[then("the global config file does not exist")]
async fn then_global_config_absent(world: &mut AmWorld) {
    let path = world.global_config_path();
    assert!(!path.exists(), "expected no global config at {path:?}");
}

#[then(expr = "the global config file contains {string}")]
async fn then_global_config_contains(world: &mut AmWorld, text: String) {
    let path = world.global_config_path();
    let content = fs::read_to_string(&path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    assert!(
        content.contains(&text),
        "expected {path:?} to contain {text:?}\ngot:\n{content}",
    );
}

/// Asserts table/line order was preserved by an in-place edit — `update_key` (`toml_edit`)
/// promises never to reorder existing content, only to touch the one key it was asked to
/// change, and this is the direct way to pin that promise instead of inferring it from
/// content-only `contains` checks.
#[then(expr = "the global config file has {string} before {string}")]
async fn then_global_config_order(world: &mut AmWorld, first: String, second: String) {
    let path = world.global_config_path();
    let content = fs::read_to_string(&path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    let first_pos = content
        .find(&first)
        .unwrap_or_else(|| panic!("{first:?} not found in {path:?}\n{content}"));
    let second_pos = content
        .find(&second)
        .unwrap_or_else(|| panic!("{second:?} not found in {path:?}\n{content}"));
    assert!(
        first_pos < second_pos,
        "expected {first:?} to appear before {second:?} in {path:?}\n{content}",
    );
}

#[then(expr = "the global config file does not contain {string}")]
async fn then_global_config_not_contain(world: &mut AmWorld, text: String) {
    let path = world.global_config_path();
    let content = fs::read_to_string(&path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    assert!(
        !content.contains(&text),
        "expected {path:?} to NOT contain {text:?}\ngot:\n{content}",
    );
}

#[then("the project config file is unchanged")]
async fn then_project_config_unchanged(world: &mut AmWorld) {
    let path = world.project_path().join(".am").join("config.toml");
    assert_unchanged(&path, world.recorded_state.get("project"));
}

#[then("the global config file is unchanged")]
async fn then_global_config_unchanged(world: &mut AmWorld) {
    let path = world.global_config_path();
    let recorded = world.recorded_state.get("global").cloned();
    assert_unchanged(&path, recorded.as_ref());
}

#[then(expr = "the project config sets {string} to {string}")]
async fn then_project_config_key(world: &mut AmWorld, key_path: String, expected: String) {
    let path = world.project_path().join(".am").join("config.toml");
    assert_toml_key(&path, &key_path, &expected);
}

/// The absence counterpart to "the project config sets ..." — see `then_global_config_key_
/// unset` for why this has to be parsed rather than string-matched.
#[then(expr = "the project config does not set {string}")]
async fn then_project_config_key_unset(world: &mut AmWorld, key_path: String) {
    let path = world.project_path().join(".am").join("config.toml");
    assert_toml_key_absent(&path, &key_path);
}

#[then(expr = "the global config sets {string} to {string}")]
async fn then_global_config_key(world: &mut AmWorld, key_path: String, expected: String) {
    let path = world.global_config_path();
    assert_toml_key(&path, &key_path, &expected);
}

/// The absence counterpart to "the global config sets ...": parsed, not string-matched, so a
/// commented-out example (`# agent_pane = "left"`) — which a substring search cannot
/// distinguish from a live key — correctly reads as "unset".
#[then(expr = "the global config does not set {string}")]
async fn then_global_config_key_unset(world: &mut AmWorld, key_path: String) {
    let path = world.global_config_path();
    assert_toml_key_absent(&path, &key_path);
}

#[then(expr = "the mock tmux log contains {string}")]
async fn then_tmux_log_contains(world: &mut AmWorld, text: String) {
    let log = world
        .mock_tmux_log
        .as_ref()
        .expect("mock tmux was not set up for this scenario");
    let content = fs::read_to_string(log).unwrap_or_default();
    assert!(
        content.contains(&text),
        "expected tmux log to contain {text:?}\ngot:\n{content}",
    );
}

#[then(expr = "the mock tmux log does not contain {string}")]
async fn then_tmux_log_does_not_contain(world: &mut AmWorld, text: String) {
    let log = world
        .mock_tmux_log
        .as_ref()
        .expect("mock tmux was not set up for this scenario");
    let content = fs::read_to_string(log).unwrap_or_default();
    assert!(
        !content.contains(&text),
        "expected tmux log to NOT contain {text:?}\ngot:\n{content}",
    );
}

#[then(expr = "the mock podman log contains {string}")]
async fn then_podman_log_contains(world: &mut AmWorld, text: String) {
    let log = world
        .mock_podman_log
        .as_ref()
        .expect("mock container runtime was not set up for this scenario");
    let content = fs::read_to_string(log).unwrap_or_default();
    assert!(
        content.contains(&text),
        "expected podman log to contain {text:?}\ngot:\n{content}",
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[then(expr = "the mock devcontainer log contains {string}")]
async fn then_devcontainer_log_contains(world: &mut AmWorld, text: String) {
    let log = world
        .mock_devcontainer_log
        .as_ref()
        .expect("mock devcontainer CLI was not set up for this scenario");
    let contents = fs::read_to_string(log).unwrap_or_default();
    assert!(
        contents.contains(&text),
        "devcontainer log missing {text:?}\n--- log ---\n{contents}"
    );
}

#[then(expr = "the mock devcontainer CLI was called {int} time(s)")]
async fn then_devcontainer_called_times(world: &mut AmWorld, expected: usize) {
    let log = world
        .mock_devcontainer_log
        .as_ref()
        .expect("mock devcontainer CLI was not set up for this scenario");
    let contents = fs::read_to_string(log).unwrap_or_default();
    let calls = contents.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        calls, expected,
        "expected {expected} CLI call(s), got {calls}\n--- log ---\n{contents}"
    );
}

/// Read a file's mtime and full content together, so "is unchanged" can assert both at
/// once — the no-op contract `am setup`'s `update_*` functions promise is "not touched at
/// all", not merely "produces the same bytes".
fn snapshot(path: &Path) -> (SystemTime, String) {
    let meta = fs::metadata(path).unwrap_or_else(|e| panic!("could not stat {path:?}: {e}"));
    let mtime = meta.modified().unwrap_or_else(|e| {
        panic!("platform does not support mtime, needed to check {path:?}: {e}")
    });
    let content = fs::read_to_string(path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    (mtime, content)
}

fn assert_unchanged(path: &Path, recorded: Option<&(SystemTime, String)>) {
    let (recorded_mtime, recorded_content) = recorded.unwrap_or_else(|| {
        panic!(
            "no state was recorded for {path:?} — add a matching 'I record the state of ...' step"
        )
    });
    let (mtime, content) = snapshot(path);
    assert_eq!(
        &content, recorded_content,
        "expected {path:?} content to be unchanged"
    );
    assert_eq!(
        &mtime, recorded_mtime,
        "expected {path:?} mtime to be unchanged, but the file was rewritten"
    );
}

/// Assert a dotted key path (e.g. "defaults.agent") in a TOML file equals `expected`,
/// parsed rather than string-matched — a substring check can't tell a live
/// `agent = "claude"` line apart from the commented example the skeleton ships with.
fn assert_toml_key(path: &Path, key_path: &str, expected: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    // `Value`'s `FromStr` parses a single bare value expression, not a document — a
    // document (with `[section]` headers and top-level comments) needs `from_str`'s
    // document-level deserializer instead.
    let value: toml::Value = toml::from_str(&content)
        .unwrap_or_else(|e| panic!("invalid TOML in {path:?}: {e}\n{content}"));
    let mut current = &value;
    for part in key_path.split('.') {
        current = current
            .get(part)
            .unwrap_or_else(|| panic!("key {key_path:?} not found in {path:?}\n{content}"));
    }
    let actual = match current {
        toml::Value::String(s) => s.clone(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    };
    assert_eq!(
        actual, expected,
        "expected {key_path:?} in {path:?} to be {expected:?}, got {actual:?}\nfile:\n{content}",
    );
}

/// Assert a dotted key path is absent from a TOML file — parsed the same way as
/// `assert_toml_key`, for the same reason: a substring search can't tell a commented-out
/// example apart from a live key that happens to hold the same text.
fn assert_toml_key_absent(path: &Path, key_path: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    let value: toml::Value = toml::from_str(&content)
        .unwrap_or_else(|e| panic!("invalid TOML in {path:?}: {e}\n{content}"));
    let mut current = Some(&value);
    for part in key_path.split('.') {
        current = current.and_then(|v| v.get(part));
    }
    assert!(
        current.is_none(),
        "expected {key_path:?} to be unset in {path:?}, found {current:?}\nfile:\n{content}",
    );
}

fn run_git(args: &[&str], dir: &PathBuf) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git: {e}"));
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

// ── Runner ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Run scenarios sequentially: each scenario spawns subprocesses that
    // block tokio threads, so parallel execution causes deadlocks.
    AmWorld::cucumber()
        .max_concurrent_scenarios(1)
        .run_and_exit("tests/features")
        .await;
}

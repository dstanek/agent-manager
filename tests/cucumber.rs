use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cucumber::{given, then, when, World};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const AM_BIN: &str = env!("CARGO_BIN_EXE_am");

// ── World ─────────────────────────────────────────────────────────────────────

/// Shared state threaded through every step in a scenario.
#[derive(Debug, World)]
pub struct AmWorld {
    /// Isolated temporary directory — serves as the project root.
    project_dir: TempDir,
    /// Output from the most recent `am` invocation.
    last_output: Option<Output>,
    /// When Some, TMUX is mocked using this binary (+ log file).
    mock_tmux_bin: Option<PathBuf>,
    mock_tmux_log: Option<PathBuf>,
    /// When Some, the container runtime is mocked (+ log file).
    mock_podman_bin: Option<PathBuf>,
    mock_podman_log: Option<PathBuf>,
}

impl Default for AmWorld {
    fn default() -> Self {
        Self {
            project_dir: TempDir::new().expect("create temp dir"),
            last_output: None,
            mock_tmux_bin: None,
            mock_tmux_log: None,
            mock_podman_bin: None,
            mock_podman_log: None,
        }
    }
}

impl AmWorld {
    fn project_path(&self) -> &Path {
        self.project_dir.path()
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

    /// Run the `am` binary with `args`, capturing stdout + stderr.
    fn run_am(&mut self, args: &[&str]) {
        let dir = self.project_path().to_path_buf();
        let mut cmd = Command::new(AM_BIN);
        cmd.args(args).current_dir(&dir);

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

        let output = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn am: {e}"));
        self.last_output = Some(output);
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
            .env("AM_CONTAINER_ENABLED", "false");

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

// ── When ──────────────────────────────────────────────────────────────────────

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

#[when(expr = "I run {string} with input {string}")]
async fn when_run_with_input(world: &mut AmWorld, cmd: String, input: String) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let args: &[&str] = match parts.first() {
        Some(&"am") => &parts[1..],
        _ => &parts[..],
    };
    world.run_am_with_input(args, &format!("{input}\n"));
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
    let sessions_path = world.project_path().join(".am").join("sessions.json");
    let content = fs::read_to_string(&sessions_path)
        .unwrap_or_else(|_| panic!("sessions.json not found at {sessions_path:?}"));
    assert!(
        content.contains(&slug),
        "expected sessions.json to contain {slug:?}\ngot:\n{content}",
    );
}

#[then(expr = "the session file does not contain {string}")]
async fn then_session_not_contain(world: &mut AmWorld, slug: String) {
    let sessions_path = world.project_path().join(".am").join("sessions.json");
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

#[then(expr = "the file {string} contains {string}")]
async fn then_file_contains(world: &mut AmWorld, rel_path: String, text: String) {
    let path = world.project_path().join(&rel_path);
    let content = fs::read_to_string(&path).unwrap_or_else(|_| panic!("could not read {path:?}"));
    assert!(
        content.contains(&text),
        "expected {path:?} to contain {text:?}\ngot:\n{content}",
    );
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
        .run("tests/features")
        .await;
}

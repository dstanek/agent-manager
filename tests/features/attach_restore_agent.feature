Feature: Restore the agent on `am attach`
  A reboot survives at the worktree and session-record level, but tmux and the agent
  process do not. `am attach` must recreate them, not just the window — see
  specs/attach-restore-agent.md.

  Background:
    Given a git repository
    And I am inside a tmux session

  # ── UC-1: post-reboot recovery, host session ──────────────────────────────────

  Scenario: attach relaunches the recorded agent into a recreated window, resuming by default
    Given a session "my-feature" has been started with agent "claude"
    And the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window for session 'my-feature' and relaunched 'claude' (resuming)."
    And the mock tmux log contains "claude --continue"

  # ── UC-6: --fresh forces a clean conversation ─────────────────────────────────

  Scenario: attach --fresh relaunches without asking the agent to resume
    Given a session "my-feature" has been started with agent "claude"
    And the tmux window no longer exists
    When I run "am attach my-feature --fresh"
    Then the command succeeds
    # Deliberately checked as a substring ending right after the quote: the "(resuming)"
    # variant of this same message would NOT contain this exact substring, since something
    # else follows the agent name there. See CLAUDE.md's guidance on {string} captures and
    # backslash escapes — neither side of this comparison needs one, so a plain contains
    # check is safe here.
    And the output contains "relaunched 'claude'."
    And the mock tmux log does not contain "--continue"

  Scenario: [attach].resume = false has the same effect as --fresh, without the flag
    Given a project config containing "[attach]\nresume = false\n"
    And a session "my-feature" has been started with agent "claude"
    And the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "relaunched 'claude'."
    And the mock tmux log does not contain "--continue"

  # ── UC-2: post-reboot recovery, containerized session ─────────────────────────

  Scenario: attach recreates a gone container and hands the run command to the new split
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the tmux window no longer exists
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window for session 'my-feature' and restarted the container."
    # The window is split first, with no command (so the record's pane targets are valid
    # even if what follows fails — see recreate_attach_window's doc comment), and the
    # rebuilt "podman run ..." command line is then typed into the fresh agent pane via
    # send-keys rather than exec'd as the split's own shell_cmd — so the proof lives in the
    # tmux log's send-keys line, not the podman log. Mirrors "start with container records
    # container metadata" in container.feature, which pins the analogous shape for `am
    # start`, which — unlike attach — can exec the command at split time because its
    # preflight always runs before it ever touches tmux.
    And the mock tmux log contains "run"
    And the mock tmux log contains "--name am-my-feature"
    And the mock tmux log contains "test-image:latest"
    And the mock tmux log contains "split-window"
    And the mock tmux log contains "send-keys"

  # A3: the window *and split* must already exist by the time a container preflight failure
  # can occur ("the user is left with an opened window and a clear error"). recreate_attach_
  # window creates the window and splits it (with no command) before attempting the
  # container plan, and persists those pane targets immediately — so a preflight failure
  # here leaves a real, addressable window+split behind, and a retry can make progress
  # instead of silently no-op'ing forever against panes the record thinks exist but don't.
  Scenario: attach reports a clear error when the container runtime disappears before recreate
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the tmux window no longer exists
    And I have set env "AM_PODMAN_BIN" to "/nonexistent/am-test-podman"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/am-test-docker"
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command fails
    And the output does not contain "panicked"
    And the mock tmux log contains "new-window"
    And the mock tmux log contains "split-window"

  # The actual point of the fix: a preflight failure must be *recoverable*, not a dead end.
  # "I have set env ..." only applies to the one `run_am` invocation that follows it (see
  # AmWorld::run_am, which drains extra_env after each call) — so this second attach runs
  # with the working mock runtime restored, exactly as a user re-running `am attach` after
  # fixing whatever broke (e.g. `podman machine start`) would.
  #
  # Uses the dedicated "gone forever" tmux mock, not "the tmux window no longer exists": that
  # mock's fail-the-first-select-window-call-then-always-succeed approximation can't tell a
  # genuinely reused window from a stale target that happens to work anyway, so it can't prove
  # this property (confirmed: reverting the split-first ordering fix left this scenario green
  # under that mock — see the stub-and-revert note below). This mock tracks real window
  # identity: only a target actually created via `new-window` succeeds, and the original `@1`
  # is excluded permanently — so a retry only reaches the fast "already exists, relaunch"
  # path (and its distinct message) if the first attempt's window/split were genuinely
  # created *and persisted* before it failed.
  Scenario: a retry after a container preflight failure makes progress instead of looping
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the original tmux window is gone forever
    And I have set env "AM_PODMAN_BIN" to "/nonexistent/am-test-podman"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/am-test-docker"
    When I run "am attach my-feature"
    Then the command fails
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "container had exited — restarted it."

  # ── UC-3: fast no-op path when the pane is confidently alive ──────────────────

  Scenario: attach stays a no-op when the agent pane is confidently running (host)
    Given a session "my-feature" has been started with agent "claude"
    And the agent pane reports "claude" as its current command
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Attached to session 'my-feature'."
    And the output does not contain "relaunched"
    And the mock tmux log does not contain "send-keys"

  Scenario: attach stays a no-op when the container is confidently running
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the agent pane reports "podman" as its current command
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Attached to session 'my-feature'."
    And the output does not contain "restarted the container"
    And the mock tmux log does not contain "send-keys"

  # ── UC-4: live window, dead agent → relaunch in place ──────────────────────────

  Scenario: attach relaunches into an existing window whose agent pane went idle
    Given a session "my-feature" has been started with agent "claude"
    And the agent pane reports "bash" as its current command
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Attached to session 'my-feature'; agent had exited — relaunched 'claude' (resuming)."
    And the mock tmux log contains "send-keys"
    And the mock tmux log contains "claude --continue"
    And the mock tmux log does not contain "new-window"

  Scenario: attach restarts a container whose foreground process exited, into the same pane
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the agent pane reports "bash" as its current command
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Attached to session 'my-feature'; container had exited — restarted it."
    And the mock tmux log contains "send-keys"
    And the mock tmux log contains "run"
    And the mock tmux log does not contain "new-window"

  # ── UC-5: legacy session record (no agent ever recorded) ──────────────────────

  Scenario: a legacy record with no agent known still attaches cleanly and points at `am run`
    Given a session "my-feature" has been started
    And the session file has no recorded agent
    And the tmux window no longer exists
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window for session 'my-feature'."
    And the output contains "am attach does not know which agent to launch — run 'am run my-feature <agent>'"
    And the mock tmux log does not contain "send-keys"

  Scenario: a legacy record falls back to the configured agent and persists it
    Given a session "my-feature" has been started
    And the session file has no recorded agent
    And a project config containing "[defaults]\nagent = \"claude\"\n"
    And the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "relaunched 'claude' (resuming)."
    And the session file contains "claude"

  # ── Regression: `am run` supersedes what `am start` originally recorded ───────

  Scenario: attach relaunches whatever `am run` most recently launched, not what `am start` recorded
    Given a session "my-feature" has been started with agent "claude"
    When I run "am run my-feature codex"
    Then the command succeeds
    Given the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "relaunched 'codex' (resuming)."
    And the mock tmux log contains "codex resume --last"
    And the output does not contain "relaunched 'claude'"

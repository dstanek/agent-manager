Feature: agents defined in config, not compiled into the binary
  The four built-ins are values of the same type a config file produces, so an agent `am`
  has never heard of is not a special case — it reaches the same code the built-ins do.
  Every scenario here uses a name that appears nowhere in the source.

  Background:
    Given a git repository
    And I am inside a tmux session
    And am init has been run

  Scenario: an agent defined only in config starts and launches its command
    Given a project config containing "[agents.aider]\ncommand = [\"aider\", \"--model\", \"sonnet\"]\n"
    When I run "am start my-feature --agent aider --no-container"
    Then the command succeeds
    And the mock tmux log contains "aider"

  Scenario: a config-defined agent needs no credential integration
    Given a project config containing "[agents.plain]\ncommand = [\"plain-agent\"]\n"
    When I run "am start my-feature --agent plain --no-container"
    Then the command succeeds
    And the mock tmux log contains "plain-agent"

  Scenario: a config-defined agent is selectable as the default
    Given a project config containing "[defaults]\nagent = \"aider\"\n\n[agents.aider]\ncommand = [\"aider\"]\n"
    When I run "am start my-feature --no-container"
    Then the command succeeds
    And the mock tmux log contains "aider"

  Scenario: overriding one field of a built-in leaves the rest of it alone
    Given a project config containing "[agents.claude]\nauto_flags = []\n"
    And I am using a mock container runtime
    When I run "am start my-feature --agent claude --auto"
    Then the command succeeds
    And the mock tmux log does not contain "dangerously-skip-permissions"

  Scenario: an entry with no command says so rather than failing obscurely
    Given a project config containing "[agents.half]\nauto_flags = [\"--x\"]\n"
    When I run "am start my-feature --agent half --no-container"
    Then the command fails
    And the output contains "has no command"

  Scenario: an unknown agent lists the configured ones, including custom entries
    Given a project config containing "[agents.aider]\ncommand = [\"aider\"]\n"
    When I run "am start my-feature --agent nope --no-container"
    Then the command fails
    And the output contains "unknown agent 'nope'"
    And the output contains "aider"

  Scenario: a relative credential path is rejected rather than resolved against the cwd
    Given a project config containing "[agents.bad]\ncommand = [\"x\"]\n\n[[agents.bad.integration.mounts]]\nhost = \"creds\"\ncontainer = \"creds\"\n"
    When I run "am start my-feature --agent bad --no-container"
    Then the command fails
    And the output contains "must start with"

  Scenario: doctor reports a config-defined agent
    Given a project config containing "[defaults]\nagent = \"aider\"\n\n[agents.aider]\ncommand = [\"aider\"]\n"
    When I run "am doctor"
    Then the output contains "aider"

  Scenario: attach's container-recreate uses the resolved command, not the agent's section name
    # Regression for a fix made while porting the harness/agent decoupling design: attach's
    # container-recreate path could launch a session's own section name as a bare command
    # instead of the agent's actual (possibly different) `command`. Pins that a custom agent
    # whose command differs from its section name still launches correctly after a recreate —
    # the same shape as "attach recreates a gone container and hands the run command to the
    # new split" in attach_restore_agent.feature, but for a config-defined agent.
    Given a project config containing "[agents.my-harness]\ncommand = [\"my-agent\", \"--flag\"]\n"
    And I am using a mock container runtime
    And a session "my-feature" has been started with agent "my-harness"
    And the tmux window no longer exists
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window for session 'my-feature' and restarted the container."
    And the mock tmux log contains "respawn-pane"
    And the mock tmux log contains "my-agent --flag"

  # ── A section removed between `am start` and a later `am attach` ─────────────
  #
  # Fix 1 from the harness/agent decoupling port: a config-defined agent whose section is
  # deleted before a later `am attach` must fail loudly and explain what happened, not
  # silently launch the session's own recorded name as a bare command (see `agent_command`'s
  # doc comment). Two arms, because the two attach paths handle this differently:
  # container-recreate cannot proceed without a resolved agent and fails outright (still
  # leaving the window and split behind, per A3); the host-relaunch path degrades to "nothing
  # launched" and reports why, but `am attach` itself still succeeds.

  Scenario: a deleted section fails the container-recreate loudly, but leaves the window and split behind
    Given a project config containing "[agents.my-harness]\ncommand = [\"my-agent\"]\n"
    And I am using a mock container runtime
    And a session "my-feature" has been started with agent "my-harness"
    # The section that made "my-harness" resolvable is gone by the time attach re-resolves it.
    And a project config containing "[defaults]\nagent = \"claude\"\n"
    And the tmux window no longer exists
    And I clear the mock tmux log
    When I run "am attach my-feature"
    Then the command fails
    And the output contains "session 'my-feature' was started with agent 'my-harness', which no longer resolves"
    And the output contains "unknown agent 'my-harness'"
    # A3: the window and split must already exist by the time this failure can occur — a
    # retry can make progress against a real, addressable window instead of a session record
    # pointing at panes that were never created.
    And the mock tmux log contains "new-window"
    And the mock tmux log contains "split-window"

  Scenario: a deleted section degrades a host relaunch to nothing launched, with a note explaining why
    Given a project config containing "[agents.my-harness]\ncommand = [\"my-agent\"]\n"
    And a session "my-feature" has been started with agent "my-harness"
    And a project config containing "[defaults]\nagent = \"claude\"\n"
    And the tmux window no longer exists
    And I have set env "NO_COLOR" to ""
    And I have set env "CLICOLOR_FORCE" to "1"
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window for session 'my-feature'."
    And the output contains the note line "could not launch agent 'my-harness': config error: unknown agent 'my-harness' — configured agents are: claude, copilot, gemini, codex"

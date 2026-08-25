Feature: am start and am attach with tmux

  Background:
    Given a git repository
    And I am inside a tmux session

  Scenario: start creates a dedicated window and splits it
    When I run "am start my-feature"
    Then the command succeeds
    And the output contains "Started session 'my-feature'"
    And the mock tmux log contains "new-window"
    And the mock tmux log contains "am-my-feature"
    And the mock tmux log contains "split-window"

  Scenario: attach switches to the session window
    Given a session "my-feature" has been started
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "my-feature"
    And the mock tmux log contains "select-window"

  Scenario: attach recreates the window when it no longer exists
    Given a session "my-feature" has been started
    And the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window"
    And the mock tmux log contains "new-window"
    And the mock tmux log contains "split-window"

  Scenario: run has been removed and never touches tmux
    Given a session "my-feature" has been started
    And I clear the mock tmux log
    When I run "am run my-feature claude"
    Then the command fails
    And the output contains "am attach my-feature"
    And the mock tmux log does not contain "send-keys"

  Scenario: destroy kills the tmux window
    Given a session "my-feature" has been started
    When I run "am destroy --force my-feature"
    Then the command succeeds
    And the output contains "Destroyed session 'my-feature'"
    And the mock tmux log contains "kill-window"
    And the mock tmux log contains "am-my-feature"

  Scenario: attach to a container session with a missing window recreates the container
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Opened new window for session 'my-feature' and restarted the container."

  Scenario: attach's headline stays plain when its window is recreated
    Given a session "my-feature" has been started
    And the tmux window no longer exists
    And I have set env "NO_COLOR" to ""
    And I have set env "CLICOLOR_FORCE" to "1"
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains the plain line "Opened new window for session 'my-feature'."

  Scenario: attach's no-agent-known note is colored like every other Note
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the tmux window no longer exists
    And I have set env "NO_COLOR" to ""
    And I have set env "CLICOLOR_FORCE" to "1"
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains the plain line "Opened new window for session 'my-feature' and restarted the container."
    And the output contains the note line "am attach does not know which agent to launch — set 'defaults.agent' in .am/config.toml (or run 'am setup --agent <name>'), then run 'am attach my-feature' again"

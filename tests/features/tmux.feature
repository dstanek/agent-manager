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

  Scenario: run sends an agent command to the session's agent pane
    Given a session "my-feature" has been started
    When I run "am run my-feature claude"
    Then the command succeeds
    And the output contains "Launched 'claude'"
    And the mock tmux log contains "send-keys"
    And the mock tmux log contains "claude"

  Scenario: destroy kills the tmux window
    Given a session "my-feature" has been started
    When I run "am destroy --force my-feature"
    Then the command succeeds
    And the output contains "Destroyed session 'my-feature'"
    And the mock tmux log contains "kill-window"
    And the mock tmux log contains "am-my-feature"

  Scenario: attach to a container session with missing window suggests a clean restart
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the tmux window no longer exists
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "  Note: the container was stopped when the window closed."
    And the output contains "To restart cleanly: am destroy --force my-feature && am start my-feature"

  Scenario: attach's headline stays plain when its window is recreated
    Given a session "my-feature" has been started
    And the tmux window no longer exists
    And I have set env "NO_COLOR" to ""
    And I have set env "CLICOLOR_FORCE" to "1"
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains the plain line "Opened new window for session 'my-feature'."

  Scenario: attach's restart note is colored like every other Note, and stays outside the dimmed hint under it
    Given am init has been run
    And I am using a mock container runtime
    And a session "my-feature" has been started
    And the tmux window no longer exists
    And I have set env "NO_COLOR" to ""
    And I have set env "CLICOLOR_FORCE" to "1"
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains the note line "the container was stopped when the window closed."
    And the output contains the dimmed line "To restart cleanly: am destroy --force my-feature && am start my-feature"

Feature: am start — create an isolated agent session

  Background:
    Given a git repository

  Scenario: start a session creates a worktree and records state
    When I run "am start my-feature"
    Then the command succeeds
    And a worktree exists at ".am/worktrees/my-feature"
    And the session file contains "my-feature"

  Scenario: starting a duplicate session fails
    Given a session "my-feature" has been started
    When I run "am start my-feature"
    Then the command fails
    And the output contains "already exists"

  Scenario: start with --agent flag sends the agent to the tmux pane
    Given I am inside a tmux session
    When I run "am start my-feature --agent claude --no-container"
    Then the command succeeds
    And the mock tmux log contains "send-keys"
    And the mock tmux log contains "claude"

  Scenario: start --auto --no-container fails with clear error
    When I run "am start my-feature --auto --no-container"
    Then the command fails
    And the output contains "--no-container"

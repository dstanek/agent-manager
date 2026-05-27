Feature: jj workspace support — sessions in jj repos

  Background:
    Given a jj repository

  Scenario: start creates a jj workspace and records the session
    When I run "am start my-feature"
    Then the command succeeds
    And a worktree exists at ".am/worktrees/my-feature"
    And the session file contains "my-feature"

  Scenario: destroy removes the jj workspace and session record
    Given a session "my-feature" has been started
    When I run "am destroy --force my-feature"
    Then the command succeeds
    And the worktree ".am/worktrees/my-feature" does not exist
    And the session file does not contain "my-feature"

  Scenario: start with container in a jj repo records container metadata
    Given am init has been run
    And I am inside a tmux session
    And I am using a mock container runtime
    When I run "am start my-feature"
    Then the command succeeds
    And the session file contains "my-feature"
    And the session file contains "container"

  Scenario: destroy in a jj repo stops the container and removes the workspace
    Given am init has been run
    And I am inside a tmux session
    And I am using a mock container runtime
    And a session "my-feature" has been started
    When I run "am destroy --force my-feature"
    Then the command succeeds
    And the mock podman log contains "stop"
    And the worktree ".am/worktrees/my-feature" does not exist
    And the session file does not contain "my-feature"

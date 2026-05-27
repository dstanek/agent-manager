Feature: container integration — session isolation with a container runtime

  Background:
    Given a git repository
    And am init has been run
    And I am inside a tmux session
    And I am using a mock container runtime

  Scenario: start with container records container metadata in the session
    When I run "am start my-feature"
    Then the command succeeds
    And the output contains "container: am-my-feature"
    And the session file contains "container"
    And the mock tmux log contains "send-keys"

  Scenario: start with --no-container skips container setup
    When I run "am start my-feature --no-container"
    Then the command succeeds
    And the session file contains "my-feature"
    And the session file does not contain "podman"

  Scenario: destroy stops and removes the container
    Given a session "my-feature" has been started
    When I run "am destroy --force my-feature"
    Then the command succeeds
    And the mock podman log contains "stop"
    And the mock podman log contains "rm"
    And the session file does not contain "my-feature"

  Scenario: destroy stops the container and kills the tmux window
    Given a session "my-feature" has been started
    When I run "am destroy --force my-feature"
    Then the command succeeds
    And the mock podman log contains "stop"
    And the mock podman log contains "rm"
    And the mock tmux log contains "kill-window"
    And the session file does not contain "my-feature"

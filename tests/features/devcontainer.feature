Feature: Dev container sessions
  A session's environment can come from the repo's own .devcontainer/devcontainer.json
  instead of an am-managed image. am delegates the build to the reference CLI and runs
  the resulting image itself.

  Background:
    Given a git repository
    And I am inside a tmux session
    And am init has been run

  Scenario: starting a session builds the devcontainer image
    Given I am using a mock devcontainer CLI
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "Building devcontainer image"
    And the output contains "from devcontainer.json"
    And the mock devcontainer log contains "build"
    And the mock devcontainer log contains "--docker-path"
    And the session file contains "my-feature"

  Scenario: build options are passed after the subcommand
    Given I am using a mock devcontainer CLI
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock devcontainer log contains "build --workspace-folder"

  # The point of hashing the config: Node stays off the per-session path.
  Scenario: a second session on an unchanged config does not invoke the CLI again
    Given I am using a mock devcontainer CLI
    And the repo has a devcontainer config
    And I have set env "AM_CONTAINER_MODE" to "devcontainer"
    When I run "am start first" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    When I run "am start second" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock devcontainer CLI was called 1 time

  Scenario: a failed build rolls the worktree back
    Given I am using a mock devcontainer CLI that fails
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "An error occurred building the container"
    And the worktree ".am/worktrees/my-feature" does not exist
    And the session file does not contain "my-feature"

  Scenario: auto mode falls back to an image when there is no devcontainer config
    Given I am using a mock devcontainer CLI
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "auto"
    Then the command succeeds
    And the mock devcontainer CLI was called 0 times
    And the session file contains "my-feature"

  Scenario: devcontainer mode without a config reports what to do
    Given I am using a mock devcontainer CLI
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "no devcontainer.json was found"
    And the worktree ".am/worktrees/my-feature" does not exist

  Scenario: compose-based configs are rejected with a way forward
    Given I am using a mock devcontainer CLI
    And the repo has a devcontainer config using docker compose
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "dockerComposeFile"
    And the output contains "container.mode"
    And the worktree ".am/worktrees/my-feature" does not exist

  # initializeCommand runs on the host, outside every boundary am provides.
  Scenario: initializeCommand is refused by default
    Given I am using a mock devcontainer CLI
    And the repo has a devcontainer config with an initializeCommand
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "runs on your host"
    And the output contains "allow_host_commands"
    And the mock devcontainer CLI was called 0 times

  Scenario: image mode ignores a devcontainer config entirely
    Given I am using a mock devcontainer CLI
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "image"
    Then the command succeeds
    And the mock devcontainer CLI was called 0 times

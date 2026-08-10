Feature: am doctor — readiness reporting
  Reports what is and is not ready for a successful `am start`, without changing
  anything. Exits non-zero when something would actually stop `am start` working, so
  it can gate a setup script.

  Scenario: a ready project reports ready and exits zero
    Given a git repository
    And am init has been run
    And I am using a mock container runtime
    When I run "am doctor"
    Then the command succeeds
    And the output contains "Ready"
    And the output contains "git repository at"

  Scenario: running outside a repository is reported, not thrown
    Given no git repository
    When I run "am doctor"
    Then the command fails
    And the output contains "not inside a git or jj repository"

  Scenario: an uninitialized project points at am init
    Given a git repository
    And I am using a mock container runtime
    When I run "am doctor"
    Then the command fails
    And the output contains "not initialized"
    And the output contains "run 'am init'"

  Scenario: a missing container runtime is a failure with install guidance
    Given a git repository
    And am init has been run
    And I have set env "AM_CONTAINER_ENABLED" to "true"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/docker"
    When I run "am doctor" with env "AM_PODMAN_BIN" = "/nonexistent/podman"
    Then the command fails
    And the output contains "Podman"

  Scenario: an unknown agent is rejected with the valid names
    Given a git repository
    And am init has been run
    And I am using a mock container runtime
    When I run "am doctor" with env "AM_AGENT" = "not-an-agent"
    Then the command fails
    And the output contains "unknown agent"
    And the output contains "claude"

  Scenario: a discovered devcontainer config is reported as the environment source
    Given a git repository
    And am init has been run
    And I am using a mock devcontainer CLI
    And the repo has a devcontainer config
    When I run "am doctor"
    Then the command succeeds
    And the output contains "devcontainer at"
    And the output contains "not built yet"

  Scenario: initializeCommand is reported as refused before a session is ever started
    Given a git repository
    And am init has been run
    And I am using a mock devcontainer CLI
    And the repo has a devcontainer config with an initializeCommand
    When I run "am doctor"
    Then the command fails
    And the output contains "runs on your host"
    And the output contains "allow_host_commands"

  Scenario: a compose config is reported as unsupported before a session is ever started
    Given a git repository
    And am init has been run
    And I am using a mock devcontainer CLI
    And the repo has a devcontainer config using docker compose
    When I run "am doctor"
    Then the command fails
    And the output contains "dockerComposeFile"

  # The whole point of the command: it is safe to run at any time.
  Scenario: doctor changes nothing
    Given a git repository
    And I am using a mock container runtime
    When I run "am doctor"
    Then the worktree ".am/worktrees" does not exist
    And the file ".am/config.toml" does not exist

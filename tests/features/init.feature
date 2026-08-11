Feature: am init — initialize am in a repo

  Scenario: init creates the .am directory and config files
    Given a git repository
    When I run "am init"
    Then the command succeeds
    And the file ".am/config.toml" exists
    And the file ".gitignore" contains ".am/worktrees/"

  Scenario: init is idempotent when run twice
    Given a git repository
    And am init has been run
    When I run "am init"
    Then the command succeeds
    And the output contains "already exists"

  Scenario: init fails outside a repo
    Given no git repository
    When I run "am init"
    Then the command fails
    And the output contains "not in a git or jj repository"

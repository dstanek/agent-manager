Feature: am list — display active sessions

  Background:
    Given a git repository

  Scenario: list reports no sessions when none exist
    When I run "am list"
    Then the command succeeds
    And the output contains "No active sessions"

  Scenario: list shows an existing session
    Given a session "my-feature" has been started
    When I run "am list"
    Then the command succeeds
    And the output contains "my-feature"

  Scenario: list output has column headers
    Given a session "my-feature" has been started
    When I run "am list"
    Then the command succeeds
    And the output contains "SLUG"
    And the output contains "CONTAINER"
    And the output contains "WORKTREE"
    And the output contains "CREATED"

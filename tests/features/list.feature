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

  # A session record outlives the checkout it describes: the repo is deleted, moved, or lives
  # on a drive that is not mounted today. `am list --all` is the command that has to keep
  # working anyway — it is how you find such a session in order to destroy it.
  Scenario: a session whose repository is gone is marked stale, not hidden
    Given a session "my-feature" has been started
    And the session "my-feature" points at a repository that no longer exists
    When I run "am list --all"
    Then the command succeeds
    And the output contains "my-feature"
    And the output contains "stale"

  # The healthy session is the one that matters here: a stale record next to it must not take
  # the whole listing down, or one dead checkout hides every live session you have.
  Scenario: a stale record does not hide healthy sessions
    Given a session "healthy" has been started
    And a session "gone" has been started
    And the session "gone" points at a repository that no longer exists
    When I run "am list --all"
    Then the command succeeds
    And the output contains "healthy"
    And the output contains "gone"

  # Records written by older versions are missing keys that were added later. Loading has
  # serde defaults for exactly this, but nothing drove it through the public command.
  Scenario: a record missing later-added fields still lists
    Given a session "my-feature" has been started
    And the session "my-feature" is missing its optional fields
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

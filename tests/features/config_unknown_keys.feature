Feature: unrecognised configuration keys are reported by every command that reads config

  A misspelled key parses fine and is then discarded, so the setting the user believes
  they made never happens. Warning is per-command rather than central, so each command
  that loads config needs its own coverage — silence in one of them reads as approval.

  Background:
    Given a git repository
    And I am inside a tmux session

  Scenario: am start warns about an unknown key
    Given a project config containing "[defaults]\nagnet = \"claude\"\n"
    When I run "am start my-feature"
    Then the command succeeds
    And the output contains "unknown config key defaults.agnet"

  Scenario: am attach warns about an unknown key
    Given a session "my-feature" has been started
    And the tmux window no longer exists
    And a project config containing "[defaults]\nagnet = \"claude\"\n"
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "unknown config key defaults.agnet"

  Scenario: a config with no unknown keys produces no warning
    Given a project config containing "[defaults]\nagent = \"claude\"\n"
    When I run "am start my-feature"
    Then the command succeeds
    And the output does not contain "unknown config key"

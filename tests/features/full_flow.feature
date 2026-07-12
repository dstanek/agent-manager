Feature: full session lifecycle

  Scenario: init then start then list then destroy
    Given a git repository
    When I run "am init"
    Then the command succeeds
    When I run "am start my-feature"
    Then the command succeeds
    When I run "am list"
    Then the command succeeds
    And the output contains "my-feature"
    When I run "am destroy --force my-feature"
    Then the command succeeds
    When I run "am list"
    Then the command succeeds
    And the output contains "No active sessions"

  Scenario: multiple sessions can coexist
    Given a git repository
    When I run "am start feature-a"
    Then the command succeeds
    When I run "am start feature-b"
    Then the command succeeds
    When I run "am list"
    Then the command succeeds
    And the output contains "feature-a"
    And the output contains "feature-b"

Feature: error handling — clear, early errors for invalid usage

  Scenario: start outside a repo fails with a clear message
    Given no git repository
    When I run "am start my-feature"
    Then the command fails
    And the output contains "not in a git or jj repository"

  Scenario: start with an invalid slug fails
    Given a git repository
    When I run "am start INVALID-SLUG"
    Then the command fails

  Scenario: start with an unknown agent fails before creating any worktree
    Given a git repository
    When I run "am start my-feature --agent bogus-agent"
    Then the command fails
    And the output contains "bogus-agent"
    And the worktree ".am/worktrees/my-feature" does not exist

  Scenario: attach with an unknown session fails
    Given a git repository
    When I run "am attach no-such-session"
    Then the command fails
    And the output contains "not found"

  Scenario: attach when not inside tmux fails
    Given a git repository
    And a session "my-feature" has been started
    When I run "am attach my-feature"
    Then the command fails
    And the output contains "tmux"

  Scenario: run when not inside tmux fails
    Given a git repository
    And a session "my-feature" has been started
    When I run "am run my-feature claude"
    Then the command fails
    And the output contains "tmux"

  Scenario: run with an unknown session fails while inside tmux
    Given a git repository
    And I am inside a tmux session
    When I run "am run no-such-session claude"
    Then the command fails
    And the output contains "not found"

Feature: configuration via environment variables

  Background:
    Given a git repository
    And I am inside a tmux session

  Scenario: AM_TMUX_SPLIT=vertical produces a vertical split
    When I run "am start my-feature" with env "AM_TMUX_SPLIT" = "vertical"
    Then the command succeeds
    And the mock tmux log contains "split-window"
    And the mock tmux log contains "-v"

  Scenario: AM_TMUX_SPLIT_PERCENT=30 passes the percentage to tmux
    Given I have set env "AM_TMUX_SPLIT_PERCENT" to "30"
    And I have set env "AM_TMUX_AGENT_PANE" to "right"
    When I run "am start my-feature"
    Then the command succeeds
    And the mock tmux log contains "split-window"
    And the mock tmux log contains "-p"
    And the mock tmux log contains "30"

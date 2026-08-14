Feature: am init — initialize am in a repo

  Scenario: a first run creates the .am directory and config files
    Given a git repository
    When I run "am init"
    Then the command succeeds
    And the file ".am/config.toml" exists
    And the file ".gitignore" contains ".am/worktrees/"
    # `init_project`'s report is rendered two ways now (see `print_init_line_plain` and
    # `print_init_line_dim` in main.rs), and `am init` wraps its rendering in a headline
    # (`render_init_report`) that `am setup` never prints for itself. Pinned literally rather
    # than just checking the files it wrote, since it's the wording and layout — not just the
    # end state — that has to survive the refactor unchanged.
    And the output contains "Initialized am in this repo."
    And the output contains "  Created .am/config.toml"
    And the output contains "  Added .am/worktrees/ to .gitignore"
    And the output contains "Run 'am start <slug>' to create your first session."
    # The headline itself stays flush left and plain — only the detail underneath it is
    # indented, and `am init` never dims (that's `am setup`'s treatment of the same report).
    And the output does not contain "  Initialized am in this repo."
    And the output does not contain "\x1b[2m"

  Scenario: init appends correctly when .gitignore lacks a trailing newline
    Given a git repository
    And the file ".gitignore" contains "target/" without a trailing newline
    When I run "am init"
    Then the command succeeds
    And the file ".gitignore" contains ".am/worktrees/"
    And the file ".gitignore" does not contain "target/.am/worktrees/"

  Scenario: a re-run with nothing to do collapses to a two-line summary
    Given a git repository
    And am init has been run
    When I run "am init"
    Then the command succeeds
    # Nothing changed, so the per-file detail is dropped rather than restating two "already
    # exists" lines under a headline that already says so.
    And the output contains "am is already initialized in this repo."
    And the output contains "  Run 'am start <slug>' to create your first session."
    And the output does not contain "already exists, skipping"
    And the output does not contain "already in .gitignore, skipping"
    And the output does not contain "Initialized am in this repo."

  Scenario: a mixed run shows both what changed and what was already fine
    Given a git repository
    And a project config containing "[defaults]\n"
    When I run "am init"
    Then the command succeeds
    # The config file already existed but .gitignore did not yet have the worktrees entry —
    # one action was taken and one was not, so the headline reflects that something changed
    # and the detail spells out which part was which.
    And the output contains "Initialized am in this repo."
    And the output contains "  .am/config.toml already exists, skipping"
    And the output contains "  Added .am/worktrees/ to .gitignore"

  Scenario: init advises narrowing a pre-existing broad .am/ gitignore entry
    Given a git repository
    And the file ".gitignore" contains ".am/" without a trailing newline
    When I run "am init"
    Then the command succeeds
    And the output contains "Note: .am/ is in .gitignore; .am/config.toml is now committable"
    And the output contains "you may want to narrow this to .am/worktrees/"

  Scenario: the advisory still appears on a re-run with nothing else to do
    Given a git repository
    And the file ".gitignore" contains ".am/" without a trailing newline
    And am init has been run
    When I run "am init"
    Then the command succeeds
    # The collapse rule for a no-op re-run only drops the per-file status detail; the
    # advisory is content the user asked for, not structure describing what this run did, so
    # it must survive the collapse too.
    And the output contains "am is already initialized in this repo."
    And the output contains "Note: .am/ is in .gitignore; .am/config.toml is now committable"

  Scenario: init fails outside a repo
    Given no git repository
    When I run "am init"
    Then the command fails
    And the output contains "not in a git or jj repository"

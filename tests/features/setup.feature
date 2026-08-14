Feature: am setup — guided setup
  Exercises only `am setup --yes[, --agent <name>]`: the cucumber harness has no seam for
  feeding interactive stdin, so the interactive prompt logic is covered by onboarding.rs's
  own unit tests against ScriptedIo. This layer proves the pieces those unit tests exercise
  in isolation (detection, the two update_* functions, doctor::run) work together end to end
  through the real binary and a real filesystem.

  Background:
    Given an isolated home directory
    And a git repository

  Scenario: greenfield setup creates config, sets the agent, and verifies readiness
    Given claude credentials are present
    And I am using a mock container runtime
    When I run "am setup --yes --agent claude"
    Then the command succeeds
    And the file ".am/config.toml" exists
    And the project config sets "defaults.agent" to "claude"
    # The active line replaces the commented example rather than sitting above it — a
    # brand-new file must not read like defaults.agent was set twice.
    And the file ".am/config.toml" does not contain "# agent = "
    And the output contains "Set defaults.agent"
    And the file ".gitignore" contains ".am/worktrees/"
    And the global config file exists
    And the output contains "Ready"

  Scenario: setup's init report renders dimmed — the contrast to init's own plain report
    # init.feature's "init's report never dims, even when color is forced on" scenario pins
    # the other half of this rule: the same report (`InitLine`, `main.rs`) renders plain for
    # `am init` and dimmed for `am setup`. Neither half means anything without the other —
    # a renderer that dims unconditionally would still pass an "am setup dims" check alone.
    Given I have set env "NO_COLOR" to ""
    And I have set env "CLICOLOR_FORCE" to "1"
    And claude credentials are present
    And I am using a mock container runtime
    When I run "am setup --yes --agent claude"
    Then the command succeeds
    And the output contains the dimmed line "Created .am/config.toml"

  Scenario: setup preserves comments and table order when changing the agent
    Given a project config containing "# custom note above container\n[container]\nenabled = true\n\n# defaults section, deliberately placed below container\n[defaults]\nagent = \"codex\"  # picked for the OPENAI project\n"
    And claude credentials are present
    And I am using a mock container runtime
    When I run "am setup --yes --agent claude"
    Then the command succeeds
    And the project config sets "defaults.agent" to "claude"
    And the file ".am/config.toml" contains "# custom note above container"
    And the file ".am/config.toml" contains "# defaults section, deliberately placed below container"
    And the file ".am/config.toml" contains "# picked for the OPENAI project"
    And the file ".am/config.toml" does not contain 'agent = "codex"'

  Scenario: --yes on an already-configured, doctor-clean repo is a true no-op
    Given a project config containing "[defaults]\nagent = \"claude\"\n"
    And a global config containing "[container]\nenabled = true\n"
    And claude credentials are present
    And I am using a mock container runtime
    And I record the state of the project config file
    And I record the state of the global config file
    When I run "am setup --yes"
    Then the command succeeds
    And the output contains "Ready"
    And the project config file is unchanged
    And the global config file is unchanged

  Scenario: --yes --agent codex on a project already set to codex is a no-op
    Given a project config containing "[defaults]\nagent = \"codex\"\n"
    And a global config containing "[defaults]\n"
    And I have set env "OPENAI_API_KEY" to "sk-test-not-a-real-key"
    And I am using a mock container runtime
    And I record the state of the project config file
    When I run "am setup --yes --agent codex"
    Then the command succeeds
    And the project config file is unchanged

  Scenario: setup writes a changed agent to the project file, not the global one
    Given a global config containing "[defaults]\nagent = \"codex\"\n"
    And claude credentials are present
    And I am using a mock container runtime
    And I record the state of the global config file
    When I run "am setup --yes --agent claude"
    Then the command succeeds
    And the project config sets "defaults.agent" to "claude"
    And the global config sets "defaults.agent" to "codex"
    And the global config file is unchanged

  Scenario: non-interactive stdin without --yes fails fast and touches nothing
    When I run "am setup"
    Then the command fails
    And the output contains "requires an interactive terminal"
    And the file ".am/config.toml" does not exist
    And the global config file does not exist

  Scenario: --yes exits 0 when the report is clean
    Given claude credentials are present
    And I am using a mock container runtime
    When I run "am setup --yes"
    Then the command succeeds
    And the output contains "Ready"

  Scenario: --yes exits 1 when the report has failures, and next steps are not printed
    Given I have set env "AM_CONTAINER_ENABLED" to "true"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/docker"
    When I run "am setup --yes" with env "AM_PODMAN_BIN" = "/nonexistent/podman"
    Then the command fails
    And the output contains "problem"
    And the output does not contain "Next steps"

  Scenario: a failing report ends with concrete remediation, not a bare pointer back at itself
    Given I have set env "AM_CONTAINER_ENABLED" to "true"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/docker"
    When I run "am setup --yes" with env "AM_PODMAN_BIN" = "/nonexistent/podman"
    Then the command fails
    And the output contains "What to do next:"
    And the output contains "  - install Podman"
    And the output contains "Then re-run 'am setup'."
    And the output does not contain "Fix the items above, then re-run 'am setup'."

  Scenario: an unknown --agent value is rejected before any file is written
    When I run "am setup --yes --agent not-a-real-agent"
    Then the command fails
    And the output contains "unknown agent"
    And the file ".am/config.toml" does not exist
    And the global config file does not exist

  Scenario: a discovered devcontainer config is reported, with no prompt
    Given claude credentials are present
    And I am using a mock devcontainer CLI
    And the repo has a devcontainer config
    When I run "am setup --yes"
    Then the command succeeds
    And the output contains "sessions will use it automatically"
    # Pins `found_devcontainer_line`'s wiring in `cmd_setup` — the last of the five
    # shortened-path call sites (see the coverage note in setup_interactive.feature's
    # "every question states where its answer will be saved" scenario); previously only
    # unit-tested against a synthetic repo root, never against a real one end to end.
    And the output contains "Found .devcontainer/devcontainer.json"

  # Verification (doctor::run) independently refuses initializeCommand — see
  # doctor.feature's "initializeCommand is reported as refused" — so this is a failing
  # report, not merely a note. The two are worth pinning down together, since setup's own
  # step-6 note runs *before* the verification step re-derives and fails on the same fact.
  Scenario: a devcontainer with initializeCommand is flagged, never silently enabled
    Given claude credentials are present
    And I am using a mock devcontainer CLI
    And the repo has a devcontainer config with an initializeCommand
    When I run "am setup --yes"
    Then the command fails
    And the output contains "runs on your host"
    And the output contains "allow_host_commands"

  # --yes and pane layout: the deliberate asymmetry with the agent question — see
  # "Non-interactive / non-TTY behaviour" in specs/guided-setup.md. The interactive layout
  # menu itself (presets, customize, write-target lines) is covered by setup_interactive.feature.

  Scenario: --yes never asks about pane layout, so nothing is written to a fresh global config
    Given claude credentials are present
    And I am using a mock container runtime
    When I run "am setup --yes --agent claude"
    Then the command succeeds
    And the global config does not set "tmux.agent_pane"
    And the global config does not set "tmux.split"
    And the global config does not set "tmux.split_percent"
    And the output does not contain "Pane layout"

  Scenario: --yes leaves an already-configured pane layout completely untouched
    Given a global config containing "[tmux]\nagent_pane = \"right\"\nsplit = \"vertical\"\nsplit_percent = 30\n"
    And claude credentials are present
    And I am using a mock container runtime
    And I record the state of the global config file
    When I run "am setup --yes --agent claude"
    Then the command succeeds
    And the global config file is unchanged

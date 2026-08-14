Feature: am setup — the interactive prompts
  setup.feature exercises only `am setup --yes[, --agent <name>]`, because a plain-pipe
  subprocess is never a TTY and `cmd_setup` gates every prompt on `stdin().is_terminal()`.
  These scenarios drive the real binary through an actual pseudo-terminal (via the `script`
  utility — see `AmWorld::run_am_pty`), so the prompts that only run interactively — the
  write-target lines, the pane-layout menu, and its customize sub-flow — get end-to-end
  coverage against the real process and a real filesystem, not just `onboarding.rs`'s
  `ScriptedIo` unit tests.

  Every scenario passes `--agent claude` and mocks a container runtime so the only prompt
  left to drive is the layout question, unless a scenario is specifically about a different
  question (the write-target-line scenario) or needs the containers question too.

  Background:
    Given an isolated home directory
    And a git repository
    And claude credentials are present
    And I am using a mock container runtime

  Scenario: verification is printed before the layout question on a clean report
    # Pins the new ordering (Resolved Decisions #10) directly, not just the presence of both
    # pieces of text — `then_output_order` proves "Checking your setup..." actually came
    # first, which two separate `contains` checks could not.
    Given a global config containing ""
    When I run "am setup --agent claude" through a pty with input "1\nn"
    Then the command succeeds
    And the output contains "Checking your setup..." before "Which layout do you want?"

  Scenario: the layout question is never reached when verification still fails
    # A returning setup (global config already exists) with no runtime reachable: the
    # containers question offers to disable containers; declining ("n") leaves them enabled,
    # so verification still fails on the missing runtime and the layout question — reached
    # only after a clean report — is never shown.
    Given a global config containing ""
    And I have set env "AM_PODMAN_BIN" to "/nonexistent/podman"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/docker"
    When I run "am setup --agent claude" through a pty with input "n"
    Then the command fails
    And the output contains "What to do next:"
    And the output does not contain "Which layout do you want?"

  Scenario Outline: choosing a layout preset writes exactly that triple to the global config, never the project one
    Given a global config containing "[tmux]\nagent_pane = \"right\"\nsplit = \"vertical\"\nsplit_percent = 33\n"
    When I run "am setup --agent claude" through a pty with input "<answer>\nn"
    Then the command succeeds
    And the global config sets "tmux.agent_pane" to "<agent_pane>"
    And the global config sets "tmux.split" to "<split>"
    And the global config sets "tmux.split_percent" to "<split_percent>"
    And the project config does not set "tmux.agent_pane"
    And the project config does not set "tmux.split"
    And the project config does not set "tmux.split_percent"

    Examples:
      | answer | agent_pane | split      | split_percent |
      | 1      | left       | horizontal | 50             |
      | 2      | right      | horizontal | 50             |
      | 3      | left       | horizontal | 70             |
      | 4      | left       | vertical   | 50             |

  Scenario: customize words the pane question left/right after a horizontal split, and writes the chosen triple
    # No global config exists yet, so this is a fresh setup: the containers consent question
    # (ask_container_consent) fires before verification and layout. Accepting its default
    # ("") keeps container.enabled at its compiled-default true, matching this scenario's
    # mocked runtime and writing nothing — the extra token is purely to get past it.
    When I run "am setup --agent claude" through a pty with input "\n5\n1\n2\n60\ny\nn"
    Then the command succeeds
    And the output contains "Which side should the agent be on?"
    And the output contains "[1] left"
    And the output contains "[2] right"
    And the output does not contain "top or on the bottom"
    And the global config sets "tmux.agent_pane" to "right"
    And the global config sets "tmux.split" to "horizontal"
    And the global config sets "tmux.split_percent" to "60"
    # Pins `set_tmux_layout_line`'s wiring in `cmd_setup` (see the coverage note on "every
    # question states where its answer will be saved" above) — the fourth of the five
    # shortened-path call sites; only unit-tested against synthetic paths before this.
    And the output contains "Set tmux.agent_pane, tmux.split, tmux.split_percent in ~/.config/am/config.toml"

  Scenario: customize words the pane question top/bottom after a vertical split, and writes the chosen triple
    # See the note on the scenario above: fresh setup, so the leading "" accepts the
    # containers consent question's default before the layout customize flow begins.
    When I run "am setup --agent claude" through a pty with input "\n5\n2\n1\n40\ny\nn"
    Then the command succeeds
    And the output contains "Should the agent be on top or on the bottom?"
    And the output contains "[1] top"
    And the output contains "[2] bottom"
    And the output does not contain "Which side should the agent be on?"
    And the global config sets "tmux.agent_pane" to "left"
    And the global config sets "tmux.split" to "vertical"
    And the global config sets "tmux.split_percent" to "40"

  Scenario: declining the customize preview re-shows the preset menu instead of the whole flow restarting elsewhere
    # See the note on "customize words the pane question left/right ..." above: fresh setup,
    # so the leading "" accepts the containers consent question's default first.
    When I run "am setup --agent claude" through a pty with input "\n5\n\n\n\nn\n2\nn"
    Then the command succeeds
    And the output contains "Use this layout?"
    And the global config sets "tmux.agent_pane" to "right"
    And the global config sets "tmux.split" to "horizontal"
    And the global config sets "tmux.split_percent" to "50"

  Scenario: every question states where its answer will be saved, with paths shortened for display
    # No global config exists yet (this scenario never seeds one), so this is a fresh setup:
    # the containers question is `ask_container_consent`, not the returning-setup
    # `ask_container_enabled` — it fires alongside the agent and layout questions regardless
    # of the Background's mocked (found) runtime, so all three write-target lines appear in
    # the same run. Declining it ("n") still writes container.enabled = false, and — because a
    # runtime genuinely is mocked as present — verification still passes, so the layout
    # question is still reached. The write-target line no longer names its own question
    # (that's the header line right above it now) — containers and layout share the exact
    # same "every repo on this machine; saved to ..." text, so each question's own header is
    # asserted alongside it to confirm it was actually shown, not just present somewhere in
    # the output.
    When I run "am setup" through a pty with input "\nn\n\nn"
    Then the output contains "Which agent do you use?"
    And the output contains "just this repo; saved to .am/config.toml."
    And the output contains "Use isolated containers for your sessions?"
    # A runtime is mocked as present (the feature Background), so the consent question's "no
    # runtime found yet" note must not appear, and neither must the old failure-framed
    # question it replaces on a fresh setup — see Resolved Decisions #12.
    And the output does not contain "no container runtime was found"
    And the output does not contain "No container runtime found on this machine"
    And the output contains "Which layout do you want?"
    And the output contains "every repo on this machine; saved to ~/.config/am/config.toml."
    # The confirmation lines below are the other place these five call sites (main.rs's
    # `created_global_config_line`, `set_project_agent_line`, `set_container_enabled_line`,
    # `set_tmux_layout_line`, `found_devcontainer_line`) are only unit-tested in isolation — no
    # test previously proved `cmd_setup` actually wires the real `home_dir`/`repo_root` through
    # to them. Pinning three of the five here (the other two, tmux layout and devcontainer, are
    # pinned in the scenarios below and in setup.feature respectively) closes that gap.
    And the output contains "Created ~/.config/am/config.toml"
    And the output contains 'Set defaults.agent = "claude" in .am/config.toml'
    And the output contains "Set container.enabled = false in ~/.config/am/config.toml"
    # This run touches every dim line `am setup` prints (the init report, both write-target
    # lines, both "currently: ..." lines) — the one scenario worth spending the ANSI-byte check
    # on, since `run_am_pty`'s `NO_COLOR=1` is what keeps every `contains` assertion above
    # meaningful: `contains` still matches its substring even if ANSI codes wrapped around it,
    # so nothing above would fail if `NO_COLOR` silently stopped working.
    And the output contains no color escape codes

  Scenario: the containers consent question notes a missing runtime without blocking the choice
    # Fresh setup (no global config seeded) with no runtime reachable — the consent question
    # still fires (it is never gated on runtime absence, unlike the returning-setup framing
    # below), and adds its one extra dim note. Verification fails afterwards regardless of the
    # answer given here (no real runtime, and the mocked-runtime test harness itself forces
    # container.enabled = true via AM_CONTAINER_ENABLED) — this scenario only cares what the
    # consent question itself printed before that happens.
    Given I have set env "AM_PODMAN_BIN" to "/nonexistent/podman"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/docker"
    When I run "am setup" through a pty with input "\n\n"
    Then the output contains "Use isolated containers for your sessions?"
    And the output contains "no container runtime was found on this machine yet"

  Scenario: a returning setup still uses the original failure-framed containers question, unchanged
    # A global config already exists (even an empty one), so this is a returning setup: the
    # containers question is the original `ask_container_enabled`, not `ask_container_consent`
    # — the two framings are mutually exclusive, and this pins the other half of that.
    Given a global config containing ""
    And I have set env "AM_PODMAN_BIN" to "/nonexistent/podman"
    And I have set env "AM_DOCKER_BIN" to "/nonexistent/docker"
    When I run "am setup --agent claude" through a pty with input "n"
    Then the output contains "No container runtime found on this machine (neither podman nor docker)."
    And the output does not contain "Use isolated containers for your sessions?"

  Scenario: an interactive first run leaves no stale example above any value it activates
    # Fresh setup: the second "" accepts the containers consent question's default, ahead of
    # the layout customize flow — see the note on "customize words the pane question left/right
    # ..." above.
    When I run "am setup" through a pty with input "\n\n5\n2\n2\n77\ny\nn"
    Then the command succeeds
    And the file ".am/config.toml" does not contain '# agent = "claude"'
    And the file ".am/config.toml" contains 'agent = "claude"'
    And the global config file does not contain '# agent_pane = "left"'
    And the global config file does not contain '# split = "horizontal"'
    And the global config file does not contain "# split_percent = 50"
    And the global config sets "tmux.agent_pane" to "right"
    And the global config sets "tmux.split" to "vertical"
    And the global config sets "tmux.split_percent" to "77"

  Scenario: a project config from an earlier "am init" invocation also loses its stale agent example
    Given am init has been run
    # Fresh setup (an earlier "am init" only creates the project file, not the global one), so
    # the second "" accepts the containers consent question's default.
    When I run "am setup" through a pty with input "\n\n1\nn"
    Then the command succeeds
    And the file ".am/config.toml" contains 'agent = "claude"'
    And the file ".am/config.toml" does not contain '# agent = "claude"'

  Scenario: a hand-edited near-miss example line survives, alongside unrelated keys and table order
    Given a global config containing "# my machine\n[container]\nenabled = true\n\n[tmux]\nagent_pane = \"left\"\n# agent_pane = \"left\"   # example I kept deliberately\nsplit_percent = 50\n"
    When I run "am setup --agent claude" through a pty with input "5\n1\n2\n80\ny\nn"
    Then the command succeeds
    And the global config file contains "# my machine"
    And the global config file contains "enabled = true"
    And the global config file contains '# agent_pane = "left"   # example I kept deliberately'
    And the global config file has "[container]" before "[tmux]"
    And the global config sets "tmux.agent_pane" to "right"
    And the global config sets "tmux.split_percent" to "80"

  Scenario: pressing Enter through every interactive question is a genuine no-op on both files
    Given a project config containing "[defaults]\nagent = \"claude\"\n"
    And a global config containing "[tmux]\nagent_pane = \"right\"\nsplit = \"vertical\"\nsplit_percent = 30\n[container]\nenabled = true\n"
    And I record the state of the project config file
    And I record the state of the global config file
    When I run "am setup" through a pty with input "\n\nn"
    Then the command succeeds
    And the project config file is unchanged
    And the global config file is unchanged

  Scenario: the project-override caveat appears only when the project config sets its own layout
    Given a project config containing "[defaults]\nagent = \"claude\"\n[tmux]\nsplit_percent = 65\n"
    # Fresh setup (no global config seeded): the leading "" accepts the containers consent
    # question's default before the layout question is reached.
    When I run "am setup --agent claude" through a pty with input "\n1\nn"
    Then the command succeeds
    And the output contains "this project's config already sets its own pane layout"

  Scenario: no project-override caveat when the project config sets nothing of its own
    # Fresh setup: same leading "" as the scenario above.
    When I run "am setup --agent claude" through a pty with input "\n1\nn"
    Then the command succeeds
    And the output does not contain "already sets its own"

Feature: Dev container sessions
  A session's environment can come from the repo's own .devcontainer/devcontainer.json
  instead of an am-managed image. am builds the image itself when it can, delegates to the
  reference CLI when the config uses something it does not implement, and runs the
  resulting image either way.

  Background:
    Given a git repository
    And I am inside a tmux session
    And am init has been run

  Scenario: starting a session builds the devcontainer image
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "Building devcontainer image"
    And the output contains "from devcontainer.json"
    And the mock podman log contains "build"
    And the session file contains "my-feature"

  Scenario: a failed build rolls the worktree back
    Given I am using a devcontainer builder whose build fails
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "An error occurred building the container"
    And the worktree ".am/worktrees/my-feature" does not exist
    And the session file does not contain "my-feature"

  Scenario: auto mode falls back to an image when there is no devcontainer config
    Given I am using am's own devcontainer builder
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "auto"
    Then the command succeeds
    And the session file contains "my-feature"

  Scenario: devcontainer mode without a config reports what to do
    Given I am using am's own devcontainer builder
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "no devcontainer.json was found"
    And the worktree ".am/worktrees/my-feature" does not exist

  # A compose config is a whole project, so am brings it up and execs the agent into the
  # named service rather than running one container.
  Scenario: a compose config starts its project and runs the agent in the named service
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config using docker compose
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock podman log contains "compose"
    And the mock podman log contains "up -d"
    And the session file contains "my-feature"

  # Without it there is nothing to say which container the agent belongs in.
  Scenario: a compose config with no service says what to add
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config using docker compose with no service
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "service"
    And the worktree ".am/worktrees/my-feature" does not exist

  Scenario: destroying a compose session takes the whole project down
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config using docker compose
    And a session "my-feature" has been started
    When I run "am destroy my-feature --force"
    Then the command succeeds
    And the mock podman log contains "down -v"

  # initializeCommand runs on the host, outside every boundary am provides.
  Scenario: initializeCommand is refused by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with an initializeCommand
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "runs on your host"
    And the output contains "allow_host_commands"

  # container.mode defaults to "auto", so a repo that describes its environment gets it
  # used with no configuration at all. This is the scenario that would catch the default
  # being flipped back by accident.
  Scenario: a devcontainer config is used with no configuration at all
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start my-feature"
    Then the command succeeds
    And the output contains "from devcontainer.json"

  Scenario: repos without a devcontainer config are unaffected by the default
    Given I am using am's own devcontainer builder
    When I run "am start my-feature"
    Then the command succeeds
    And the session file contains "my-feature"

  # ── am's own builder ────────────────────────────────────────────────────────
  # The point of the native builder: no Node on the machine at all for the common case.

  Scenario: am builds a plain-image config without the CLI
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "from devcontainer.json"
    And the session file contains "my-feature"

  # userEnvProbe defaults to loginInteractiveShell, so a devcontainer that says nothing still
  # gets the environment its dotfiles set up — which is where a devcontainer's toolchain
  # frequently lives.
  Scenario: the agent inherits the shell environment a devcontainer sets up
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "-lic"

  Scenario: userEnvProbe none leaves the environment alone
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config that disables the user env probe
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log does not contain "-lic"

  # An unchanged config must skip the build regardless of which builder produced the image.
  Scenario: a second session on an unchanged config does not rebuild
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start first" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    When I run "am start second" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds

  # A config with nothing to build from is invalid, not unsupported: the reference CLI rejects
  # it too ("No image information specified in devcontainer.json"). Handing it over would ask a
  # second tool the same unanswerable question — and, with no CLI installed, send the user off
  # to install Node over what is usually a typo.
  Scenario: a config with nothing to build from is an error, not a fallback
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with no base image
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "nothing to build from"
    And the output contains "dockerComposeFile"
    And the worktree ".am/worktrees/my-feature" does not exist

  # A Feature vendored in the repo needs no registry at all, so this is the one Feature path
  # that runs end to end with no network — which is why it can live in the ordinary suite.
  Scenario: a feature vendored in the repo is built without the CLI
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config using a local feature
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds

  # The case the old "postAttachCommand is never run" note was about: the session is live, so
  # am only moves tmux focus — but attaching is still attaching, and the hook is due. It is the
  # one lifecycle hook that cannot be chained into a container command, because there is no new
  # container to chain it into.
  Scenario: attaching to a live session runs postAttachCommand in the container
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a postAttachCommand
    And a session "my-feature" has been started
    And the agent pane reports "podman" as its current command
    When I run "am attach my-feature"
    Then the command succeeds
    And the output contains "Attached to session"
    And the mock podman log contains "exec"
    And the mock podman log contains "echo attached"
    # And starting the session ran it too, chained into the container command — starting is
    # also attaching, so both routes to the hook are covered by this one scenario.
    And the mock tmux log contains "echo attached"

  # No hook means no reason to exec into anything, on a path whose whole job is switching to a
  # window that already works.
  Scenario: attaching to a live session with no postAttachCommand execs nothing
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    And a session "my-feature" has been started
    And the agent pane reports "podman" as its current command
    When I run "am attach my-feature"
    Then the command succeeds
    And the mock podman log does not contain "exec"

  Scenario: image mode ignores a devcontainer config entirely
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "image"
    Then the command succeeds

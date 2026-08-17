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

  # `-v` would remove the project's *named* volumes — a database, not scratch space — which
  # outliving a session is the whole point of naming them.
  Scenario: destroying a compose session takes the project down but keeps its named volumes
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config using docker compose
    And a session "my-feature" has been started
    When I run "am destroy my-feature --force"
    Then the command succeeds
    And the mock podman log contains "down"
    And the mock podman log does not contain "down -v"

  # initializeCommand runs on the host, outside every boundary am provides.
  Scenario: initializeCommand is refused by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with an initializeCommand
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "runs on your host"
    And the output contains "allow_host_commands"

  # The opt-in is meant to actually run the command, not just stop refusing the config.
  Scenario: an allowed initializeCommand runs on the host, in the session worktree
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with an initializeCommand that writes a sentinel
    And a project config containing "[devcontainer]\nallow_host_commands = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "running initializeCommand on your host"
    And the file ".am/worktrees/my-feature/.am-initialize-ran" exists
    And the file ".am/worktrees/my-feature/.am-initialize-ran" contains "initialized"

  # A non-zero initializeCommand is a failed preflight step, same as a failed build: the
  # session never starts and the worktree it would have used is not left behind.
  Scenario: a failing initializeCommand stops the session and rolls back the worktree
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with an initializeCommand that fails
    And a project config containing "[devcontainer]\nallow_host_commands = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "initializeCommand"
    And the worktree ".am/worktrees/my-feature" does not exist
    And the session file does not contain "my-feature"

  # runArgs is a runtime-escalation option (alongside privileged, capAdd, securityOpt), gated
  # by its own flag — not the one that gates initializeCommand.
  Scenario: runArgs is dropped by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with runArgs
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "ignoring runArgs"
    And the output contains "allow_runtime_escalation"
    And the mock tmux log does not contain "--network=host"

  Scenario: an allowed runArgs is passed to the container runtime
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with runArgs
    And a project config containing "[devcontainer]\nallow_runtime_escalation = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "--network=host"

  # allow_host_commands alone must not also grant the runtime-escalation options — the whole
  # point of splitting what used to be one flag into three.
  Scenario: allow_host_commands alone does not grant runArgs
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with runArgs
    And a project config containing "[devcontainer]\nallow_host_commands = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "ignoring runArgs"
    And the mock tmux log does not contain "--network=host"

  # The other three runtime-escalation options, each through the public CLI rather than only
  # through apply_trust's return value: the gate is only real if nothing downstream emits them.
  Scenario: privileged is dropped by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config asking for privileged
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "not granting it"
    And the output contains "allow_runtime_escalation"
    And the mock tmux log does not contain "--privileged"

  Scenario: an allowed privileged reaches the container runtime
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config asking for privileged
    And a project config containing "[devcontainer]\nallow_runtime_escalation = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "--privileged"

  Scenario: capAdd is dropped by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config asking for capAdd
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "not granting capabilities"
    And the mock tmux log does not contain "SYS_PTRACE"

  Scenario: an allowed capAdd reaches the container runtime
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config asking for capAdd
    And a project config containing "[devcontainer]\nallow_runtime_escalation = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "SYS_PTRACE"

  # seccomp=unconfined is the example that matters: it removes the syscall filter entirely.
  Scenario: securityOpt is dropped by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config asking for securityOpt
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "not granting security options"
    And the mock tmux log does not contain "seccomp=unconfined"

  Scenario: an allowed securityOpt reaches the container runtime
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config asking for securityOpt
    And a project config containing "[devcontainer]\nallow_runtime_escalation = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "seccomp=unconfined"

  # workspaceMount naming the worktree is the property's whole purpose, and must keep working
  # with no consent at all — a policy that broke it would be a policy nobody could adopt.
  Scenario: a workspaceMount of the session worktree needs no consent
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a workspaceMount of the worktree
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    # The mount, not the workdir: workspaceFolder puts "/workspaces/app" in the command on its
    # own, so asserting on the target alone would pass with the mount dropped. The `:` pins it
    # to a -v argument.
    And the mock tmux log contains ":/workspaces/app:rw"
    And the output does not contain "not mounting"

  # ...and pointing it elsewhere must not be a way around the policy the other mounts go through.
  Scenario: a workspaceMount outside the worktree is dropped by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a workspaceMount outside the worktree
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "not mounting"
    And the mock tmux log does not contain "outside-workspace"

  Scenario: an allowed workspaceMount outside the worktree is honoured
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a workspaceMount outside the worktree
    And a project config containing "[devcontainer]\nallow_host_mounts = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "outside-workspace"

  # The array form is argv, executed without a shell. The sentinel's name carries a `;` so a
  # regression that routes argv through `sh -c` would split it and write a different file.
  Scenario: an argv initializeCommand runs without a shell
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with an argv initializeCommand
    And a project config containing "[devcontainer]\nallow_host_commands = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the file ".am/worktrees/my-feature/argv;ran" exists

  # The object form runs its members concurrently; both must actually run.
  Scenario: a named initializeCommand runs every member
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a named initializeCommand
    And a project config containing "[devcontainer]\nallow_host_commands = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the file ".am/worktrees/my-feature/member-one" exists
    And the file ".am/worktrees/my-feature/member-two" exists

  # One failing member fails the group, names which one, and rolls the worktree back — the
  # sibling member having already run is exactly why the failure must not be swallowed.
  Scenario: a failing member of a named initializeCommand stops the session
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a failing named initializeCommand
    And a project config containing "[devcontainer]\nallow_host_commands = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command fails
    And the output contains "bad"
    And the worktree ".am/worktrees/my-feature" does not exist
    And the session file does not contain "my-feature"

  # A bind mount whose source resolves outside the session worktree is a direct escape from
  # the isolation am exists to provide, gated by its own flag.
  Scenario: a bind mount outside the worktree is dropped by default
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a mount outside the worktree
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "not mounting"
    And the output contains "allow_host_mounts"
    And the mock tmux log does not contain "/host-secret"

  Scenario: an allowed outside-worktree mount is passed to the container runtime
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a mount outside the worktree
    And a project config containing "[devcontainer]\nallow_host_mounts = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the mock tmux log contains "/host-secret"

  # allow_runtime_escalation alone must not also grant an outside-worktree mount.
  Scenario: allow_runtime_escalation alone does not grant an outside-worktree mount
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config with a mount outside the worktree
    And a project config containing "[devcontainer]\nallow_runtime_escalation = true\n"
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    And the output contains "not mounting"
    And the mock tmux log does not contain "/host-secret"

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

  # `onAutoForward: "ignore"` is the one part of portsAttributes that is not about an editor: it
  # asks that the port not be forwarded, and am's forwarding is publishing it.
  Scenario: a port portsAttributes asks to ignore is not published
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config that forwards a port it asks to be ignored
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "devcontainer"
    Then the command succeeds
    # The `podman run` line is what tmux is told to run in the agent pane, so that is where the
    # published ports show up.
    And the mock tmux log contains "127.0.0.1:8080:8080"
    And the mock tmux log does not contain "127.0.0.1:3000:3000"

  Scenario: image mode ignores a devcontainer config entirely
    Given I am using am's own devcontainer builder
    And the repo has a devcontainer config
    When I run "am start my-feature" with env "AM_CONTAINER_MODE" = "image"
    Then the command succeeds

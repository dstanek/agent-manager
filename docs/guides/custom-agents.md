# Custom agents

`am` ships four built-in agents (`claude`, `copilot`, `gemini`, `codex`), but `--agent <name>`
isn't limited to them. It names an entry in `am`'s agent table, and that table is open: add a
`[agents.<name>]` section to config for a tool `am` was never compiled with — Aider, an
in-house agent, anything with a CLI — and it works exactly like a built-in. There are no flags
for defining a one-off agent inline; a custom agent is something you add to config and commit,
not something you retype per invocation.

---

## The minimum: no credentials at all

An agent that needs nothing from the host beyond the command itself only needs `command`:

```toml
# .am/config.toml
[agents.plain]
command = ["plain-agent"]
```

```sh
am start feat --agent plain
```

`am doctor` reports this agent's `credentials` check as `✓ none required` — not a lesser state
than a built-in's, just a true statement that there is nothing to check.

---

## Adding autonomous mode, resume, and credentials

A fuller definition — `aider`, authenticating with an Anthropic API key:

```toml
# .am/config.toml
[agents.aider]
command = ["aider", "--model", "sonnet"]
auto_flags = ["--yes-always"]
resume = ["--restore-chat-history"]

[agents.aider.integration]
env = ["ANTHROPIC_API_KEY"]
hint = "export ANTHROPIC_API_KEY=sk-..."
requires_any = [[{ env = "ANTHROPIC_API_KEY" }]]

[[agents.aider.integration.mounts]]
host = "~/.aider.conf.yml"
container = "~/.aider.conf.yml"
mode = "rw"
required = false
only_if_exists = true
```

- `command` — argv to launch. Required whenever there's no built-in of the same name to inherit
  one from.
- `auto_flags` — appended to `command` under `am start --auto`. Leave the key unset to inherit
  nothing (a fresh custom agent has no autonomous mode by default); set it to an actual list of
  flags to give it one. An **explicit `auto_flags = []`** is itself a meaningful value — it says
  "this agent genuinely has no autonomous mode" — which matters when you're overriding a
  *built-in* that does have one and want to turn it off without deleting the key (see
  [Overriding one field of a built-in](#overriding-one-field-of-a-built-in) below).
- `resume` — argv `am attach` uses to resume the previous conversation. Omit it if the agent has
  no such flag.
- `[agents.aider.integration]` — everything about how the agent authenticates. Omit the whole
  table for an agent that needs nothing from the host.

Inside `integration`:

- `env` lists host environment variables forwarded into the container when set.
- `mounts` (as `[[agents.<name>.integration.mounts]]` entries) mount host credential files or
  directories into the container. `mode` is `"ro"` (default) or `"rw"`; `required` (default
  `true`) fails preflight when the host path is missing; `only_if_exists` (default `false`)
  skips the mount instead of letting the container runtime create it root-owned on the host.
- `requires_any` is what `am doctor`/`am setup` check to decide the agent is authenticated: an
  OR of ANDs — a list of requirement groups, and *any one* group being *fully* satisfied counts.
  Most agents need one way to sign in, so most `requires_any` values are a single one-element
  group, as above. An agent with two independent ways to authenticate (e.g. codex's built-in:
  an interactive sign-in *or* an API key) uses two groups:

  ```toml
  requires_any = [
    [{ path = "~/.aider/auth.json" }],
    [{ env = "ANTHROPIC_API_KEY" }],
  ]
  ```

  Each requirement is `{ path = "~/..." }` (host path exists) or `{ env = "NAME" }` (host env
  var is set and non-empty) — never both in the same table.
- `hint` is the fix `am doctor` prints when nothing in `requires_any` is satisfied — make it a
  concrete command, e.g. a sign-in instruction or the variable to export.

See the [configuration reference](../reference/configuration.md#custom-agents) for the full key
list and every field's default.

---

## Checking your definition

`am doctor` resolves and checks whatever `defaults.agent` currently names, so point it at your
new agent (temporarily, or via `--agent` on `am setup`) and read the `Agent` section of the
report:

```sh
am doctor
```

```
Agent
  ✓ agent                  aider
  ✗ credentials            agent 'aider' has no credentials
      → export ANTHROPIC_API_KEY=sk-...
```

A typo in `requires_any`, a bad host path, or an invalid mount `mode` is caught here — at
`am doctor`/`am start` time, not silently. A relative host path (missing the leading `~/` or
`/`) is rejected outright: `host path 'creds' must start with '~/' or '/'`.

Once credentials check out:

```
Agent
  ✓ agent                  aider
  ✓ credentials            present
```

`am setup`'s agent menu lists your custom section too, after the four built-ins, alphabetically
sorted, with the same `credentials found` / blank / `no integration` markers `am doctor` uses.

---

## A container image

Custom agents need somewhere to run just like built-ins do. Set an image the same way:

```toml
[agents.aider]
command = ["aider", "--model", "sonnet"]
image = "ghcr.io/your-org/am-aider:latest"
```

or point `container.mode` at your own `.devcontainer/devcontainer.json` instead of an `am`-built
image — see [Dev Containers](devcontainers.md). If the agent has a published Dev Container
Feature, map it with `devcontainer_feature` the same way a built-in does.

---

## Overriding one field of a built-in

`[agents.<name>]` doesn't just define new agents — for a name that already has a built-in
(`claude`, `copilot`, `gemini`, `codex`), it overlays the fields you set and leaves the rest
alone. For example, to run Claude Code without `--dangerously-skip-permissions` even under
`--auto`:

```toml
[agents.claude]
auto_flags = []
```

Every other field of the `claude` built-in — its `command`, `resume`, and full credential
`integration` — is untouched. Setting `[agents.claude].integration`, however, replaces the
built-in's integration wholesale rather than merging into it: there's no way to add one extra
mount while keeping the rest, only to restate the whole thing.

---

## Starting a session

```sh
am start feat --agent aider
```

Once `[agents.aider]` exists in either your project or global config, `--agent aider` works
everywhere an agent name does: `am start`, `defaults.agent`, `am setup --agent aider`, and
`am doctor`.

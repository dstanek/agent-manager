# Feature: Decouple Command, Integration, and Image

## Background

From `BACKLOG.md`, "Architecture Audit Follow-ups" → "Decouple command, integration, and
image (highest priority)":

> Today a single `--agent` string means three things at once: the command that launches
> (`main.rs` appends it as the container CMD), the auth preset (`container.rs::resolve_agent_auth`),
> and the image (`config::resolve_image` via `[agents.<name>]`). `KnownAgent::parse` rejects
> any name outside `claude|copilot|gemini|codex` — even with `--no-container` — so there is no
> path to "run this image, mount these creds, exec this command."

Concretely: `cmd_start` (`src/main.rs:646`) computes one string, `effective_agent`
(`src/main.rs:674`), and immediately parses it into one `KnownAgent`, `effective_known_agent`
(`src/main.rs:689-692`), via `.transpose()?` — a hard error for any name outside the four
built-ins, unconditionally, even with `--no-container`. That single `KnownAgent` value then
drives three unrelated things: the literal command exec'd in the container/pane
(`agent_command`, `src/main.rs:1366`, which just echoes back the same string), the auth preset
(`container::resolve_agent_auth`, `src/container.rs:490`, keyed on the `KnownAgent` value), and
the image (`config::resolve_image`, `src/config.rs:293`, keyed on the same string via
`[agents.<name>]`). Naming your command is naming your credentials is naming your image, and
none of the three can vary independently.

**Correction already recorded in the backlog, not re-derived here:** this was originally
logged as blocking Dev Container Support and turned out not to be. Devcontainer mode never
calls `config::resolve_image` — that function has exactly one caller, `plan_image`
(`src/main.rs:1039`), which devcontainer sessions never reach (`plan_container`,
`src/main.rs:991`, routes to `plan_devcontainer` instead whenever a config is discovered). So
`--agent claude` already stops implying an image on that path today; this spec does not touch
that fact.

This unblocks the adjacent backlog item, **"Custom-harness fast path"**:
`am start idea --image my-image --cmd my-agent`, no built-in integration required. This spec
treats that as the acceptance target and ships it in the same change — see
[Resolved Decisions](#resolved-decisions) #1.

## Assumptions

- **A1.** "Integration" means exactly what `container.rs` already calls the six behaviors
  keyed on `KnownAgent` today: credential mounts, extra env, credential presence-validation,
  the credentials hint, auto-mode flags, and resume flags. Nothing new is being invented here
  — see [Design](#design-the-coupling-is-narrower-than-it-looks).
- **A2.** "Image" in this spec means only the `container.image` / `[agents.<name>].image`
  axis (`plan_image`'s world). Devcontainer mode's environment still comes entirely from the
  repo's own config, per the correction above; this spec does not add an `--image` escape
  hatch that fights with `plan_devcontainer` — `--image` is documented as inert whenever
  `container.mode` resolves to `devcontainer`, the same way `container.image` already is today
  (see [Open Questions](#open-questions), OQ-4).
- **A3.** `am run <slug> <agent>`'s existing `agent: String` CLI argument (`src/cli.rs:94`,
  no `value_parser`) already *is* an unvalidated command string, persisted onto
  `Session.agent` verbatim — `attach-restore-agent.md`'s own data model section
  (`src/session.rs:315-329`) says so explicitly. `am run` therefore already models "Command"
  correctly today; the only gap this spec closes for it is giving it a way to say *which*
  integration (if any) that command belongs to, instead of inferring it by re-parsing the
  string — see [`am run` impact](#am-run-impact).
- **A4.** No change to the tmux/container mount, network, or SELinux-labeling machinery in
  `container.rs`. Every one of its public functions already takes `Option<KnownAgent>` and
  nothing else agent-related; this spec adds no new parameter to any of them.

## Design: the coupling is narrower than it looks

Read literally, "introduce independent concepts: command, integration, image" sounds like it
touches everywhere `KnownAgent` appears — the raw grep counts the team-lead supplied
(`container.rs` 94, `onboarding.rs` 86, `main.rs` 34, `doctor.rs` 2, `cli.rs` 1) make it look
like a 200+ site rewrite. It is not, and the reason is worth stating plainly because it is the
whole design:

**`KnownAgent` already means "integration," everywhere it is used, except at the one place
it gets constructed.** Look at what it actually parameterizes: `resolve_agent_auth_mounts`,
`agent_extra_env` (the `env` half of `resolve_agent_auth`, `src/container.rs:490`),
`validate_agent_credentials`, `credentials_hint`, `agent_auto_flags`, `agent_resume_flags` —
every one of the six behaviors named in the task — plus `agent_command`'s own `known` parameter
(`src/main.rs:1366`) and `resume_will_apply`'s `known_agent` parameter
(`src/main.rs:1647`, used only to decide whether the "(resuming)" wording applies). None of
these ever look at the *literal command string* — `agent_command` takes it as a wholly separate
`agent_name: Option<&str>` parameter already, and just echoes it into `cmd[0]`. The type was
never wrong for what it does; `onboarding.rs`'s 86 references are the `am setup` menu picking
one of four known integrations and writing `defaults.agent` — also already correct, because
`am setup` only ever offers presets (see [`am setup` impact](#am-setup-impact)).

The actual bug is narrower: **there is exactly one way to produce a `KnownAgent` value today
— parse the command string — and exactly one way to produce a command string that `am`
actually launches — pass it as `--agent`, which requires it to parse as a `KnownAgent`.**
`effective_known_agent = effective_agent.map(KnownAgent::parse).transpose()?`
(`src/main.rs:689-692`) is the coupling, and it is three lines. Fix those three lines' worth of
*resolution logic* — give command and integration each a second, independent way to be
supplied — and the six behaviors, `agent_command`, `resume_will_apply`, `plan_container`, and
essentially all of `container.rs` and `onboarding.rs` need no interface change at all. They
already do the right thing once fed the right values.

What *does* need real, non-mechanical work, concentrated almost entirely in `main.rs` and
`config.rs`:

| File | Raw `KnownAgent` refs | What actually changes |
|---|---|---|
| `src/container.rs` | 94 | **None.** Every function already takes `Option<KnownAgent>` meaning "integration." Doc-comment wording only (a handful of `agent` → `integration` in prose, not code). |
| `src/onboarding.rs` | 86 | **None.** `am setup` stays preset-only — see [`am setup` impact](#am-setup-impact). Same reasoning: it already only ever selects an integration and writes `defaults.agent`. |
| `src/main.rs` | 34 | **Real.** This is where command/integration/image get resolved and dispatched: `cmd_start`, `cmd_attach`, `cmd_run`, `plan_container_runtime`/`plan_image`/`plan_devcontainer`'s parameter docs, `agent_command`'s doc comment, `injected_features` (one real bug fix — see below), `print_next_steps`. |
| `src/config.rs` | 0 direct (via `resolve_image`/`resolve_agent_feature`) | **Real.** New `Config.command`/`Config.integration` fields, a new `resolve_launch` helper, `resolve_agent_feature`'s key changes from command to integration (bug fix). |
| `src/doctor.rs` | 2 | **Real but small.** `check_agent`/`check_image_mode` take the resolved integration/image instead of re-deriving them. |
| `src/cli.rs` | 1 | **Real but small.** Three new flags on `Start`, one new flag on `Run`. |
| `src/session.rs` | 0 | **Real but small.** One new `Option<String>` field, `#[serde(default)]`. |
| `src/error.rs` | 0 | **Real but small.** Two error variants renamed for accuracy, no behavior change. |

This is the load-bearing claim of the whole spec, so it is worth being explicit about what
would falsify it: if `container.rs` or `onboarding.rs` turn out to need a real interface
change during implementation (not just a doc-comment reword), that is a sign this design
document was wrong about the boundary and needs revisiting before continuing, not a sign to
push through it.

## The three concepts

- **Command** — the literal argv[0] exec'd in the agent pane or as the container CMD.
  Free-form string, never validated against anything. Already modeled correctly today as
  `Session.agent: Option<String>` and `agent_name: Option<&str>` throughout `main.rs`; this
  spec gives it a second source (`--cmd`) beyond the `--agent` shorthand.
- **Integration** — which auth preset (if any) applies: credential mounts, extra env,
  credential-presence validation, the doctor hint, auto-mode flags, resume flags, and (new —
  see below) which devcontainer Feature installs the agent. Already modeled correctly today as
  `container::KnownAgent`; this spec gives it a second source (`--integration`) beyond parsing
  the command string.
- **Image/profile** — the container image to run, in image mode only (A2). Already modeled as
  `config::resolve_image`, keyed on a name; this spec (a) gives it a third source, `--image`,
  above `container.image` and `[agents.<name>].image`, and (b) fixes what that key means — see
  [Devcontainer agent-Feature injection](#devcontainer-agent-feature-injection-keyed-by-integration-not-command).

## Resolution model

Three independent axes, each resolved by "most specific CLI flag beats less specific CLI flag
beats most specific config key beats less specific config key beats nothing." `--agent` and
`defaults.agent` are the *shorthand* tier on two axes at once (command and integration); `--cmd`
/ `defaults.command` and `--integration` / `defaults.integration` are the *explicit* tier, new
in this spec, and always win when present.

**Command:**

1. `--cmd <string>` (new, CLI) — never validated.
2. `--agent <name>` (existing, CLI) — validated via `KnownAgent::parse`; used as the literal
   command string too, exactly as today.
3. `defaults.command` (new, config) — never validated.
4. `defaults.agent` (existing, config) — validated, same as `--agent`.
5. `None` — legal. A container starts with the image's own `ENTRYPOINT`/`CMD`; a host session
   opens the window with nothing launched, exactly like today's "no agent configured" state.

**Integration:**

1. `--integration <name>` (new, CLI) — validated via `KnownAgent::parse`; an unrecognized name
   is a hard error (typo protection — this is the one place in the whole design that still
   errors on an unknown name, and deliberately so, since the user asked for a specific preset
   by name).
2. `--agent <name>` (existing, CLI), if it parses as a `KnownAgent` — it always does, since
   `--agent` is still fully validated (see [Resolved Decisions](#resolved-decisions) #2).
3. `defaults.integration` (new, config) — validated the same way as `--integration`.
4. `defaults.agent` (existing, config), if it parses.
5. `None` — legal, and the entire point of the custom-harness fast path. No mounts, no extra
   env beyond `container.env`, no credential validation, no auto flags, no resume flags, no
   devcontainer Feature injected for it.

**Image** (image mode only — A2):

1. `--image <string>` (new, CLI).
2. `container.image` (existing, config) — unchanged behavior and precedence.
3. `[agents.<integration>].image` (existing, config) — **now keyed by the resolved
   *integration*, not by the resolved command** (see the Feature-injection bug fix below for
   why this distinction matters and was already latent).
4. `None` — legal only when no image is required (devcontainer mode, or `--no-container`).
   Otherwise `AmError::ContainerImageNotConfigured`, message updated to mention `--image`.

Worked example, the wrapper case this design exists to support: `--agent claude --cmd
my-claude-wrapper`. Command = `my-claude-wrapper` (tier 1). Integration = `claude` (tier 2, `
--agent` parses). Image = `[agents.claude].image` (tier 3) — claude's credentials get mounted,
claude's compiled-in image is used, and the container's CMD is the wrapper script, not
`claude` itself.

Worked example, the acceptance target: `am start idea --image my-image --cmd my-agent`.
Command = `my-agent`. Integration = `None` (nothing at any tier parses or is given). Image =
`my-image` (tier 1). No credential preflight, no mounts beyond `container.env`, no auto/resume
flags ever appended regardless of `--auto`/`--fresh`.

## Data model

### `Config` (`src/config.rs:219-233`)

Two new fields, both additive, both following the existing `agent`/`FileDefaults` pattern
(`src/config.rs:307-312`) exactly:

```rust
pub struct Config {
    pub agent: Option<String>,          // unchanged — the shorthand
    pub command: Option<String>,        // NEW — explicit command override
    pub integration: Option<String>,    // NEW — explicit integration override
    pub agents: HashMap<String, AgentSettings>,  // unchanged in shape
    // ...
}
```

`[defaults]` in the TOML file gains two optional keys alongside the existing `agent`:

```toml
[defaults]
agent = "claude"        # shorthand: sets command + integration + image-lookup-key together
# command = ""           # explicit override: what to exec (independent of `agent`)
# integration = ""       # explicit override: which auth preset to use (independent of `agent`)
```

No key is renamed, no key is removed, no existing key's meaning changes for a config that only
ever sets `agent` — which is every config in the wild today. `[agents.<name>]` and
`container.image` are untouched in shape; only what `<name>` is *resolved from* changes (see
Resolution model above).

### `resolve_launch` (new, `src/config.rs`, beside `resolve_image`)

The one place all three call sites — `cmd_start`, `cmd_attach`'s OQ-1-style legacy fallback,
and `doctor::run` — get command/integration/image, so they cannot drift apart from each other
the same way `am setup`'s verification step already shares logic with `am doctor`
(`specs/guided-setup.md`, "Relationship to `am doctor`").

```rust
pub struct LaunchFlags<'a> {
    pub agent: Option<&'a str>,
    pub cmd: Option<&'a str>,
    pub integration: Option<&'a str>,
    pub image: Option<&'a str>,
}

pub struct ResolvedLaunch {
    pub command: Option<String>,
    pub integration: Option<container::KnownAgent>,
    pub image: Option<String>,  // image mode only; None in devcontainer mode regardless of input
}

/// Resolve the three independent axes per the precedence in this spec. Errors only when
/// `flags.integration` (or `cfg.integration`) names something `KnownAgent::parse` rejects —
/// every other axis degrades to `None` rather than erroring, which is what makes an unknown
/// `--cmd` value a first-class supported case instead of a typo to catch.
pub fn resolve_launch(flags: LaunchFlags, cfg: &Config) -> Result<ResolvedLaunch>;
```

### `Session` (`src/session.rs:110-131`)

One new field, mirroring exactly how `attach-restore-agent.md` added `agent` itself:

```rust
pub struct Session {
    // ...existing fields...
    pub agent: Option<String>,  // unchanged in name and meaning: the command last launched
    /// The integration (if any) resolved when `agent` was last launched — used by `am attach`
    /// to look up resume/auto flags without re-parsing `agent` as a `KnownAgent`, which would
    /// silently misfire for a custom command wrapping a known integration (`agent =
    /// "my-claude-wrapper"`, `integration = "claude"`). `None` for records written before this
    /// field existed, or for a launch that genuinely had no integration.
    #[serde(default)]
    pub integration: Option<String>,
}
```

**Backward compatibility for reads.** A record with no `integration` key deserializes as
`None`. `am attach`'s existing legacy-fallback shape (`src/main.rs:1960-1979`, OQ-1 in
`attach-restore-agent.md`) already re-derives `known_agent` by parsing `s.agent` when nothing
better is available — extend that fallback one step: when `s.integration` is `None`, fall back
to `KnownAgent::parse(s.agent).ok()`, exactly what the code does today, and persist the result
onto `s.integration` the same way OQ-1 already persists a recovered `s.agent` — so the
inference runs at most once per legacy record, same idiom, same file.

`SessionContainer` (`src/session.rs:33-61`) needs no changes — nothing about which image was
used, or the devcontainer identity, depends on command vs. integration; it already records
`image`/`config_hash`/`remote_user` as opaque strings.

## CLI contract

### `am start <slug>` (`src/cli.rs:53-69`)

Three new flags, all optional, all combinable with the existing `--agent`:

```rust
Start {
    slug: String,
    #[arg(short, long)]
    agent: Option<String>,        // unchanged
    #[arg(long)]
    cmd: Option<String>,          // NEW — explicit command, never validated
    #[arg(long)]
    integration: Option<String>,  // NEW — explicit integration, validated (typo → error)
    #[arg(long)]
    image: Option<String>,        // NEW — explicit image override, image mode only
    #[arg(long)]
    no_container: bool,           // unchanged
    #[arg(long)]
    auto: bool,                   // unchanged
    #[arg(long)]
    rebuild: bool,                // unchanged
}
```

Validation: `--integration <name>` where `KnownAgent::parse(name)` fails is a hard error
naming the four valid integrations, same wording `--agent`'s error already uses
(`src/container.rs:49-60`). `--cmd` and `--image` accept anything; nothing about the custom
command or the custom image is ever validated by `am` — that is the entire point of the
harness-agnostic path, and it mirrors `container.image`'s existing no-validation contract.

### `am run <slug> <command> [--integration <name>]`

```rust
Run {
    slug: String,
    agent: String,                 // unchanged in name and behavior — still unvalidated
    #[arg(long)]
    integration: Option<String>,   // NEW, optional
}
```

Behavior: `s.agent = Some(command)` (unchanged). `s.integration` is set to
`integration.map(|i| KnownAgent::parse(&i)).transpose()?.map(|k| k.to_string())` when the flag
is given (validated — same typo-protection reasoning as `am start --integration`); when the
flag is *not* given, `s.integration = KnownAgent::parse(command).ok().map(|k| k.to_string())`
— i.e., exactly today's implicit "does the command name happen to match a known integration"
inference, preserved as the default so `am run feat codex` keeps working with zero flags, same
as before this spec. The flag exists only for the case that inference gets wrong: a wrapper
script whose name doesn't match any `KnownAgent` variant.

### `am attach <slug> [--fresh]` — no new flags

Unchanged surface. `am attach` never lets you pick a different command/integration for a
session — `am run` is still the dedicated tool for that (A3's reasoning, and
`attach-restore-agent.md`'s own A5, both carry over unchanged).

### `am doctor` — no new flags

Deliberately. See [`am doctor` impact](#am-doctor-impact) for why the existing zero-argument
surface is enough.

## Per-behavior dispatch

The six behaviors the task names, and what each is keyed on after this change — column three
is the point: every single one is unchanged, because every single one was already keyed on
`KnownAgent`/integration, never on the command string.

| Behavior | Location | Keyed on | Changed? |
|---|---|---|---|
| Credential mounts | `resolve_agent_auth_mounts`, `src/container.rs:326` | `KnownAgent` (integration) | No |
| Extra env | `resolve_agent_auth`'s `env` field, `src/container.rs:490` | `KnownAgent` (integration) | No |
| Credential validation | `validate_agent_credentials`, `src/container.rs:565` | `KnownAgent` (integration) | No |
| Credentials hint | `credentials_hint`, `src/container.rs:607` | `KnownAgent` (integration) | No |
| Auto-mode flags | `agent_auto_flags`, `src/container.rs:406` | `KnownAgent` (integration) | No |
| Resume flags | `agent_resume_flags`, `src/container.rs:430` | `KnownAgent` (integration) | No |
| Devcontainer Feature injection | `resolve_agent_feature`, `src/config.rs:280` via `injected_features`, `src/main.rs:1397` | **command today, integration after this change** | **Yes — see below** |

## Devcontainer agent-Feature injection: keyed by integration, not command

`injected_features` (`src/main.rs:1397-1420`) calls
`config::resolve_agent_feature(agent_name, cfg)` — today `agent_name` is the same string as
the integration, so this has never visibly misbehaved. Once command and integration can differ
(`--agent claude --cmd my-claude-wrapper`), keying on command would look up
`cfg.agents.get("my-claude-wrapper")`, find nothing, and inject no Feature — even though the
session is unambiguously "claude, launched via a wrapper" and should get
`ghcr.io/anthropics/devcontainer-features/claude-code:1` baked in exactly as a bare `--agent
claude` would. `injected_features` and `resolve_agent_feature`'s call sites in `doctor.rs`
(`src/doctor.rs:725-744`, mirrored to keep image-currency checks from drifting from
`am start`) switch to the resolved *integration name* — `resolved.integration.map(|k|
k.to_string())` — not the resolved command. This is a real (if currently unobservable) latent
bug, fixed as a required part of this change, not an optional drive-by.

## The `agent_auto_flags`-before-`agent_resume_flags` ordering bug

`agent_command` (`src/main.rs:1366-1394`) already carries an in-code comment on this: auto
flags are appended before resume flags, which is harmless today only because no agent combines
a non-empty `agent_auto_flags` with a subcommand-shaped resume form (Codex's `resume --last`
must be the first token). `agent_command`'s signature is already being touched by this change
(its doc comment needs updating for the command/integration split regardless), so fixing the
ordering — append resume flags first when the resume form is subcommand-shaped, or more simply,
special-case "resume flags whose first element is not a `-`-prefixed flag go first" — costs
nothing extra on top of work already required and removes a landmine for the next agent
integration. **Recommendation: fix it as part of this change**, since the alternative is
touching the same function's signature twice (once here, once whenever the ordering bug is
finally addressed) for no reason.

## Use-Cases

### UC-1: `--agent claude` — the shorthand, unchanged

**Actor:** any existing user. **Preconditions:** none beyond today's. **Main flow:** `am start
feat --agent claude` behaves byte-for-byte as it does before this change — command =
integration = image-lookup-key = `"claude"`, all three resolved from tier 2 of their
respective axes. **Postconditions:** identical to today. **Business rule:** this is the
regression the entire task is scoped around not breaking; it must be pinned by tests, not just
argued.

### UC-2: Custom harness, the acceptance target

**Actor:** a user with their own agent CLI and their own image, no built-in integration.
**Preconditions:** an image exists (built elsewhere, or via `--rebuild`-independent means) that
contains the `my-agent` binary. **Main flow:**

1. `am start idea --image my-image --cmd my-agent`.
2. Command resolves to `my-agent` (tier 1), integration resolves to `None` (nothing supplies
   it), image resolves to `my-image` (tier 1).
3. No credential preflight runs (`plan_image`'s `agent_auth = match agent { Some(_) => ...,
   None => AgentAuth::default() }`, `src/main.rs:1054-1057`, already handles `None` correctly —
   no code change needed here, just correct input).
4. Container starts with `my-image`, mounts the worktree/VCS/gitconfig/ssh exactly as any
   session does, sets `container.env`, and execs `my-agent` as CMD.

**Postconditions:** a running session with no `am`-managed credentials and no devcontainer
Feature injection — the user's image is fully responsible for having `my-agent` and whatever
it needs already baked in. **Business rules:** `--image` and `--cmd` never validate their
argument's existence — the runtime's own "image not found" / "exec format error" is the
failure mode, exactly as it already is for `container.image` today.

### UC-3: Wrapper around a known integration

**Actor:** a user who wants Claude's credentials and default image, but launches through their
own wrapper script instead of the bare `claude` binary. **Main flow:** `am start feat --agent
claude --cmd ./scripts/claude-with-logging.sh`. Command = the wrapper (tier 1, `--cmd` beats
`--agent`'s shorthand-as-command). Integration = `claude` (tier 2, `--agent` still supplies
it). Image = `[agents.claude].image` (tier 3, from the integration). **Postconditions:**
Claude's credentials are mounted, Claude's devcontainer Feature is injected if devcontainer
mode applies, and the container's CMD is the wrapper script, which presumably `exec`s `claude`
itself eventually.

### UC-4: `am run` with a command that isn't a recognized integration

**Actor:** a user with an already-running session who wants to launch a one-off custom command
into the agent pane. **Main flow:** `am run feat ./scripts/my-tool.sh`. `s.agent =
Some("./scripts/my-tool.sh")`; since `--integration` wasn't given and the string doesn't parse
as a `KnownAgent`, `s.integration = None`. **Postconditions:** a subsequent `am attach feat`
relaunches `./scripts/my-tool.sh` with no resume/auto flags — see UC-5.

### UC-5: `am attach` relaunching a custom command with no integration

**Actor:** a user whose machine rebooted, mid-session on UC-2's or UC-4's setup.
**Preconditions:** `Session.agent = Some("my-agent")`, `Session.integration = None`, tmux
window gone. **Main flow:** identical to `attach-restore-agent.md`'s UC-1/UC-2, except every
integration-gated step is a no-op: `known_agent` resolves to `None` (from `s.integration`, not
by re-parsing `s.agent`), so `agent_command`'s `auto`/`resume` branches never execute
(`src/main.rs:1384-1391`, both gated on `if let Some(agent) = known`), `preflight_agent_auth`
is never called for the container-recreate path, and `resume_will_apply(None, resume)` returns
`false`. **Postconditions:** `Opened new window for session 'idea' and relaunched 'my-agent'.`
— no `(resuming)` suffix, because there is nothing to resume and `am` never claims otherwise.
**Business rule, directly answering the team-lead's question:** auto mode and resume both do
*nothing* for a custom command with no integration, and say so by simply omitting the
resuming/auto language from the success line rather than printing an error — this already
falls out of the existing gating with zero new code, once `known_agent` is sourced correctly.

### UC-6: `am doctor` / `am setup --agent` on a custom-command config

**Actor:** a user running `am doctor` (or `am setup`'s verification step) against a config that
sets `defaults.command`/`defaults.integration` by hand, or a fresh repo with no config at all
using `--cmd`/`--image` at the `am start` call site (doctor cannot see per-invocation flags —
see [`am doctor` impact](#am-doctor-impact)). **Main flow:** `check_agent` reports "command:
my-agent" and "integration: none — no credential checks apply" as an **ok**, not a warning —
this is a fully supported, intentional configuration, and warning on it would contradict the
entire point of the feature. `check_image_mode` reports whatever `container.image`/the
integration-keyed lookup resolves to, unchanged in shape.

### UC-7: Devcontainer mode with an integration but a custom command

**Actor:** UC-3's user, in devcontainer mode. **Main flow:** `injected_features` resolves the
Feature by integration (`claude`), so `ghcr.io/anthropics/devcontainer-features/claude-code:1`
is injected regardless of what `--cmd` was; the built image's CMD is still the wrapper script.
**Postconditions:** the image contains both `claude` (from the Feature) and whatever the
wrapper needs (from the base image or other Features) — `am` never inspects whether the wrapper
actually calls `claude`.

### UC-8: A typo in `--integration` errors; a typo in `--cmd` does not

**Actor:** a user who mistypes. **Main flow A:** `am start feat --integration cladue` — hard
error, "unknown agent 'cladue' — valid agents are: claude, copilot, gemini, codex" (reusing
`KnownAgent::parse`'s existing message verbatim). **Main flow B:** `am start feat --cmd
my-agnet` — no error; a container starts and execs a binary named `my-agnet`, which the runtime
then reports missing. **Business rule:** this asymmetry is intentional, not an oversight — see
[Resolved Decisions](#resolved-decisions) #2.

## `am doctor` impact

**No CLI surface change.** `am doctor` continues to report the *configured* (file-based) state
— `resolve_launch(LaunchFlags { agent: agent_flag, cmd: None, integration: None, image: None
}, &cfg)`, where `agent_flag` is the existing `Option<&str>` parameter `doctor::run` already
takes (`src/doctor.rs:196`), used only by `am setup`'s verification call to preview an
in-progress choice (`specs/guided-setup.md`, "Verification step"). A per-invocation `--cmd`/
`--integration`/`--image` triple passed to `am start` is invisible to `am doctor` by design,
the same way a bare `am start --agent codex` today is invisible to a doctor run that doesn't
pass `--agent` — `am doctor` has always answered "is the *persisted config* ready," not "would
this specific command line work." Extending `am doctor` with its own `--cmd`/`--integration`/
`--image` flags for symmetry is real, small, and deliberately left as a follow-on — see
[Open Questions](#open-questions), OQ-1.

`check_agent` (`src/doctor.rs:746-782`) and `check_image_mode` (`src/doctor.rs:576-589`) both
take `resolved.command`/`resolved.integration` instead of re-deriving them from a single
`agent_name: Option<&str>` — small signature change, no behavior change for any config that
only ever sets `defaults.agent` (every config today). New behavior only for a config that sets
`defaults.command`/`defaults.integration` explicitly: `check_agent` reports command and
integration as two separate lines instead of one, and treats `integration: none` as **ok**,
never **fail** or **warn** (see UC-6). `check_image_mode`'s fail hint gains a mention of
`--image`: `"...or set defaults.agent = \"...\" in .am/config.toml, or set container.image /
defaults.command + defaults.integration + container.image for a custom harness"`.

## `am setup` impact

**Deliberately preset-only — no change to the guided flow.** `am setup`'s agent menu
(`onboarding.rs:29-33`'s `MENU` constant, `ask_agent`, `src/onboarding.rs:593`) continues to
offer exactly the four known integrations and write `defaults.agent`, unchanged. This is not a
gap being left unaddressed; it is the same boundary `guided-setup.md` already drew and
justified for its own scope ("`am setup` doesn't replace those workflows or grow into a general
config editor" — Assumptions, and Resolved Decisions #4 there): a wizard that walks a
first-time user through *four Enter-key defaults* is the wrong tool for "type your own image
name and your own command," which is an advanced, scripting-flavored flow by construction —
the same "fast supported-integration path vs. advanced/custom-harness path" split
`BACKLOG.md`'s own "Docs" follow-up item already calls for. A user who wants the custom-harness
path already knows enough to type `--image`/`--cmd` on a command line; asking `am setup` to
prompt for an arbitrary image string with no validation, no autocomplete, and no way to check
it works before `am start` actually runs it would be worse UX than just running `am start`
directly. **This is why `onboarding.rs`'s 86 references need no interface change** — the table
in [Design](#design-the-coupling-is-narrower-than-it-looks) already accounts for this.

## `am run` impact

Covered in [CLI contract](#am-run-slug-command---integration-name) and A3/UC-4 above. Net
effect: `am run` gains one optional flag and otherwise behaves identically, including for every
existing test that doesn't pass it.

## `am attach` impact

Covered in UC-5. The only functional change to `cmd_attach` (`src/main.rs:1943-2006`) is
sourcing `known_agent` from `s.integration` (falling back to parsing `s.agent`, per the Session
data-model section) instead of always parsing `s.agent`. Everything downstream —
`recreate_attach_window`, `relaunch_into_existing_window`, `agent_pane_status`,
`run_post_attach` — is unaffected, because all of them already take `known_agent:
Option<KnownAgent>` as an opaque input.

## Resolved Decisions

1. **The custom-harness fast path ships in the same change as the decoupling, not as a
   follow-on.** Once command/integration/image are resolved independently inside `main.rs`,
   exposing that as `--cmd`/`--integration`/`--image` is three `clap` fields and wiring them
   into `resolve_launch` — a few hours of the same PR, not a separately schedulable project.
   Shipping the decoupling without any CLI surface to exercise it would leave the refactor
   internally correct but externally unverifiable end-to-end (no cucumber scenario could
   reach the "integration is `None`" branch), which is precisely the branch this whole change
   exists to make safe. The alternative — landing `resolve_launch`/`Session.integration`/the
   Feature-injection fix now and the three flags later — saves nothing and defers the only
   test coverage that proves the design works.

2. **`--agent`/`defaults.agent` keep today's strict `KnownAgent::parse` validation; they are
   not relaxed to accept arbitrary names.** Considered and rejected: making `--agent
   my-custom-thing` succeed (as command-only, integration `None`) would let the new
   harness-agnostic path piggyback on the existing flag instead of adding `--cmd`. Rejected
   because `--agent`'s current hard error is real, valuable typo protection for the
   overwhelmingly common case (a user meant one of the four built-ins and fat-fingered it), and
   there is no way to keep that protection while also accepting arbitrary strings on the same
   flag — the two goals are in direct conflict on one axis. `--cmd` is new, unvalidated, and
   named differently on purpose, so a user who wants "no validation" has to opt into it
   explicitly rather than lose a safety net they didn't ask to give up. This also happens to
   match the acceptance target's own spelling (`--image ... --cmd ...`, not `--agent
   my-agent`).

3. **`KnownAgent` is not renamed in this change.** Considered `KnownAgent` → `KnownIntegration`
   for clarity, since after this change the type unambiguously means "integration" and never
   "command." Rejected for *this* change: it is a mechanical, behavior-preserving rename
   touching the 94 + 86 + 34 + 2 + 1 = 217 references the team-lead counted, and bundling it
   with the actual logic change (a few hundred lines in `main.rs`/`config.rs`) makes the real
   diff harder to review for no functional benefit — a renamed-but-unchanged 94-site file
   reviews identically to an unchanged one, badly. Left as an optional, purely mechanical
   follow-up (a single `cargo`-verified rename), not blocking, not part of this spec's task
   list. See [Open Questions](#open-questions), OQ-2, for whether to schedule it at all.

4. **`resolve_launch` lives in `config.rs`, not `container.rs` or a new module.** `config.rs`
   already owns `resolve_image` and `resolve_agent_feature`, both of which `resolve_launch`
   subsumes the precedence logic for; keeping resolution logic in one file next to the config
   structs it reads is the existing pattern (`config::resolve_image`'s own doc comment already
   states its precedence order the same way this spec states `resolve_launch`'s).

5. **Devcontainer Feature injection is fixed to key on integration, not command, as a required
   part of this change, not an optional drive-by.** See the dedicated section above. Left
   unfixed, it would be a regression introduced by this spec (today it's merely
   coincidentally-correct, not tested-and-correct) rather than a pre-existing gap this spec
   declines to close.

6. **The `agent_auto_flags`/`agent_resume_flags` ordering bug is fixed as part of this change.**
   See the dedicated section above — `agent_command`'s signature is already being touched, so
   the marginal cost is near zero and the alternative touches the same function twice for no
   reason.

## Task breakdown

### backend-engineer

- [ ] `src/config.rs`: `Config.command`/`Config.integration` fields (`#[serde(default)]` via
      the existing `FileDefaults` pattern), `LaunchFlags`/`ResolvedLaunch`/`resolve_launch` per
      [Data model](#resolve_launch-new-srcconfigrs-beside-resolve_image); `resolve_agent_feature`
      keyed on integration.
- [ ] `src/session.rs`: `Session.integration: Option<String>`, `#[serde(default)]`; update
      `make_session`/test helpers; roundtrip test with the field set and a legacy-record test
      confirming a missing key loads as `None` (mirror the existing `agent` field's tests).
- [ ] `src/cli.rs`: `--cmd`/`--integration`/`--image` on `Start`; `--integration` on `Run`.
- [ ] `src/main.rs`: thread `resolve_launch`'s output through `cmd_start` (replacing
      `effective_agent`/`effective_known_agent`'s direct derivation), `cmd_run` (per
      [CLI contract](#am-run-slug-command---integration-name)), and `cmd_attach`'s legacy
      fallback (per [Data model](#session-srcsessionrs110-131)); update `plan_image`'s image
      resolution to take the CLI `--image` value; update `injected_features` to key on
      integration; fix the auto/resume flag ordering in `agent_command`; update doc comments
      on `agent_command`, `ContainerPlanInput`, `plan_container`/`plan_image`/`plan_devcontainer`
      to say "command"/"integration" instead of "agent" where that's what they now mean.
- [ ] `src/error.rs`: rename `AutoRequiresAgent` → `AutoRequiresCommand` (message: "--auto
      requires a command; set one with --agent, --cmd, or configure defaults.agent /
      defaults.command"), update `ContainerImageNotConfigured`'s message to mention `--image`.
- [ ] `src/doctor.rs`: `check_agent`/`check_image_mode` take resolved command/integration/image
      instead of a single `agent_name`; `check_agent`'s `integration: none` case is `ok`, not
      `warn`/`fail`.
- [ ] `cargo test` and `cargo clippy --all-targets -- -D warnings` clean.

### integration-tester

- [ ] New `tests/features/harness_decoupling.feature`, with `AM_PODMAN_BIN`/`AM_DOCKER_BIN`
      mocks:
  - UC-1: `--agent claude` unchanged — pin the exact `run` invocation (image, mounts, CMD)
        against the pre-change baseline.
  - UC-2: `--image my-image --cmd my-agent` — assert the mocked runtime records a `run` with
        `my-image` and CMD `my-agent`, and asserts **no** credential mount/env for any of the
        four known agents appears.
  - UC-3: `--agent claude --cmd ./wrapper.sh` — assert Claude's credential mount is present
        *and* CMD is the wrapper, not `claude`.
  - UC-4/UC-5: `am run <slug> ./tool.sh` then simulate window loss and `am attach <slug>` —
        assert relaunch with no `(resuming)` wording and no auto/resume flags in the replayed
        command.
  - UC-8: `--integration cladue` fails before any container/worktree side effect; `--cmd
        my-agnet` succeeds at the `am` level (the mocked runtime doesn't know or care that the
        binary is missing, which is the correct thing to assert — `am` did its job).
  - Regression: every existing scenario in `start.feature`/`container.feature`/
        `full_flow.feature`/`attach_restore_agent.feature` still passes unmodified — this is
        the test that the "no interface change" cells in the [Design](#design-the-coupling-is-narrower-than-it-looks)
        table are actually true.
  - Devcontainer: a config with `agent_install = "feature"` and `--agent claude --cmd
        ./wrapper.sh` — assert the built image's label still carries the claude-code Feature
        (this is the regression test for the Feature-injection bug fix; it cannot be written
        against the *unfixed* code without a mock devcontainer CLI, so write it to fail loudly
        against `main` before the fix lands, not just pass after).
- [ ] Unit tests for `resolve_launch` covering every cell of the three precedence tables
      independently (tier-1-beats-tier-2 for each axis, all four tiers empty → `None`,
      integration typo → error, command typo → no error).

### code-reviewer

- [ ] Confirm zero interface changes landed in `container.rs`/`onboarding.rs` beyond doc
      comments — this is the design's central claim and the easiest place for scope to creep
      back in during implementation.
- [ ] Confirm `check_agent`'s `integration: none` path is `ok`, never `warn`/`fail`.
- [ ] Confirm `--cmd`/`--image` never call `KnownAgent::parse` or any validation function
      anywhere in the new code paths.
- [ ] Confirm the Feature-injection fix and the auto/resume ordering fix both shipped with
      their own dedicated tests, not folded silently into an unrelated assertion.
- [ ] Confirm `Session.integration` round-trips through a legacy record (missing key → `None`,
      no crash) and that the attach-time inference-and-persist happens at most once per record.

### documentation-writer

- [ ] `docs/reference/configuration.md`: document `defaults.command`/`defaults.integration`
      alongside the existing `defaults.agent`, and `--cmd`/`--integration`/`--image` alongside
      `--agent` in the `am start`/`am run` sections.
- [ ] `docs/reference/commands.md`: `am start`/`am run` sections gain the three/one new flags;
      a new short "Custom harnesses" callout showing the acceptance-target example verbatim.
- [ ] Per `BACKLOG.md`'s existing "Docs: separate the fast path from the custom path" item —
      this spec's shipped feature is what makes that split concrete; note in `BACKLOG.md`
      (owner: documentation-writer, not this spec, since the instruction is spec-only and does
      not modify `BACKLOG.md`) that the docs item is now unblocked and should point at the new
      flags as "the custom/advanced path."
- [ ] `BACKLOG.md`: leave to the orchestrator to mark both backlog items — "Decouple command,
      integration, and image" and "Custom-harness fast path" — resolved once implementation and
      review land; this spec does not modify `BACKLOG.md` itself per instruction.

## Test plan

Summarized from the task breakdown above; the two properties that matter most and are each
worth their own explicit assertion rather than incidental coverage:

1. **No observable behavior change for any config that only sets `defaults.agent`/`--agent`.**
   Every existing cucumber scenario passes unmodified — a diff in `.feature` files anywhere
   outside the new `harness_decoupling.feature` is a signal something leaked.
2. **Integration `None` is a fully working, first-class state, not a degraded one.** No error,
   no warning from `am doctor`, no crash from `agent_command`/`resume_will_apply`/
   `plan_image`/`plan_devcontainer` — all four already handle `Option<KnownAgent> = None`
   today (verified by reading them during this spec's research; `agent_command`'s early guard,
   `plan_image`/`plan_devcontainer`'s `match agent { Some(_) => ..., None =>
   AgentAuth::default() }`), so the test plan's job is proving that continues to hold once
   `None` becomes reachable through a path other than "no `--agent` given at all."

## Edge Cases & Considerations

- **Security:** a custom-harness session (integration `None`) mounts *strictly less* than a
  preset session — no credential mounts, no credential env — so this change narrows attack
  surface for that path rather than widening it. `--image`/`--cmd` accepting unvalidated
  strings is not a new risk: `container.image` already accepts an arbitrary string today with
  identical trust properties (the image is trusted exactly as much as any other `container.image`
  value already is).
- **Performance:** `resolve_launch` is pure string/enum resolution, no I/O; negligible next to
  the container preflight it feeds into.
- **UX:** the doctor/start error messages for `ContainerImageNotConfigured`/
  `AutoRequiresCommand` need updating in the same PR that changes the conditions under which
  they fire, or a user hits a stale message that doesn't mention the flag that would have
  fixed it — flagged explicitly in the task breakdown, not left implicit.
- **Race conditions:** none introduced; no new shared state, no new concurrency.
- **Config drift across a shared `.am/config.toml`:** unaffected by the additive-only key
  policy — a teammate on an older `am` binary sees `command`/`integration` as unknown keys and
  gets the existing warn-don't-fail behavior (`BACKLOG.md`, "Decided against" —
  `deny_unknown_fields`), exactly the property that policy exists to preserve.

## Open Questions

Each has a recommended default; ship the default unless the user overrides it.

### OQ-1: Should `am doctor` (and `am setup --agent`) grow its own `--cmd`/`--integration`/
`--image` flags for full symmetry with `am start`?

**Recommendation: not in this change.** `am doctor` has always reported the *persisted config's*
readiness, not a hypothetical command line's — it takes no `--no-container`/`--auto`/`--rebuild`
either, for the same reason. Adding three flags here is a small, real, independently-shippable
follow-on with no dependency on anything in this spec beyond `resolve_launch` already existing.
Low urgency: the only user this helps is someone iterating on a `--cmd`/`--image` combination
before committing it to config, who can just run `am start` itself to find out.

### OQ-2: Should `KnownAgent` be renamed to `KnownIntegration` at all, and if so, when?

**Recommendation: yes, eventually, as its own PR, not scheduled as part of this feature.**
The type's *meaning* is settled by this spec (it is "integration," full stop); the name is now
mildly misleading but not incorrect (an integration is still tied to a specific agent CLI). A
pure `cargo`-mechanical rename is low-risk and easy to review in isolation — exactly the kind
of change that should not share a PR with a logic change, per [Resolved Decisions](#resolved-decisions)
#3. Needs a decision on timing (next release cycle? opportunistic, next time someone touches
`container.rs` for an unrelated reason?), not a decision on whether it's correct.

### OQ-3: Is a `--integration none` (or empty-string) escape hatch needed, to force integration
off even when `--agent`/`defaults.agent` would otherwise supply one?

**Recommendation: not in this change, revisit if requested.** No use-case in this spec's
research needs it — `--cmd` alone, with no `--agent`, already gets you integration `None`; the
only gap is "I want to type `--agent claude` for its image-lookup convenience but explicitly
suppress its credential mounting," which is a narrow, easily-worked-around case (just use
`--image`/`defaults.agent`'s `[agents.claude].image` value directly instead of `--agent`).
Adding a sentinel value now, before anyone has asked for it, risks guessing wrong about its
spelling or semantics.

### OQ-4: Should `am start --image` with a devcontainer-mode session (where `--image` is
inert) print a note, or stay silent the way `container.image` already does today?

**Recommendation: stay silent, matching existing behavior for `container.image`.** No prior
complaint or test exists about `container.image` going unused in devcontainer mode despite
being set; adding a note only for the new `--image` flag while leaving the old one silent
would be an inconsistency, not an improvement. If this is judged worth fixing, it should be
fixed for both flags together, as its own small change, not smuggled into this one.

# Spec: Agent Profiles

**Status:** implemented and shipped. Implements `BACKLOG.md` → *Decouple command, integration,
and image* and *Custom-harness fast path*.

## Background

This document supersedes two from-scratch designs that duplicated work already done. A local
bookmark, `harness-profiles` (never pushed), had already implemented this backlog item before
either was written; `BACKLOG.md` listed it as unstarted, which was wrong. Their design won on
its decisive point — persist the agent's *name* only and re-resolve it fresh, once per use,
feeding a single resolved value to every consumer — and was ported onto this branch with two of
its own product decisions rejected: no `agent`→`harness` rename, and no `--cmd`/`--image` CLI
flags. Both rejections are analyzed below, since they are exactly the questions a from-scratch
design would ask again. The name "agent profiles" reflects what shipped: an agent is a profile
of command, environment, and integration living in `[agents.<name>]` — not a "harness" (the
rejected rename) and not a "decoupling" (a design that was never built).

## Feature Overview

Before this feature, a single `--agent` string meant three things at once: the command to
exec, the credential preset to mount, and (via `[agents.<name>].image`) the image to run.
`KnownAgent::parse` rejected any name outside `claude|copilot|gemini|codex`, even with
`--no-container`, so there was no path to "run this image, mount these credentials, exec this
command" for anything `am` was not compiled with.

**User value:**

- A repo defines `[agents.aider]` once, and `am start idea --agent aider` works exactly like a
  built-in — same validation, same doctor checks, same menu entry in `am setup`.
- The built-in four stop being privileged: their commands, auto/resume flags, images, and
  credential mounts are all user-overridable, field by field, because they are expressed in the
  same structure a user's own config produces.
- Integration is optional. An agent with no credential preset at all — `[agents.plain]` with
  only a `command` — is a first-class, fully supported configuration, not a degraded one. This
  is what makes `am` genuinely agent-agnostic rather than an `am`-blessed-four launcher.

**Non-goal, unaffected by this feature:** no ad-hoc `--cmd`/`--image` fast path (see
[Rejected: ad-hoc `--cmd`/`--image` flags](#rejected-ad-hoc---cmd---image-flags)) — a one-off
custom agent means adding a config section first. No daemon, no web UI, no PTY ownership, no
cross-machine coordination — see `BACKLOG.md` → *Decided Against*.

## The Three Concepts

`--agent` used to conflate these three. They are now independently specifiable per agent, in
the same `[agents.<name>]` table:

| Concept | What it decides | Where it comes from |
|---|---|---|
| **command** | argv exec'd in the agent pane | `[agents.<name>].command`, or inherited from a built-in of the same name |
| **environment** | the image, or a devcontainer | `[agents.<name>].image`, or devcontainer detection (unaffected by this feature — see the note in Config Surface) |
| **integration** | credential mounts, env passthrough, preflight | `[agents.<name>.integration]`, optional |

## Data Model

### `Harness` — the resolved shape (`src/harness.rs:162-178`)

```rust
/// A named, fully-resolved agent: what `[agents.<name>]` (plus, for a built-in, the
/// compiled-in profile) resolves to. Does **not** carry `image`/`devcontainer_feature` —
/// those stay resolved separately via `config::resolve_image`/`resolve_agent_feature`, which
/// already read the same `[agents.<name>]` table; duplicating them here would create two
/// sources of truth for the one decision config already owns.
pub struct Harness {
    pub name: String,
    pub command: Vec<String>,           // argv; first element is the binary
    pub auto_flags: Vec<String>,        // appended under --auto
    pub resume: Option<Vec<String>>,    // argv form for resuming; None = agent confirmed not to support it
    pub integration: Option<Integration>,
}
```

`Harness` is internal Rust vocabulary — the module doc comment is explicit that nothing here
renames the user-facing surface: "the user-facing vocabulary stays 'agent' throughout
(`--agent`, `[agents.<name>]`, `defaults.agent`, `AM_AGENT`)." See
[Why the wire vocabulary stays "agent"](#why-the-wire-vocabulary-stays-agent) for why.

### `Integration` — credential wiring, optional (`src/harness.rs:117-136`)

```rust
/// How an agent authenticates. `None` on a `Harness` is a first-class value — a command that
/// needs no credentials from the host is a perfectly good agent.
pub struct Integration {
    pub mounts: Vec<CredentialMount>,
    pub env: Vec<EnvSource>,
    /// OR of ANDs: at least one group must be fully satisfied. Only Codex needs the outer list
    /// to have more than one entry — an API key *or* an interactive sign-in — but expressing
    /// the other three as a single one-element group keeps one code path.
    pub requires_any: Vec<Vec<Requirement>>,
    /// Shown when `requires_any` has alternatives and none is satisfied. A single-group
    /// integration reports the specific missing path instead, which is more useful; with
    /// alternatives there is no single path to name.
    pub alternatives_message: Option<String>,
    pub hint: String,                   // am doctor's failure hint
    pub home_optional: bool,            // an unresolvable $HOME yields no mounts, not an error
}
```

`requires_any` being an OR-of-ANDs is the minimum shape that expresses the Codex rule the code
already implemented as a `match` arm: authenticated by `~/.codex/auth.json` *or*
`OPENAI_API_KEY`, either sufficient.

`CredentialMount` (`src/harness.rs:86-101`) carries two fields beyond the obvious host/container
path pair, each earning its place by encoding behavior that used to be implicit in a `match`
arm: `required` (Claude declares two mounts but only the config dir is a precondition —
`.claude.json` is useful when present and not worth failing a session over) and
`only_if_exists` (mounting a missing path makes the runtime create it root-owned on the host;
only Codex's `.codex` directory needs this). `HostPath` (`:30-40`) is not a plain `PathBuf`
because Claude honours `CLAUDE_CONFIG_DIR`, and every path is resolved against `$HOME` when the
harness is *used*, not when it is defined — `EnvOrUnderHome` and `UnderHome` express that.
`EnvSource::GhToken` (`:105-115`) stays a named variant rather than a general "run this command"
escape hatch, deliberately: shelling out during preflight (`gh auth token`) is a capability
worth granting by name, not by config — a user-defined agent cannot express it.

### What each pre-existing function became

| Before | After |
|---|---|
| `KnownAgent::parse` (closed 4-name enum) | `harness::AgentName::parse` — checks the merged agent table: built-ins plus whatever `[agents.<name>]` defines |
| `resolve_agent_auth_mounts` | read `Integration::mounts` |
| `agent_auto_flags` | read `Harness::auto_flags` |
| `agent_resume_flags` | read `Harness::resume` |
| `validate_agent_credentials` | `Integration::satisfied()` — evaluates `requires_any` |
| `credentials_hint` / `codex_credentials_error` | read `Integration::hint` / `alternatives_message` |
| `config::resolve_agent_feature` | unchanged — reads `[agents.<name>].devcontainer_feature` directly (see the deviation note below) |

**One behavior change, in an edge case.** Claude's credential check previously resolved `$HOME`
with `unwrap_or_default()`, so an unset `HOME` produced the misleading `requires path to exist:
.claude` (a relative path, checked against the current directory). It now reports plainly that
`HOME` is unset (`src/harness.rs:61-65`).

**Deviation from the design this was ported from:** `Harness` carries no `image`. Images already
live in `[agents.<name>].image`, where a user could override them before this feature landed;
duplicating compiled-in images into `Harness` would create two sources of truth for a decision
config already owns. `config::resolve_image`/`resolve_agent_feature`
(`src/config.rs:338-361`) are unchanged in shape and are exactly where that decision stays.

### `AgentName` — identity before resolution (`src/harness.rs:346-406`)

Replaces `KnownAgent` for the many call sites that only want *identity* — which agent is
configured, what to print, what to record in the session — not the full `Harness`. An enum
could only ever name agents the binary was compiled with; `AgentName::parse(value, cfg)` checks
`builtin(value).is_some() || cfg.agents.contains_key(value)`, so it widens for free the moment a
config defines a new section.

```rust
pub const BUILTIN_NAMES: &[&str] = &["claude", "copilot", "gemini", "codex"];

/// Every agent name available, built-ins first, then every other name in `cfg.agents`,
/// alphabetically. The list an error message or a menu shows.
pub fn all_names(cfg: &Config) -> Vec<String>;
```

An unknown name's error lists exactly this list — `"unknown agent 'nope' — configured agents
are: claude, codex, copilot, gemini"` plus any custom sections — strictly better than a fixed
four-name list, since it can point at a genuine near-miss instead.

### Overriding a built-in, and what "override" means per field

`harness::resolve(name, cfg)` (`src/harness.rs:414-451`) starts from the compiled-in profile (if
any) and overlays `[agents.<name>]` on top, **field by field** for `command`/`auto_flags`/
`resume`, but **wholesale** for `integration`: a config-supplied `integration` table replaces
the built-in's entirely rather than merging into it, deliberately — "a half-overridden set of
credential rules is not a thing anyone can reason about" (`src/config.rs:105-108`'s doc comment
on `AgentSettings.integration`). A config-only entry (no built-in of that name) with no
`command` is a distinct, named error — `"agent 'half-defined' is defined in config but has no
command"` — from an unrecognized name entirely, which matters for anyone debugging their own
config: one is a typo, the other is an incomplete definition.

### Config parse shape — kept in sync with the resolved shape by a real-file test

`AgentSettings` (`src/config.rs:93-109`, the merged in-memory shape) and `FileAgentSettings`
(`:373-382`, the per-file TOML parse shape, with its own `#[serde(flatten)] unknown` catch-all)
both declare the same five fields — `image`, `devcontainer_feature`, `command`, `auto_flags`,
`resume`, `integration` — and `apply_file_config`'s per-agent merge loop
(`:526-545`) copies every one of them. This is the exact class of bug a struct gaining fields
while its file-parsing twin does not would produce — new keys silently caught by the `unknown`
flatten, warned about, and never reached — and it is pinned by a dedicated test
(`agent_command_and_integration_parse_from_a_real_toml_file`, `:1417-1454`) that parses a real
TOML file through `load_with_global` rather than constructing `AgentSettings` directly, which
"would pass even if these two keys silently landed in `unknown` and never reached
`harness::resolve`" (the test's own comment). `auto_flags = []` is handled with
`apply_opt_some` rather than `apply_opt_string`, specifically because an empty list is a
meaningful override — "this agent has no autonomous mode" — that must overwrite the built-in's
`auto_flags` rather than being treated as absent the way an empty string is.

`IntegrationSettings`/`MountSettings`/`RequirementSettings` (`src/config.rs:113-148`) are the
TOML shapes for `[agents.<name>.integration]`; `harness::convert_integration`
(`src/harness.rs:461-525`) is where they become `Integration`/`CredentialMount`/`Requirement`,
validating as it goes: an unknown `mode` (anything but `"ro"`/`"rw"`), a `requires_any` entry
naming both `path` and `env` (or neither), and a host path that is neither `~/`-relative nor
absolute (`src/harness.rs:531-545` — a relative path "would resolve against whatever directory
`am` happened to run in", which is never what anyone means) are all rejected at resolve time
with a specific, quoted error naming the offending agent.

## Config Surface

```toml
[agents.aider]
command = ["aider", "--model", "sonnet"]
image = "ghcr.io/me/aider:latest"
auto_flags = ["--yes-always"]
resume = ["--restore-chat-history"]

[agents.aider.integration]
env = ["ANTHROPIC_API_KEY"]
requires_any = [[{ env = "ANTHROPIC_API_KEY" }], [{ path = "~/.aider.conf.yml" }]]
hint = "export ANTHROPIC_API_KEY, or create ~/.aider.conf.yml"

[[agents.aider.integration.mounts]]
host = "~/.aider.conf.yml"
container = "~/.aider.conf.yml"
mode = "rw"
```

Overriding a built-in is the same operation, at whatever granularity is wanted:

```toml
[agents.claude]
auto_flags = []    # opt out of --dangerously-skip-permissions; image, command, integration
                    # all still come from the compiled-in default
```

A `command`-only agent needs no integration table at all:

```toml
[agents.plain]
command = ["plain-agent"]
```

`[agents.""]` (an empty section name — syntactically legal TOML, produced by a stray
`[agents.]` heading with the dot swallowed) is rejected explicitly at load, rather than sitting
in `am setup`'s menu as an unlabeled entry (`src/config.rs:943-953`).

**Unaffected by this feature, stated so it is not re-derived:** `container.image` still wins
over any per-agent `image` (`config::resolve_image`'s existing precedence,
`src/config.rs:346-361`, unchanged); devcontainer mode never resolves an `am` image at all
(`plan_image` is the only caller of `resolve_image`, and devcontainer sessions route to
`plan_devcontainer` instead) — so nothing in this feature changes how devcontainer mode picks
its environment.

## CLI Surface

No new flags. `am start`, `am setup`, and `am doctor` all keep their existing `--agent`/`-a`
(`src/cli.rs:47-59`) — it now resolves through `harness::AgentName::parse`/`harness::resolve`
instead of `KnownAgent::parse`, but the flag itself, its short form, and its help text are
unchanged. There is no `--cmd`, `--image`, or `--integration` flag anywhere — see the rejection
below. A one-off custom agent means adding a section to `.am/config.toml` first; there is no
faster path than that, by design.

## Why the wire vocabulary stays "agent"

The design this was ported from proposed a hard-break rename: `--agent` → `--harness`,
`[agents.<name>]` → `[harnesses.<name>]`, `defaults.agent` → `defaults.harness`, `AM_AGENT` →
`AM_HARNESS`, `Session.agent` → `Session.harness` — with every old spelling detected and
refused with an explanatory error, committed as `feat(cli)!` with a `BREAKING CHANGE:` footer.
Rejected in the port. The reasoning, adapted from the sharpest analysis in an earlier draft of
this document:

**The compatibility risk is not new — the rename made it worse, not better.** `.am/config.toml`
is committed and shared, and — with or without this feature — a `defaults.agent` value naming
anything outside the compiled-in four has always hard-errored on a binary that predates whatever
introduced that name (`KnownAgent::parse`'s closed enum, then and now). That is a real,
unavoidable cost of this feature existing at all: the moment someone commits `defaults.agent =
"aider"`, every teammate on an older `am` gets `am start` failing outright in that repo until
they upgrade — the single most-used command, not a peripheral one. Nothing in this design
removes that risk. What the rename would have done is make the *same* risk manifest worse.

**A rename fails silently; the current design fails loudly, which is strictly better.** An `am`
binary predating a rename has no `harness`/`harnesses` field anywhere in its config structs —
`defaults.harness` and every key under `[harnesses.<name>]` fall into the top-level
`#[serde(flatten)] unknown` catch-all (`FileConfig::unknown`, `src/config.rs:382-384`), which
the project's standing policy warns about rather than rejects. `cfg.agent` — the field the old
binary actually reads — stays unset, because the new config never populates it under a hard
break with no dual-reading. `effective_agent` resolves to `None`. **A plain `am start` does not
error at all in that case** — it opens a session with an empty agent pane, silently, and the
only trace of what went wrong is one line in a warning block a user may never read. Contrast
the actual, shipped design: an unrecognized `defaults.agent` value still hard-errors, loudly,
immediately, naming exactly what is wrong (`"unknown agent '<name>' — configured agents are:
..."`). A loud failure that stops a user in their tracks is a better failure than a silent one
that lets them keep going with nothing running.

**This directly contradicts the project's own reasoning for rejecting `deny_unknown_fields`.**
`BACKLOG.md` records that decision precisely so a teammate on an older `am` is not broken the
moment someone commits a key their binary predates. That policy's entire value is that an old
binary keeps working, merely ignorant of one new thing. A wholesale rename defeats it by
construction: the old binary does not become ignorant of one key, it becomes ignorant of *every*
key in the table it needs, because the whole vocabulary moved out from under it in one step —
the "warn and carry on with defaults" behavior the policy was built to provide degrades into
"warn and silently run with nothing configured." Keeping `--agent`/`[agents.<name>]`/
`defaults.agent`/`AM_AGENT`/`Session.agent` exactly as they are is what lets an old binary
reading a config with a *new custom section* still correctly resolve `defaults.agent` when it
happens to name a built-in, and fail loudly — the worst case this feature can produce — only
when it names something the old binary has never heard of. That is the smallest failure surface
available, not a compromise short of the "proper" rename.

**Accepted, not solved, and worth stating plainly:** a config-only agent name (`defaults.agent =
"aider"`) genuinely does hard-error on any `am` predating this feature. No message this design
can print reaches that binary before it fails — the CHANGELOG and release notes are the only
mitigation, and neither is enforced by the tool itself. This is the same risk the rejected
rename would also have carried in its own "loud" failure mode (an old binary hitting the
detected-and-refused old-spelling error); the rename's actual cost was adding a second, *silent*
failure mode on top of it, for the exact same underlying compatibility gap.

## Rejected: ad-hoc `--cmd`/`--image` flags

Two independent implementations reached for `--cmd`/`--image`/`--auto`-adjacent CLI flags as the
"fast path" for a one-off custom agent, and both hit the same three edge cases — evidence about
the approach, not about who wrote it:

1. **`--image` versus `--no-container`.** An explicit `--image` on the command line implies a
   container is wanted. Honouring `container.enabled = false`/`--no-container` at the same time
   would silently drop the flag the user just typed — the only coherent resolution is a hard
   error when both are present, which is itself a new error condition with its own wording to
   get right and keep right.
2. **`--cmd` merge semantics.** Does `--cmd` alone build a fully anonymous, integration-less
   agent, or does `--cmd` alongside `--agent claude` replace only the command while keeping
   Claude's image and credentials? Both are real, wanted use cases, and distinguishing them
   correctly — and only them — is exactly the kind of precedence question that took multiple
   review passes to pin down correctly in an earlier draft of this feature, for a capability
   that config-only agents do not need at all: `[agents.claude-logging]` with `integration =
   "claude"`-equivalent semantics already expresses "borrow Claude's credentials, run something
   else" as ordinary config, no flag precedence to define.
3. **`--auto` on an agent with no `auto_flags`.** Should it error (a harness that cannot be
   made autonomous should say so) or warn (an ad-hoc agent's `Session.auto` still selects the
   `Auto-Piloted-By` commit trailer, which is a legitimate thing to want even with nothing to
   add to the command line)? Getting this right for an *anonymous*, per-invocation agent is a
   design question in its own right, separate from `--auto`'s existing, unchanged
   `AutoRequiresAgent` check (`src/error.rs:40`, `src/main.rs:699-700`) — which still only means
   "an agent must be named at all," exactly as before this feature, since no ad-hoc agent
   construction exists to raise the question.

None of these three needs an answer under the shipped design, because there is no anonymous,
per-invocation agent construction to define semantics for at all. The cost accepted in
exchange — a genuine one-off custom agent requires a config edit first, even for a single
session — is judged worth it: it is a small, one-time cost against a CLI surface that, in both
independent attempts, needed real design work to get right and keep right across `--no-container`,
agent-name precedence, and `--auto`. Config-only agents get all three of these questions
answered implicitly and correctly by construction (a section either exists with an image and no
container conflict is possible, `command` either overrides or the whole agent inherits, and
`--auto` behaves exactly as it always has), for zero additional CLI surface.

## `am attach`: resolve fresh, every time, from the plain name

`Session.agent: Option<String>` is unchanged in name, type, and meaning — it has always been,
and still is, the agent's *name* (`"aider"`, `"my-harness"`), never a resolved command or a
separately-tracked integration. Every consumer that needs the full definition —
`plan_container_runtime`'s credential check, `plan_image`/`plan_devcontainer`'s command and
mounts, `agent_command`'s composed argv — calls `harness::resolve(name, cfg)` fresh, at the
point it is needed, rather than reading anything persisted beyond the name. This is what makes
the design simpler than the one it replaces: there is no second, resolved-value shape to keep
in sync with the name across a session's lifetime, because nothing is ever cached past a single
resolve call.

**`attach_recreate_container_cmd`** (`src/main.rs:1718-1776`) resolves `s.agent` once, at the
top of the function, and threads that single `Option<Harness>` to both
`plan_container_runtime`'s credential check and `ContainerPlanInput` — one resolution, two
consumers, so the two cannot disagree about which agent this is. This is a fix made during the
port: an earlier design threaded a separately-computed, already-resolved value into one of the
two consumers and the plain name into the other, which meant a config edit to an agent's
`integration` between `am start` and a later `am attach` could validate credentials against one
answer while planning the container against another. Resolving once, at the top, for both, makes
that particular class of drift structurally unrepresentable rather than merely avoided.

**A section deleted between `am start` and a later `am attach` fails loudly, on both attach
paths, but differently — and deliberately so.** The container-recreate path cannot proceed
without a resolved agent, so it fails the whole `am attach` outright, with an error naming the
session, the agent, and what went wrong (`"session '<slug>' was started with agent '<name>',
which no longer resolves: <reason>\nHint: restore [agents.<name>] in .am/config.toml, or run
'am destroy <slug> --force' and start over"`, `src/main.rs:1744-1750`) — but only *after* the
window and split already exist (A3 in `attach-restore-agent.md`: a retry has something real to
act on). The host-relaunch path (`launch_into_agent_pane`, `src/main.rs:1885-1933`) degrades
instead: `am attach` itself still succeeds, nothing is launched into the pane, and a `Note:`
explains why (`AttachLaunch::AgentNotConfigured { name, reason }`,
`print_agent_not_configured_note`, `:2005-2009`) — the same asymmetry `agent_command`'s own doc
comment states plainly: a host session has no container to fail to recreate, so there is nothing
to lose by leaving the pane idle and saying why, where a container session genuinely cannot be
brought up at all. Both scenarios are pinned by `tests/features/custom_agent.feature`'s two
"deleted section" scenarios (`:78-116`).

## `am doctor` impact

`check_agent`/`check_image_mode` (`src/doctor.rs:576-589`, `746-793`) resolve through
`harness::resolve`/`harness::all_names` exactly as `am start` does, so the two cannot drift.
Two things worth stating explicitly:

- **An unrecognized name's hint lists every configured agent**, not a fixed four — the same
  `harness::all_names(cfg)` list `am start`'s own error uses.
- **`credentials: none required`, not `credentials: present`, for an agent with no
  integration.** "Present" would be a lie for a check that found nothing because nothing was
  looked for — the distinction is "you are authenticated" versus "authentication is not `am`'s
  business here for this agent," and reporting the wrong one would misrepresent a fully working,
  intentional configuration as unverified.

No permanent `am doctor` warning exists for the mixed-version risk described above. One was
considered during review and rejected: every other `Status::Warn` in `doctor.rs` is transient
and locally actionable (credentials clear on login, an unbuilt image clears on the next `am
start`); a warning for "this repo's `defaults.agent` might break a teammate's older binary"
would fire forever for a correctly configured agent, name a risk the local run can neither
observe (other people's binary versions) nor resolve, and train users to skim past warnings —
which costs more than the risk it would name. The mitigation is documentation, not a standing
check: whatever `docs/reference/configuration.md` says about `[agents.<name>]` is the place this
risk is stated, not `am doctor`'s output.

## `am setup` impact: the agent menu is dynamic

`am setup`'s agent menu lists every configured agent — built-ins and custom sections alike —
not a fixed four. `menu(cfg)` (`src/onboarding.rs:50-58`) is built from
`harness::all_names(cfg)`, the exact same list `am doctor`'s unknown-agent hint and `am start`'s
`AgentNotConfigured`-equivalent error use, so the menu and every other agent-name-listing
surface in the tool can never disagree about what exists.

**Ordering.** The four built-ins appear first, in their existing fixed order (`claude`,
`copilot`, `gemini`, `codex`), followed by every other name in `cfg.agents` sorted
alphabetically (ASCII byte order). `cfg.agents` is a `HashMap`, whose iteration order carries no
guarantee and must never leak into an interactive menu; sorting the non-built-in tail is what
keeps the menu's order identical across runs. Built-ins keep their existing positions rather
than folding into one fully alphabetical list so a config with no custom agents — the
overwhelmingly common case — shows today's exact menu (`[1] claude`, `[2] copilot`, ...), and a
custom agent whose name happens to sort earlier than `claude` does not bump it out of `[1]` and
disrupt existing muscle memory.

**Credential annotation — three states, not two.** A row is `"credentials found"` when the
agent has an integration and it is satisfied, blank when it has one and it is not (silently
"not yet", the pre-existing behavior), and `"no integration"` when the agent has none at all
— so a config-defined agent with no credential preset is never confused with a built-in whose
credentials merely have not been set up yet. `"no integration"` never appears next to a
built-in, which always has one; the two are pinned as mutually exclusive by
`ask_agent_renders_no_integration_for_a_custom_agent_alongside_a_built_in_row`
(`src/onboarding.rs:3053-3067`).

**Column alignment generalizes for free.** The gap between the longest name and its note is
still a fixed constant (`MENU_NOTE_GAP`), but the column *width* it is added to is computed
fresh from the dynamic entry list at the call site, exactly the pattern that already worked for
a fixed four-item array — a longer custom name never silently misaligns the menu.

**`am setup --agent <name>` and free-text entry at the prompt both validate against the same
table `am start` uses**, sharing the identical `AgentNotConfigured`-shaped error. Typing an
*existing* custom agent's name at the prompt is accepted — selecting, the same way typing
`"claude"` instead of `"1"` already was — but there is no `[N] add a new agent...` option and no
free-text entry defines a section on the fly. **Listing configured agents is not the same as
becoming a config editor**: only agents that already exist in `cfg.agents` are ever listed or
selectable; defining a new one stays purely a config-editing action outside `am setup`'s scope.

## Use-Cases (from `tests/features/custom_agent.feature`)

- **A config-only agent, no built-in of that name.** `[agents.aider]` with just a `command` —
  `am start feat --agent aider --no-container` launches it, with no credential mounts, exactly
  like the acceptance target: a name `am` has never heard of reaches the same code path a
  built-in does.
- **`command` differs from the section name.** `[agents.my-harness]` with `command =
  ["my-agent", "--flag"]` — starts and, after `am attach` recreates the container, still
  launches `my-agent --flag`, not `my-harness`. This is the regression the port's own fix
  specifically pins: launching the section *name* as the command "worked only because every
  built-in is named after its binary," and broke silently for the first config-defined agent
  whose name and command differ.
- **Overriding one field of a built-in.** `[agents.claude]` with `auto_flags = []` — `am start
  feat --agent claude --auto` no longer appends `--dangerously-skip-permissions`, while the
  command, resume flags, and credential mounts are all still Claude's own.
- **An incomplete config entry.** `[agents.half]` with no `command` — fails with `"has no
  command"`, distinct from an unrecognized name entirely.
- **A malformed credential path.** A relative `host` in a mount fails at resolve time with
  `"must start with"`, before any container work starts.
- **`am doctor` on a config-defined default agent** — reports it by name, through the same
  resolution `am start` uses.
- **A section deleted before a later `am attach`** — see
  [the two-armed behavior above](#am-attach-resolve-fresh-every-time-from-the-plain-name).

## Testing

769 unit tests, 163 cucumber scenarios, `cargo clippy --all-targets -- -D warnings` clean, as of
the port landing. `tests/features/custom_agent.feature` is the dedicated coverage for this
feature (nine scenarios, listed above); `src/harness.rs`'s own unit tests
(`:547-749`) cover `resolve`'s merge semantics, both `requires_any` shapes, and every rejection
`convert_integration`/`parse_host_path` can produce, all without any container runtime.

## Risks

- **The mixed-version compatibility gap is real and unmitigated in-tool** — see
  [Why the wire vocabulary stays "agent"](#why-the-wire-vocabulary-stays-agent) and
  [`am doctor` impact](#am-doctor-impact). Documentation is the only mitigation; nothing in the
  running tool warns a user committing a custom `defaults.agent` value that older teammates will
  hard-error until they upgrade.
- **Blast radius.** `container.rs` routes every credential and mount decision through `Harness`
  now; the surrounding suite (769 unit tests) is what keeps this from being a silent regression
  rather than an argued one.
- **Credential preflight still checks presence, not validity** — the pre-existing, open
  `BACKLOG.md` item. This feature does not fix it, and a user-defined `requires_any` rule is
  equally presence-only. Not a regression introduced here; called out so it is not mistaken for
  one.

## Rejected Alternatives

- **Renaming `--agent` to `--harness` (and the matching config/env/session renames).** See
  [Why the wire vocabulary stays "agent"](#why-the-wire-vocabulary-stays-agent).
- **Ad-hoc `--cmd`/`--image` CLI flags for a one-off custom agent.** See
  [Rejected: ad-hoc `--cmd`/`--image` flags](#rejected-ad-hoc---cmd---image-flags).
- **Keeping `KnownAgent` alongside config-defined agents**, resolving each through a separate
  code path. Rejected: two implementations of one concept, and the built-ins would have stayed
  privileged in exactly the ways users kept hitting — the entire motivation for this feature.

# Feature: Decouple Command, Integration, and Image

## Background

From `BACKLOG.md`, "Architecture Audit Follow-ups" → "Decouple command, integration, and
image (highest priority)":

> Today a single `--agent` string means three things at once: the command that launches
> (`main.rs` appends it as the container CMD), the auth preset (`container.rs::resolve_agent_auth`),
> and the image (`config::resolve_image` via `[agents.<name>]`). `KnownAgent::parse` rejects
> any name outside `claude|copilot|gemini|codex` — even with `--no-container` — so there is no
> path to "run this image, mount these creds, exec this command."

This revision replaces two earlier drafts of this spec with a different model, proposed by the
user: instead of adding parallel CLI flags for command/integration/image alongside `--agent`,
**complete the table `[agents.<name>]` already is.** `--agent <name>` stops being a
`KnownAgent::parse` gate and becomes a lookup into `cfg.agents`, which gains two new fields
(`command`, `integration`) alongside its existing two (`image`, `devcontainer_feature`). One
flag, one lookup, no new CLI surface at all.

**Note on scope, not re-derived here:** `am run` was removed prior to this spec, and that
removal is now committed history (`7334eaeb`; see `src/main.rs`'s `cmd_run_removed`,
`AmError::RunRemoved`) — a prior, independent change, not part of this spec. This document
accounts for its absence but does not own or re-argue that decision.

**Correction already recorded in the backlog, not re-derived here:** this was originally logged
as blocking Dev Container Support and turned out not to be. Devcontainer mode never resolves an
`am` image outside `plan_image` (`src/main.rs:1039`), which devcontainer sessions never reach —
so `--agent claude` already stops implying an image on that path today, unaffected by this spec.

The backlog's adjacent item, **"Custom-harness fast path"**, and the decoupling item above
collapse into the same mechanism under this model: there is no separate CLI work to "unlock" a
custom harness once `[agents.<name>]` can name its own command and (optionally) an integration
— defining the section *is* the fast path. The acceptance target is now: add
`[agents.my-harness]` with `command`/`image` to `.am/config.toml`, then `am start idea --agent
my-harness`. The user has explicitly accepted that a one-off custom harness requires editing
config first — there is no `am start --cmd ...`-shaped escape hatch in this design.

## Assumptions

- **A1.** "Integration" means exactly what `container.rs` already calls the six behaviors keyed
  on `KnownAgent` today: credential mounts, extra env, credential presence-validation, the
  credentials hint, auto-mode flags, and resume flags. Nothing new is being invented here.
- **A2.** "Image" means only the `container.image` / `[agents.<name>].image` axis
  (`plan_image`'s world). Devcontainer mode's environment still comes entirely from the repo's
  own `.devcontainer/devcontainer.json`; nothing here changes that.
- **A3.** No change to the tmux/container mount, network, or SELinux-labeling machinery in
  `container.rs`. Every one of its public functions already takes `Option<KnownAgent>` and
  nothing else agent-related; this spec adds no new parameter to any of them.

## The new model, in a few sentences

`AgentSettings` (`src/config.rs:88-93`) gains `command: Option<String>` and `integration:
Option<String>`. `--agent <name>` / `defaults.agent = "name"` resolve to a lookup,
`cfg.agents.get(name)`, not a `KnownAgent::parse`. Two defaulting rules make every config in
the wild today keep working unchanged: **command defaults to the section name**, and
**integration defaults to the section name when it parses as a `KnownAgent`, otherwise
`None`**. `KnownAgent` survives exactly as it is today — it is the *value* `integration`
resolves to, still driving the same six behaviors in `container.rs`, unchanged. Incoherent
states that needed a whole paragraph to argue against in the previous draft (`--agent claude
--integration gemini` — mount gemini's credentials, exec `claude`) are simply unrepresentable
now: there is one flag, and it names one section, which has one `integration` value.

## Why this beats the previous two drafts

`[agents.<name>]` was already "everything `am` knows about an agent"; it stopped short only of
the command and the auth preset, which is exactly why those two leaked onto `--agent` as hidden
extra meanings in the first place. Completing the table dissolves the coupling instead of
building a second, parallel CLI surface beside it (`--cmd`/`--integration`/`--image`, as the
previous draft did). It also makes custom agents *committable and shareable*: a repo can define
as many named harnesses as it wants in `.am/config.toml`, where `defaults.command` (the
previous draft's design) gave it exactly one. And the "217 references" framing the earlier
drafts leaned on — the argument that `container.rs`/`onboarding.rs` don't need interface
changes because `KnownAgent` already means "integration" everywhere — still holds, and holds
*more cleanly*: nothing about command or image is threaded through `KnownAgent` under this
model at all, in any file, ever. See [Honest cost report](#honest-cost-report) below for where
that claim still needed real, if smaller, revision.

## Data model

### `AgentSettings` (`src/config.rs:88-93`)

```rust
pub struct AgentSettings {
    pub command: Option<String>,       // NEW — what to exec; defaults to the section name
    pub integration: Option<String>,   // NEW — which built-in auth preset; see defaulting rule 2
    pub image: Option<String>,         // existing, unchanged in shape
    pub devcontainer_feature: Option<String>,  // existing, unchanged in shape
}
```

`[agents.<name>]` in the TOML file gains two optional keys:

```toml
[agents.my-harness]
command = "./scripts/agent.sh"
integration = "claude"     # optional — omit for no built-in auth preset at all
# image = "..."             # optional — see Inheritance below for what happens when omitted
# devcontainer_feature = "..."
```

No key is renamed, no key is removed. Every config that only ever sets `[agents.claude]`/
`[agents.copilot]`'s existing keys, or `defaults.agent`, is untouched — see
[Defaulting rules](#defaulting-rules) for why.

### `resolve_agent` (new, `src/config.rs`, beside the existing `resolve_image`)

The one place that resolves a section name into everything downstream needs, shared by
`cmd_start` and `doctor::run` so they cannot drift — the same "shared, not duplicated"
principle `specs/guided-setup.md` already applies between `am setup`'s verification and
`am doctor`. Absorbs what `resolve_image` and `resolve_agent_feature` did separately today,
because both need the inheritance rule below and neither can apply it alone.

```rust
pub struct ResolvedAgent {
    pub name: String,                              // the section name, for display
    pub command: String,                            // never empty — rule 1 guarantees this
    pub integration: Option<container::KnownAgent>, // rule 2
    pub image: Option<String>,                      // see Inheritance
    pub devcontainer_feature: Option<String>,        // see Inheritance
}

/// Resolve `name` against `cfg.agents`. Errors only when no section named `name` exists —
/// `AmError::AgentNotConfigured(name, available)`, `available` being every configured section
/// name, sorted, so the message can list them. A section that exists but leaves every field
/// unset still resolves successfully (rules 1 and 2 fill `command`/`integration`); it is
/// finding no section at all that fails, not an incomplete one.
pub fn resolve_agent(name: &str, cfg: &Config) -> Result<ResolvedAgent>;
```

`cmd_start`'s existing three-line coupling bug —
`effective_known_agent = effective_agent.map(KnownAgent::parse).transpose()?`
(`src/main.rs:689-692`) — becomes, at the same call site, roughly the same number of lines:
`effective_agent.as_deref().map(|name| config::resolve_agent(name, &cfg)).transpose()?`. The
fix does not need to be bigger than the bug was; it needs to call a real resolver instead of an
enum parse.

### Defaulting rules

Verified against the actual compiled-in skeleton (`src/config.rs:239-263`,
`global_config_template`'s `[agents.*]` block at `src/config.rs:640-660`), per the instruction
not to assert this without checking.

1. **`command` defaults to the section name.** `[agents.gemini]` with only `image` set still
   runs `gemini`.
2. **`integration` defaults to the section name when it parses as a `KnownAgent`, otherwise
   `None`.** `[agents.claude]` still gets Claude's credential mounts with no new keys written
   anywhere.

Together these mean every config in the wild — which only ever sets `image`/
`devcontainer_feature` on `[agents.claude]`/`[agents.copilot]`, or `defaults.agent` naming one
of the four built-ins — resolves identically to today, field for field.

**One real, required change the rules alone don't cover, found by checking rather than
assuming:** `default_agent_images()` (`src/config.rs:239-263`) only populates `claude` and
`copilot` in the compiled-in `cfg.agents` map — `gemini` and `codex` have **no entry at all**
today, compiled-in or otherwise. `resolve_image`'s existing `cfg.agents.get(name)` already
tolerates that (falls through to "no image," fine in `--no-container` mode or host sessions).
But `resolve_agent`'s contract is "error if the section doesn't exist," and a bare `--agent
gemini --no-container` — which works today with zero configuration — would regress into
`AgentNotConfigured` if `gemini` genuinely has no section to find. **`default_agent_images()`
must be extended to include all four `KnownAgent` presets as entries**, with `image: None`/
`devcontainer_feature: None` for the two that have no compiled default, so a section always
exists to find for any of the four built-ins, exactly matching `onboarding.rs`'s `MENU` (which
already lists all four). This is a small change — two more tuples in an existing array literal
— but a real, necessary one; not covered by "the two rules alone."

### Inheritance: does `integration` pull in the preset's `image`/`devcontainer_feature`?

The question the team-lead posed directly: `[agents.team-harness]` sets `integration =
"claude"` and nothing else. Does it get Claude's compiled-in image and devcontainer Feature, or
none?

**Decision: inherit.** When a section's own `image`/`devcontainer_feature` is unset, and its
resolved `integration` is `Some(k)`, `resolve_agent` falls back to `cfg.agents.get(&k.to_string())`'s
`image`/`devcontainer_feature` — the *built-in preset's own section* — before giving up. Each
field is independent: a section can inherit the image and set its own `devcontainer_feature`,
or vice versa, or override both, or neither.

**Rejected alternative: section-is-self-contained** (no inheritance; a custom section gets
exactly what it states, nothing more). Rejected because it forces every team-wrapper config to
duplicate the preset's image string (`ghcr.io/dstanek/am-claude-minimal:latest`) verbatim into
their own section — the exact class of duplication `[agents.<name>]` exists to avoid, and one
that drifts silently the day `[agents.claude]`'s compiled default changes and nobody remembers
to update the copy. The trade-off accepted in exchange: a section that sets `integration =
"claude"` and nothing else picks up an image the author never typed, which could read as
surprising in isolation — mitigated by `am doctor`/`am start`'s existing detail lines already
naming the resolved image explicitly, so "which image did this actually use" is never a mystery
at the point it matters.

### `Session` (`src/session.rs:110-131`)

One new field:

```rust
pub struct Session {
    // ...existing fields...
    pub agent: Option<String>,  // unchanged in name; now documented as the *resolved command*,
                                 // not the section name — see reasoning below
    #[serde(default)]
    pub integration: Option<String>,  // NEW — the resolved integration, if any
}
```

**What gets persisted, and why it is the resolved values and not the section name.** A section
can be edited or deleted between `am start` and a later `am attach` — `.am/config.toml` is a
file, not a database, and nothing stops a teammate from renaming `[agents.my-harness]` or
deleting it entirely while a session built from it is still running. If `Session` recorded only
the *section name*, `am attach` would have to re-resolve `cfg.agents` at attach time to know
what to relaunch — and a section edited in the meantime would silently relaunch something
different from what is actually running, or a deleted section would break the relaunch outright
with no fallback. Persisting the already-resolved `command`/`integration` instead means `am
attach` never needs `cfg.agents` at all: it relaunches exactly what was launched before,
independent of whatever the config says *now*. This is the same principle
`SessionContainer.image`/`config_hash` already follow — an opaque, already-resolved string, not
a pointer back into a config that may have moved.

**Not persisted: the section name itself.** A `Session.agent_section` field recording which
`[agents.<name>]` produced a given session would be genuinely useful for `am list` observability
("this session was started via `my-harness`") — but that's the existing, separately-tracked
`BACKLOG.md` item "Session observability in `am list`," not something this spec's correctness
goal (`am attach` relaunches the right thing) requires. Left out deliberately rather than
smuggled in; a future observability pass can add it without touching anything this spec
defines.

**Backward compatibility.** A record with no `integration` key deserializes as `None`. `am
attach`'s existing legacy-fallback shape (`src/main.rs:1970-1979`) already re-derives
`known_agent` by parsing `s.agent` when nothing better is available — extend that one step:
when `s.integration` is `None`, fall back to `KnownAgent::parse(s.agent).ok()`, persisting the
result the same way OQ-1 in `attach-restore-agent.md` already persists a recovered `s.agent`.

## `AmError::AgentNotConfigured` and `KnownAgent::parse`'s role, unchanged

Two distinct validation failures now, where there was one before:

- **`--agent`/`defaults.agent` names a section that doesn't exist at all** — new
  `AmError::AgentNotConfigured(name, available)`, message: `"no agent 'cladue' configured —
  configured agents: claude, codex, copilot, gemini, my-harness"` (sorted, and — this is the
  point — includes any user-defined sections, which a fixed four-name enum list never could).
  This replaces `KnownAgent::parse`'s error on the `--agent`/`defaults.agent` path entirely.
- **`AgentSettings.integration` is explicitly set to something that doesn't parse** —
  unchanged: `KnownAgent::parse`'s existing error (`src/container.rs:49-60`), still exactly the
  four-name list, because `integration` genuinely can only ever be one of the four built-ins —
  there is nothing else it could coherently name. `KnownAgent::parse` itself needs **no code
  change**; it is simply called from a narrower place (validating one field) than it used to be
  (validating the whole `--agent` flag).

## Per-behavior dispatch

Unchanged from the previous draft's finding, and — under this model — even more clearly true,
since nothing about command or image is threaded through `KnownAgent` in any file, ever:

| Behavior | Location | Keyed on | Changed? |
|---|---|---|---|
| Credential mounts | `resolve_agent_auth_mounts`, `src/container.rs:326` | `KnownAgent` | No |
| Extra env | `resolve_agent_auth`'s `env` field, `src/container.rs:490` | `KnownAgent` | No |
| Credential validation | `validate_agent_credentials`, `src/container.rs:565` | `KnownAgent` | No |
| Credentials hint | `credentials_hint`, `src/container.rs:607` | `KnownAgent` | No |
| Auto-mode flags | `agent_auto_flags`, `src/container.rs:406` | `KnownAgent` | No |
| Resume flags | `agent_resume_flags`, `src/container.rs:430` | `KnownAgent` | No |
| Devcontainer Feature injection | `resolve_agent_feature` → folded into `resolve_agent` | `KnownAgent`, via inheritance | No — see below |

### The devcontainer Feature-injection "bug" from the previous draft evaporates

The previous draft found a real latent bug: `injected_features` keyed the Feature lookup on the
*command* string, which only ever matched the integration by coincidence (command and
integration were forced equal). Under this model there is no command string to key on at all —
`resolve_agent` resolves `devcontainer_feature` from the section's own value, or, via the
inheritance rule above, from the *integration's own section* — which is exactly "keyed on
integration," by construction, not by a fix layered on top. **Saying so plainly, as instructed
rather than defended: this is not a bug carried forward from the previous draft. It does not
exist under this model, and there is nothing to fix.**

## Honest cost report

The previous draft's headline claim — "`container.rs` and `onboarding.rs` need no interface
changes" — was checked against this model rather than repeated on faith, per the instruction.

**`container.rs`: still zero.** Confirmed above — no function in `container.rs` gains, loses,
or changes a parameter. `KnownAgent::parse`, `Display`, and all six behavior functions are
untouched.

**`onboarding.rs`: revised upward — the user overrode this spec's OQ-1 recommendation, and the
menu itself now changes.** The previous pass of this spec recommended keeping `am setup`'s
4-item preset menu (`MENU`, `src/onboarding.rs:29-33`) untouched, on the reasoning that listing
custom sections was a config-editor-shaped feature `am setup` shouldn't grow. The user rejected
that recommendation directly: a menu that silently omits an agent the user themselves
configured reads as a bug, not a boundary. See
[The agent menu becomes dynamic](#the-agent-menu-becomes-dynamic) under `am setup` impact for
the resulting design (ordering, credential annotation, column width, `--agent` validation, and
the boundary that *is* still preserved — listing, never creating).

With that reversal, the things the previous estimate said were untouched no longer are: `MENU`
itself is deleted (replaced by a list computed per invocation from `cfg.agents`),
`agent_credentials`/`has_credentials` become section-derived rather than `KnownAgent`-indexed,
`parse_agent_answer` needs the resolved entry list to accept a typed section name (not just a
`KnownAgent::parse`), and the render loop's credential annotation grows a third state. Layered
on top of the previously-identified `resolve_effective`/`effective_agent` fix (unchanged — still
needed, still the same reasoning), the total footprint in the "which agent" question's code path
is roughly 150–250 lines across ~8–9 items (`MENU`'s replacement, `DetectedState::gather`,
`agent_credentials`/`has_credentials`, `default_agent()`, `agent_write`, `ask_agent`'s render
loop and "currently" line, `parse_agent_answer`, `resolve_effective`'s agent branch,
`MENU_NOTE_GAP`'s doc comment) — larger than the 60–100 lines previously quoted, and reported as
such rather than smoothed over. Still confined to the single "which agent" question: the
container/layout questions, the toml-writing helpers (`update_project_agent`, etc.), and the
shared `write_target_line`/`dim_line` machinery are untouched.

**The one genuinely structural finding, not just "more lines in the same shape":**
`DetectedState::gather`'s own module doc comment states a deliberate principle — "Deliberately
not `config::load_with_global`: that merges the layers into a single answer, and knowing *which*
layer an answer came from is the whole point here" (`src/onboarding.rs`, module doc). That
principle was true for the four scalar keys `gather` tracks (`defaults.agent`,
`container.enabled`, the three `tmux.*` keys), each read per-layer via `TrackedKeys`/
`read_tracked`. Computing a *menu row* for a custom section is a different kind of question:
its resolved `integration` (and, transitively, any inherited `image`/`devcontainer_feature`)
can genuinely depend on values from a *different* layer than the one that defines the section
(a project-level `[agents.my-harness]` naming `integration = "claude"`, whose image comes from
a global or compiled-in `[agents.claude]`) — exactly what `config::resolve_agent` is built to
resolve, and `resolve_agent` operates on an already-merged `&Config`, not a per-layer
`TrackedKeys`. This is the first time `am setup`'s detection phase needs a genuinely merged
config, not just per-layer tracked scalars, and it is worth being explicit that this changes
what the module's own stated principle covers — it is no longer true of the module as a whole,
only of the four scalar keys it always meant. The cleaner shape, to keep this an intentional
addition rather than a contradiction buried inside `gather`: `cmd_setup` loads a `Config` once
(the same `load_config` helper `cmd_start` already uses) and passes it into
`DetectedState::gather` as a new parameter, which `gather` uses only to build the menu-entry
list via `config::resolve_agent`, leaving the four tracked scalars' per-layer reads exactly as
they are today. Two resolution strategies coexisting on purpose, each documented for what it is
for, rather than one silently absorbing the other's job.

**`doctor.rs`: bigger than the previous draft assumed, still bounded to two functions.**
`check_agent`/`check_image_mode` (`src/doctor.rs:576-589`, `746-782`) previously just needed to
*receive* an already-resolved value. Now they need `resolve_agent` to actually run the
resolution — section lookup, both defaulting rules, the inheritance lookup — because there is
no other call site computing it for them. Both are updated to call `config::resolve_agent`
once (shared between them via `doctor::run`, so they cannot report two different answers for
the same config) and gain a genuinely new failure mode that didn't exist before:
`Status::Fail` when `--agent`/`defaults.agent` names a section that doesn't exist at all
(`AgentNotConfigured`), with the hint listing configured names.

## Use-Cases

### UC-1: `--agent claude` — the built-in shorthand, unchanged

**Actor:** any existing user. **Main flow:** `am start feat --agent claude` resolves
`cfg.agents.get("claude")`, finds the compiled-in section, and gets `command = "claude"` (rule
1, section name), `integration = Some(Claude)` (rule 2, "claude" parses), `image =
"ghcr.io/dstanek/am-claude-minimal:latest"` (the section's own value). Byte-for-byte identical
container run to today. **Business rule:** this is the regression the entire task is scoped
around not breaking; pinned by tests, not just argued.

### UC-2: Custom harness, the acceptance target

**Actor:** a user with their own agent CLI and their own image, no built-in integration.
**Preconditions:** `.am/config.toml` has been edited to add:

```toml
[agents.my-harness]
command = "my-agent"
image = "my-image"
```

**Main flow:** `am start idea --agent my-harness`. `resolve_agent("my-harness", &cfg)` finds
the section, `command = "my-agent"` (explicit), `integration = None` ("my-harness" doesn't
parse as a `KnownAgent`, and no explicit `integration` key was set), `image = "my-image"`
(explicit). No credential preflight, no mounts beyond `container.env`, no devcontainer Feature
injected. **Postconditions:** a running session with no `am`-managed credentials — the user's
image is fully responsible for having `my-agent` and whatever it needs already baked in.

### UC-3: Wrapper around a known integration, with inheritance

**Actor:** a user who wants Claude's credentials and default image, but launches through a
wrapper script instead of the bare `claude` binary. **Preconditions:**

```toml
[agents.claude-logging]
command = "./scripts/claude-with-logging.sh"
integration = "claude"
```

No `image`/`devcontainer_feature` in this section. **Main flow:** `am start feat --agent
claude-logging`. `resolve_agent` finds `command` explicit, `integration = Some(Claude)`
explicit, then — since `image`/`devcontainer_feature` are unset here — inherits both from
`cfg.agents.get("claude")` (the built-in section) via the inheritance rule.
**Postconditions:** Claude's credentials are mounted, Claude's devcontainer Feature is injected
if devcontainer mode applies, and the container's CMD is the wrapper script. A variant worth
noting: the same section could additionally set its own `image = "my-team-claude-image"` to
keep Claude's credentials while overriding only the image — each field inherits independently.

### UC-4: `am attach` relaunching a session with no integration

**Actor:** a user whose machine rebooted, mid-session on UC-2's setup. **Preconditions:**
`Session.agent = Some("my-agent")`, `Session.integration = None`, tmux window gone. **Main
flow:** identical to `attach-restore-agent.md`'s UC-1/UC-2, except every integration-gated step
is a no-op: `known_agent` resolves to `None` (from `s.integration`, never by re-resolving
`cfg.agents`), so `agent_command`'s `auto`/`resume` branches never execute, and
`preflight_agent_auth` is never called for a container recreate. **Postconditions:** `Opened
new window for session 'idea' and relaunched 'my-agent'.` — no `(resuming)` suffix, because
there is nothing to resume and `am` never claims otherwise. This already falls out of the
existing gating with zero new code once `known_agent` is sourced from `Session.integration`.

### UC-5: `am doctor` on a custom-command config, and on a dangling one

**Actor:** a user running `am doctor` against a config using UC-2's or UC-3's setup, or a typo.
**Main flow (UC-2's config):** `check_agent` reports `command: my-agent` and
`integration: none — no credential checks apply` as **ok**, not a warning — a fully supported,
intentional configuration. `check_image_mode` reports `image: my-image`. **Main flow (a typo,
`defaults.agent = "cladue"`):** `check_agent` reports `Status::Fail`, `"no agent 'cladue'
configured"`, hint listing every configured section name (built-ins plus any user-defined
ones) — a strictly better hint than the previous model's fixed four-name list could give,
since it can point at a genuine near-miss like `"claude-logging"` if that's what the user
actually meant.

### UC-6: Devcontainer mode with an inherited integration and a custom command

**Actor:** UC-3's user, in devcontainer mode. **Main flow:** the resolved
`devcontainer_feature` (inherited from `[agents.claude]`) is injected regardless of the
section's own `command`; the built image's CMD is still the wrapper script.
**Postconditions:** the image contains both `claude` (from the inherited Feature) and whatever
the wrapper needs — `am` never inspects whether the wrapper actually calls `claude`.

### UC-7: A typo, at each of the two places a typo can now occur

**Actor:** a user who mistypes. **Main flow A — `--agent` names a nonexistent section:**
`am start feat --agent cladue` — `AmError::AgentNotConfigured`, `"no agent 'cladue' configured
— configured agents: claude, codex, copilot, gemini"` (or more, with user-defined sections).
**Main flow B — `integration` inside a real section is misspelled:**

```toml
[agents.my-harness]
integration = "cladue"
```

`am start feat --agent my-harness` — `KnownAgent::parse`'s existing error, unchanged wording,
fired from `resolve_agent`'s field-level validation. **Business rule:** both are now real,
CLI-surfaced typo errors — unlike the previous two drafts, there is no third case ("a typo in
`--cmd`") that silently succeeds, because there is no `--cmd` flag left to typo.

## `am setup` impact

**The menu now lists configured sections — the user overrode this spec's earlier
recommendation, and the reasoning was direct:** *"if we don't list custom agents it will be
filed as a bug by users. it's not what i would expect either."* A menu that silently omits an
agent the user themselves configured is a bug report waiting to happen, not a scoping boundary
worth defending. See the design below.

### The agent menu becomes dynamic

**Source of truth.** `const MENU: [container::KnownAgent; 4]`'s own doc comment
(`src/onboarding.rs:27-28`) says `KnownAgent` is "the source of truth for which agents exist."
That was always slightly wrong even under the old model (the source of truth was really "the
four names `KnownAgent::parse` accepts," which happened to be enumerable as a `KnownAgent`
array) and is now actually wrong: `cfg.agents` is the source of truth for which agents exist —
the four built-ins appear in it automatically, as compiled-in sections (see the defaulting-rules
fix to `default_agent_images()` above), and any custom section a user adds appears the same way.
`MENU` as a fixed `[KnownAgent; 4]` is deleted; the doc comment claiming it as the source of
truth goes with it.

**1. Ordering.** The four built-ins appear first, in their current fixed order (`claude`,
`copilot`, `gemini`, `codex`), followed by every other name in `cfg.agents` sorted alphabetically
(ASCII byte order — no locale-aware collation, kept simple and deterministic). Justification:
`cfg.agents` is a `HashMap`, whose iteration order is not guaranteed and must never leak into an
interactive menu — that's the non-determinism bug named directly. Putting the built-ins first
in their existing order, rather than merging everything into one alphabetical list, preserves
today's exact menu positions (`[1] claude`, `[2] copilot`, ...) for the overwhelmingly common
case of a config with no custom sections — a fully alphabetical merge would silently reorder
built-ins the moment a custom section's name sorts earlier (e.g. `"aardvark-harness"` bumping
`claude` to `[2]`), which is disruptive to existing muscle memory for a feature most users will
never touch. Custom sections carry no prior positional expectation among themselves, so
alphabetical is a reasonable, simple, and — the point — deterministic tiebreak.

**2. Credential note per row — three states, not two.** Today's row is either `"credentials
found"` (checked) or nothing (not checked, read as "not yet"). That silence works when there
are only two possible states; it stops working the moment a third state — "there is nothing to
check" — exists, because unlabeled silence would then read as "credentials missing" for a
config that has no credentials to be missing, which is exactly the confusion the team-lead
flagged. Three states, three presentations:

- `integration: Some(k)`, credentials found → `"credentials found"` (unchanged).
- `integration: Some(k)`, credentials not found → nothing extra (unchanged) — still legitimately
  "not yet," and adding a label here isn't needed to resolve the ambiguity above.
- `integration: None` → `"no integration"` — an explicit, honest annotation, chosen to be the
  same length-class as `"credentials found"` so the column still reads cleanly, and to say
  plainly "there is nothing here to check" rather than leave a blank a user could misread as a
  problem.

**3. Column alignment generalizes, but the doc comment doesn't yet — fix the comment, not the
mechanism.** `MENU_NOTE_GAP`'s doc comment (`src/onboarding.rs:36-40`) already computes `width`
freshly at the call site from whatever's being displayed (`MENU.iter().map(|a|
a.to_string().len()).max()`), which is exactly the pattern that generalizes: swap `MENU` for the
dynamic entry list and the same `.map(|e| e.name.len()).max()` computation produces correct
alignment for any set of names, built-in or custom, of any length. Verified there is no other
fixed-width assumption anywhere else in the render path. What needs to change is only the
comment's specific claim that width is "computed from `MENU`... so a longer agent name added
later doesn't silently misalign the menu" — true in spirit, wrong in specifics once `MENU` no
longer exists; reword to name the dynamic entry list instead.

**4. `am setup --agent <name>`.** Currently validated via `KnownAgent::parse`
(`src/onboarding.rs:593-601`, the `agent_flag: Option<container::KnownAgent>` parameter to
`ask_agent`). Becomes a section lookup, sharing the exact `AgentNotConfigured` error `am start`
raises for the same condition — one error type, one message shape, reached from two call sites,
not two independently-worded "unknown agent" messages that could drift apart.

**5. The boundary that survives, stated explicitly so it doesn't quietly disappear along with
the one that didn't.** Listing configured sections is not the same as becoming a config editor,
and this design does not blur that line: the menu enumerates what already exists in
`cfg.agents` — there is no `[N] add a new custom agent...` option, no free-text prompt that
creates a section on the fly. Typing an existing custom section's name at the prompt (instead of
its menu number) is accepted, the same way typing `"claude"` instead of `"1"` already is today —
selecting, not creating. Typing a name that matches nothing in `cfg.agents` re-prompts with the
same "not one of 1-N or a configured agent name" handling the menu already has for an
out-of-range number, exactly mirroring `am start`'s `AgentNotConfigured` for the same input.
Defining a *new* section is still, and remains, purely a config-editing action outside `am
setup`'s scope — this is the boundary the previous draft was protecting, just drawn one line
over from where it was.

## `am doctor` impact

`check_agent`/`check_image_mode` now share one `resolve_agent` call per `doctor::run` invocation
(see [Honest cost report](#honest-cost-report)) and report a section-not-found failure that
didn't exist before. No CLI surface change — `am doctor` still takes no flags related to this
spec.

## `am attach` impact

Unchanged in shape from the previous draft's finding: the only functional change to
`cmd_attach` (`src/main.rs:1943-2006`) is sourcing `known_agent` from `s.integration` (falling
back to parsing `s.agent`, per the Session data-model section) instead of always parsing
`s.agent`. Everything downstream is unaffected, because it already takes `known_agent:
Option<KnownAgent>` as an opaque input.

## Task breakdown

### backend-engineer

- [ ] `src/config.rs`: `AgentSettings.command`/`AgentSettings.integration` fields
      (`#[serde(default)]`); `default_agent_images()` extended to include `gemini`/`codex` as
      entries with `image: None`/`devcontainer_feature: None`; `resolve_agent`/`ResolvedAgent`
      (folding in `resolve_image`/`resolve_agent_feature`'s existing logic plus the two
      defaulting rules and the inheritance rule).
- [ ] `src/session.rs`: `Session.integration: Option<String>`, `#[serde(default)]`; roundtrip
      test with the field set and a legacy-record test confirming a missing key loads as
      `None`.
- [ ] `src/main.rs`: `cmd_start` calls `resolve_agent` instead of `KnownAgent::parse`;
      `cmd_attach`'s legacy fallback per the Session data-model section; `agent_command`'s doc
      comment updated (signature likely takes `Option<&ResolvedAgent>` or its unpacked
      `command`/`integration` fields — implementer's choice, no behavior change either way);
      fix the `agent_auto_flags`-before-`agent_resume_flags` ordering while `agent_command` is
      already being touched (unchanged rationale from the previous draft — zero marginal cost,
      no agent combines the two today).
- [ ] `src/error.rs`: new `AmError::AgentNotConfigured(String, String)` (name, sorted
      comma-joined configured list); update `ContainerImageNotConfigured`'s message to mention
      adding `image` to `[agents.<name>]`.
- [ ] `src/doctor.rs`: `check_agent`/`check_image_mode` call the shared `resolve_agent` result;
      new `Status::Fail` case for a not-configured section; `integration: none` stays `ok`.
- [ ] `src/onboarding.rs`: per [Honest cost report](#honest-cost-report) and
      [The agent menu becomes dynamic](#the-agent-menu-becomes-dynamic) — `cmd_setup` loads a
      `Config` once and passes it into `DetectedState::gather`; `MENU` deleted, replaced by a
      menu-entry list built from `cfg.agents` (four built-ins first in current order, then
      other sections alphabetically); `agent_credentials`/`has_credentials` become
      section-derived; `ask_agent`'s render loop gains the three-state credential annotation
      (`"credentials found"` / blank / `"no integration"`); `parse_agent_answer` accepts a
      typed section name, not just a `KnownAgent::parse`; `MENU_NOTE_GAP`'s doc comment
      reworded for the dynamic list; `effective_agent`'s type changes to
      `Effective<Option<String>>`; `agent_write`/`default_agent()`/`ask_agent`'s "currently"
      line updated to compare section names as strings; `cmd_setup`'s `--agent` validation
      switches from `KnownAgent::parse` to the shared `AgentNotConfigured` path.
- [ ] `cargo test` and `cargo clippy --all-targets -- -D warnings` clean.

### integration-tester

- [ ] New `tests/features/harness_decoupling.feature`, with `AM_PODMAN_BIN`/`AM_DOCKER_BIN`
      mocks:
  - UC-1: `--agent claude` unchanged — pin the exact `run` invocation against the pre-change
        baseline.
  - UC-2: a config defining `[agents.my-harness]` with `command`/`image`, no `integration` —
        assert the mocked runtime records `my-image` and CMD `my-agent`, and asserts no
        credential mount/env for any of the four built-ins appears.
  - UC-3: `[agents.claude-logging]` with `integration = "claude"`, no `image` — assert Claude's
        credential mount is present, the inherited image is used, and CMD is the wrapper.
        Include the variant where the section also sets its own `image`, overriding only that
        one inherited field.
  - UC-4: simulate window loss on UC-2's session, `am attach` — assert relaunch with no
        `(resuming)` wording and no auto/resume flags.
  - UC-5/UC-7: a config with `defaults.agent = "cladue"` — `am doctor`/`am start` both fail
        with `AmError::AgentNotConfigured`, hint lists configured names; a config with
        `[agents.my-harness]` `integration = "cladue"` — fails with `KnownAgent::parse`'s
        unchanged error.
  - UC-6: a devcontainer config with `agent_install = "feature"` and UC-3's section — assert
        the built image's label carries the claude-code Feature (this is now a
        correct-by-construction assertion, not a regression test for a fix — see
        [the evaporated bug](#the-devcontainer-feature-injection-bug-from-the-previous-draft-evaporates)).
  - Regression: every existing scenario in `start.feature`/`container.feature`/
        `full_flow.feature`/`attach_restore_agent.feature` still passes unmodified.
- [ ] Unit tests for `resolve_agent`: both defaulting rules independently; the inheritance rule
      for `image` and `devcontainer_feature` independently, including the "section sets its
      own, no inheritance" case; `AgentNotConfigured` lists every configured name, sorted,
      including user-defined ones; a compiled-in `gemini`/`codex` lookup succeeds with `image:
      None` (the regression this spec's defaulting-rules section flags explicitly).
- [ ] Unit tests for `onboarding.rs`'s updated `resolve_effective`/`effective_agent`: a
      `defaults.agent` naming a valid, configured custom section reports it as the effective
      value (not "none configured"); a `defaults.agent` naming nothing configured still reads
      as unfilled, preserving UC3's repair-prompt behavior in `guided-setup.md`.
- [ ] Unit tests for the dynamic agent menu: built-ins always appear first in their current
      order regardless of `cfg.agents`' `HashMap` iteration order; custom sections appear
      after them, alphabetically, and a second run with the same config produces the identical
      order (determinism, not just "looks sorted once"); a section with `integration: None`
      shows `"no integration"`, not a blank; a section with `integration: Some(k)` and no
      credentials on this host shows a blank, not `"no integration"` — the two must not be
      confusable; `am setup --agent <bogus>` and typing a bogus name at the interactive prompt
      both produce the same `AgentNotConfigured`-shaped message `am start` would.

### code-reviewer

- [ ] Confirm zero interface changes landed in `container.rs` — the design's central claim,
      and still the easiest place for scope to creep back in.
- [ ] Confirm the agent menu's ordering is deterministic across repeated runs regardless of
      `cfg.agents`' `HashMap` iteration order — this is the exact bug class flagged during
      design; a flaky-looking cucumber scenario here is a real bug, not a harness issue.
- [ ] Confirm the three credential-note states (`"credentials found"` / blank / `"no
      integration"`) are each reachable and distinguishable, and that `am setup`'s scope
      boundary holds: no menu option creates a section, and no free-text entry silently
      defines one.
- [ ] Confirm `resolve_agent`'s inheritance rule is applied per-field independently (a section
      can inherit `image` while setting its own `devcontainer_feature`, or vice versa).
- [ ] Confirm `check_agent`/`check_image_mode` call `resolve_agent` exactly once per
      `doctor::run` invocation, not once each (drift risk if they diverge).
- [ ] Confirm `Session.integration` round-trips through a legacy record and that the
      attach-time inference-and-persist happens at most once per record.

### documentation-writer

- [ ] `docs/reference/configuration.md`: document `AgentSettings.command`/`.integration`
      alongside the existing `image`/`devcontainer_feature`, with the inheritance rule spelled
      out and a worked custom-harness example matching UC-2/UC-3.
- [ ] `docs/reference/commands.md`: `am start`'s `--agent` description updated from "select a
      known agent" to "select a configured `[agents.<name>]` section"; new short "Custom
      harnesses" callout.
- [ ] `BACKLOG.md`: left to the orchestrator to mark the two backlog items resolved once
      implementation and review land; this spec does not modify `BACKLOG.md` itself.

## Edge Cases & Considerations

- **Security:** a custom-harness session (`integration: None`) mounts *strictly less* than a
  preset session — no credential mounts, no credential env. Unchanged reasoning from the
  previous draft.
- **Performance:** `resolve_agent` is a hashmap lookup plus at most one more (the inheritance
  fallback); negligible.
- **UX:** `AgentNotConfigured`'s message needs to land in the same PR that introduces the
  failure mode it names, or a user hits a stale error — flagged in the task breakdown.
- **Config drift across a shared `.am/config.toml`:** unaffected by the additive-only key
  policy (`command`/`integration` are new, optional `AgentSettings` fields) — a teammate on an
  older `am` binary sees them as unknown keys and gets the existing warn-don't-fail behavior.
- **A section can name itself as its own integration's inheritance source pathologically**
  (e.g. `[agents.claude]` with `integration = "claude"` — the default anyway, so this is
  self-referential but not infinite: the inheritance lookup only ever recurses one level, into
  the *named* integration's section, never further, so there's no cycle to guard against.

## Decided by the user

All three Open Questions from the previous draft have been resolved; recorded here rather than
left as open items.

- **Custom sections appear in `am setup`'s agent menu** — this spec's earlier recommendation
  (stay preset-only) was overridden directly: *"if we don't list custom agents it will be filed
  as a bug by users. it's not what i would expect either."* See
  [The agent menu becomes dynamic](#the-agent-menu-becomes-dynamic) for the resulting design.
- **`KnownAgent` → `KnownIntegration` is scheduled, not merely recommended** — its own PR, once
  this spec's implementation lands, never sharing a PR with the decoupling itself. Filed in
  `BACKLOG.md` by the orchestrator; not tracked further in this document.
- **No `am agents list` command in this change** — confirmed as recommended, folded into the
  existing `BACKLOG.md` item "Session observability in `am list`" by the orchestrator; not
  tracked further in this document.

No open questions remain.

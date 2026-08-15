# Feature: Dev Container Support

Let a session's environment come from the repo's own `.devcontainer/devcontainer.json`
instead of an `am`-specific image, using the [Dev Containers specification](https://containers.dev).

## Background

Today `am` has one environment model: pick an OCI image (canned via `[agents.<name>].image`,
or custom via `container.image`) and `podman run --rm -it <image> <agent>` with `am`-chosen
mounts. Every project that wants an agent with its own toolchain has to maintain a second,
`am`-shaped image alongside the environment it already describes for its editors and CI.

Dev containers are the existing standard for "this repo's development environment."
Supporting them replaces the bespoke canned/custom-image axis with something projects already
have, and makes `am` composable with VS Code, Codespaces, DevPod, and Zed.

**Prerequisite:** assumes the backlog item *"Decouple command, integration, and image"* has
landed. In devcontainer mode there is no `am`-resolved image, so `--agent claude` must stop
implying one.

---

## Design: split at the build/run seam

The spec has two halves with very different characteristics.

**The build half** — turning `devcontainer.json` into a runnable image — is where all the
complexity and all the churn lives: resolving `features` from OCI registries, ordering them
by `dependsOn`/`installsAfter`, generating a Dockerfile that runs each `install.sh` under the
`_REMOTE_USER`/`_CONTAINER_USER` contract, and composing feature entrypoints.

**The run half** — mounts, user mapping, network, workspace path, lifecycle hooks — is small,
stable, and *is exactly what `am` already does* in `container.rs`.

So: **delegate the build, own the run.**

```
devcontainer.json ──[ devcontainer build ]──► OCI image + devcontainer.metadata label
                                                        │
                            am reads label + devcontainer.json, merges per spec
                                                        │
                                    am's existing podman/docker run machinery
```

`devcontainer build --workspace-folder <worktree> --image-name <name>` produces a plain OCI
image with features baked in, and prints `{"outcome":"success","imageName":…}`. Everything
after that is `am`'s own code path.

### Why this over the alternatives

Against **delegating everything** (`devcontainer up` + `exec`): the CLI insists on owning the
workspace mount and has no concept of jj. An earlier draft of this plan needed
`--mount-git-worktree-common-dir`, `git worktree add --relative-paths`, and either a computed
mount target or a generated `--override-config` with rewritten Dockerfile paths — all of it
`am` bending its worktree layout to fit someone else's assumptions. The split deletes every
one of those workarounds. It also keeps Node off the hot path: no per-pane `exec` latency.

Against **implementing the whole spec in Rust**: the build half is a real project. Zed's
`crates/dev_container` is ~8,000 lines of implementation (plus roughly as much test code),
including a hand-rolled OCI puller. That is bounded and someone has proven the shape — but it
is a project with its own cadence, not a subtask of `am`. Note it ships `LICENSE-GPL`; `am` is
MIT, so it can be read for scope and architecture but **not copied from**.

The split is also the right staging ground: the *only* Node-dependent step becomes "produce an
image from a devcontainer.json." If a Rust feature-builder ever lands — as a separate crate,
where the crates.io gap is real — it swaps that one call and the run path never changes.

### What the Node dependency is, after the split

`@devcontainers/cli` v0.88.0 is 8 files, 1.9 MB unpacked, **zero runtime npm dependencies**:
one 1.72 MB pre-bundled script, a 423 B shim, and `scripts/updateUID.Dockerfile`.
`engines.node` is `>=20.0.0`.

Invocation is a subprocess via `command.rs`, exactly like `git`/`jj`/`tmux`/`podman` today. No
bindings, no FFI, nothing in `Cargo.toml`. `am`'s own artifact is unchanged: one static Rust
binary, same release process, same `install.sh`.

Critically, **it runs once per config change, not once per session.** Images are keyed by a
hash of the resolved config (see *Image identity*), so the second session on an unchanged
config never invokes Node at all — `am start` is a pure `podman run`.

Distribution: detect on `$PATH` with an `AM_DEVCONTAINER_BIN` override and an actionable error
naming `npm install -g @devcontainers/cli`. If friction shows up, the zero-dependency bundle
is trivially vendorable (`include_bytes!` the 1.72 MB script + the Dockerfile asset, extract to
`~/.cache/am/`, run `node <path>`) — MIT, requires shipping `ThirdPartyNotices.txt`.

**Gotcha:** subcommand options must come *after* the subcommand.
`devcontainer build --docker-path podman …` is correct; `devcontainer --docker-path podman
build …` is rejected by yargs. (This is Zed issue #50513 — encode it in a test.)

---

## Spec

### Mode selection

```
container.mode = "auto" | "image" | "devcontainer"     # default: "auto"
```

- `image` — today's behaviour, unchanged.
- `devcontainer` — require a config; error if missing or unbuildable.
- `auto` — devcontainer mode when a config is discovered in the worktree, else image mode,
  announced on stdout.

CLI: `--devcontainer` / `--no-devcontainer` / `--devcontainer-config <path>`.

### Discovery

Resolved **relative to the session worktree** — the config is a checked-in, branch-specific
file. Order: explicit override → `.devcontainer/devcontainer.json` → `.devcontainer.json` →
`.devcontainer/<folder>/devcontainer.json` (single match only; if several, error and list them).

### Image identity and caching

Build into `am-dc-<config-hash>`, where the hash covers the resolved `devcontainer.json`
bytes, the referenced Dockerfile and its build context fingerprint, and any
`--additional-features` `am` injects.

- Image exists → skip the build entirely (no Node).
- Missing or hash changed → build.
- `am start --rebuild` forces it.

Sessions sharing a config share an image. Record the hash in the session so `am list` can flag
a stale environment.

### Build step

```
devcontainer build \
  --workspace-folder <worktree> \
  [--config <path>] \
  --image-name am-dc-<hash> \
  [--additional-features '<json>'] \
  [--docker-path <podman>]
```

Parse `{outcome, imageName}` from stdout; `imageName` is an array in 0.88 but the schema
allows a string — handle both. On failure the CLI exits **1** and prints
`{"outcome":"error","message":...,"description":...}` on stdout (verified — see *Spike
results*), so either signal is sufficient; check both anyway. Human-readable build progress
goes to **stderr**, leaving stdout clean for the JSON.

### Configuration merge

`am` needs the resolved runtime config without a second Node call. Two sources, and the spike
shifted the balance decisively toward the first.

**1. The `devcontainer.metadata` image label** — a JSON array, written by the build, in
merge order: metadata inherited from the base image, then one snippet per Feature, then
**the whole `devcontainer.json` reduced to the metadata schema as the final element**. The
build has already done the hard part: base-image inheritance and feature ordering are baked
in, so `am` merges a flat list left-to-right. Accept a bare object too — the schema permits
it even though 0.88 emitted an array in every spike case.

What it carries (all observed): `entrypoint`, `mounts`, `init`, `privileged`, `capAdd`,
`securityOpt`, `containerEnv`, `remoteEnv`, `containerUser`, `remoteUser`,
`updateRemoteUserUID`, `userEnvProbe`, `overrideCommand`, `forwardPorts`, `shutdownAction`,
`customizations`, all five in-container lifecycle commands, and `waitFor`.

**2. `devcontainer.json` itself**, parsed natively by `am` (JSONC — comments and trailing
commas confirmed accepted by the CLI, so `serde_json_lenient` or `jsonc-parser` is required),
for the properties the label **does not** carry:

| Property | Why `am` needs it |
| --- | --- |
| `runArgs` | Raw runtime flags; gated by the trust prompt |
| `workspaceFolder` | `--workdir`; defaults to the mirrored host path |
| `workspaceMount` | Detect a custom mount that conflicts with host-path mirroring |
| `initializeCommand` | Only to detect and refuse it — `am` never runs it |
| `dockerComposeFile` | Detect and reject (phases 1–2) |
| `name` | Display in `am list` |

Everything else comes from the label. Note the asymmetry this creates: the label is the
source of truth for *runtime behaviour*, `devcontainer.json` for the handful of properties
the metadata schema deliberately drops.

Merge per the spec's documented rules: `capAdd`/`securityOpt` union; `init`/`privileged`
boolean-OR; lifecycle commands and mounts collected (mounts last-wins on conflict);
`waitFor`/`containerUser`/`remoteUser` last-wins; env merged per variable. Array order in the
label already encodes precedence, so **later elements win** and no reordering is needed.

**Mounts arrive in two shapes.** Features contribute objects
(`{"source":...,"target":...,"type":"bind"}`); `devcontainer.json` contributes strings
(`"source=...,target=...,type=bind"`). Both appear in the same array, so the mount parser
must be an untagged enum over string and object.

**Variable substitution is not done for you.** The label preserves `${localWorkspaceFolder}`
verbatim, which is what `am` wants — it substitutes its own worktree path. Implement
`${localEnv:VAR}`, `${containerEnv:VAR}`, `${localWorkspaceFolder}`,
`${containerWorkspaceFolder}`, and `${localWorkspaceFolderBasename}` over the consumed
properties of *both* sources. `${devcontainerId}` has a specified derivation — implement it
only if a consumed property uses it.

### Run step

`am`'s existing `container::build_run_command` and `resolve_mounts`, extended with the merged
config. Everything that already works keeps working: host-path mirroring for the worktree and
VCS dirs, SELinux `,z` labeling, `--userns=keep-id` / `--user`, `gitconfig`/`ssh`/agent-auth
mounts, network gating.

Added from the merged config: `containerEnv`/`remoteEnv` as `-e`; `mounts` translated to `-v`
with `am`'s labeling applied; `init`, `privileged`, `capAdd`, `securityOpt` (gated — see
*Trust*); `runArgs` appended (gated); `workspaceFolder` as `--workdir`, defaulting to the
mirrored host path.

**Entrypoints.** Features may contribute entrypoint scripts that must run before the agent.
Compose them into a single `sh -c '<ep1> && <ep2> && exec <agent>'` invocation rather than
relying on the image's `ENTRYPOINT`.

**User.** `remoteUser`/`containerUser` replaces `container.user` for deriving the container
home (`/root` for root, else `/home/<user>`), with a `devcontainer.home` override. Rootless
podman's `keep-id` already maps the host user, so the CLI's `updateRemoteUserUID` chown step
is unnecessary — that is one more thing the split avoids.

### VCS: both git and jj work in phase 1

This is the payoff. Because `am` owns the mounts, the worktree and VCS dirs are mirrored at
their host paths exactly as they are today (`container.rs:446`–`473`):

- **git** — the worktree's `.git` file holds an absolute `gitdir:`, which resolves because the
  common dir is mounted at the same path. No `--relative-paths`, no
  `--mount-git-worktree-common-dir`, no minimum git version, no migration for existing sessions.
- **jj** — `.jj/repo` is relative (`../../../../.jj/repo`, verified in this repo) and resolves
  because host-path mirroring preserves the offset. Colocated `.git` is mounted as it is today.

Both ship in phase 1. In the delegate-everything design, jj was a phase-2 research item.

### Lifecycle hooks

`am` runs these itself — they are just commands in a container, and `am` already owns the
session state that says whether they have run.

| Hook | When | Where |
|---|---|---|
| `initializeCommand` | before create | **host** — refused by default, see *Trust* |
| `onCreateCommand` → `updateContentCommand` → `postCreateCommand` | once, on container create | container |
| `postStartCommand` | each container start | container |
| `postAttachCommand` | each `am attach` | container |

Each accepts `string | array | object` (the object form runs named commands in parallel).
Honour `waitFor`. A failing hook aborts the remaining hooks, matching the spec. Record
completion in the session record rather than container-side marker files — `am` has better
state than the CLI does.

`--skip-post-create` has no equivalent need; `devcontainer.skip_lifecycle = true` skips them.

**Non-goal for phase 1:** `userEnvProbe`. Interactive panes get the login environment
naturally; document that non-interactive `am run` may see a thinner env.

### Getting the agent into the image

The project's devcontainer has *their* toolchain, not `claude`. Selected by
`devcontainer.agent_install`:

- **`feature`** — inject via `--additional-features` at build time, so it is baked into the
  cached image. Claude Code has an official Feature:
  `ghcr.io/anthropics/devcontainer-features/claude-code:1` (needs Node; installs it on
  Debian/Ubuntu/Alpine/Fedora/RHEL if absent). Verified in the spike: the injected Feature is
  ordered *before* the `devcontainer.json` snippet in the label, contributes no entrypoint or
  mounts of its own, and lands `claude` on `PATH`. Per-agent mapping in config. No official
  Features exist for copilot/gemini/codex → they fall through.
- **`bootstrap`** — an `am`-owned install script into a named volume (`am-agent-<name>`),
  run before the agent and added to `PATH`. Works on any base image, cached across sessions.
- **`none`** — the devcontainer already provides it.
- **`auto`** (default) — `feature` if mapped, else `bootstrap`, else `none` if already on
  `PATH` in the built image.

Credentials reuse `container::resolve_agent_auth` unchanged, against the container home
derived above. Env-var auth (`OPENAI_API_KEY`, `GH_TOKEN`) maps to `-e` as it does today.

### Trust

A `devcontainer.json` is repo-controlled code, and `am` exists to *isolate* agents.

- **`initializeCommand` runs on the host.** Refuse by default; require
  `devcontainer.allow_host_commands = true`. (Owning the run path means `am` simply never
  executes it, rather than hoping a CLI flag suppresses it.) The spike confirms the delegated
  half is safe too: `devcontainer build` neither runs `initializeCommand` nor emits it into
  the label, so the only host-side execution in devcontainer mode is `am`'s own. Feature
  install scripts still run *inside* the build, which is the normal container threat model.
- **Escalating options** — `privileged`, `capAdd`, `securityOpt`, `--network=host` or extra
  `-v` in `runArgs`, and `mounts` touching sensitive host paths — are summarized and confirmed
  once, VS Code workspace-trust style, keyed by the config hash so a changed config re-prompts.
  Denied options are dropped, not fatal.
- **`container.network = "none"`** applies to the run step only; the build step needs network
  for features. Detect and explain rather than failing opaquely.
- **Credential exposure** — `~/.ssh` and `~/.claude` get mounted into an image the repo
  defines. Same risk as today's `container.image` escape hatch, but it becomes the common
  path. Say so in the docs.

### Compose

`dockerComposeFile` configs need run-time orchestration `am` would have to own. Detect and
error with "not yet supported" in phases 1–2.

### Session state

```rust
pub struct SessionContainer {
    pub runtime: String,
    pub mode: ContainerMode,           // Image | Devcontainer
    pub image: String,                 // am-dc-<hash> in devcontainer mode
    pub config_path: Option<PathBuf>,
    pub config_hash: Option<String>,
    pub remote_user: Option<String>,
    pub lifecycle_done: Vec<String>,   // which create-time hooks have run
    pub container_id: Option<String>,
}
```

Deserialization must tolerate older records (default `mode` to `Image`).

### Config surface

```toml
[container]
mode = "auto"                    # "image" | "devcontainer" | "auto"

[devcontainer]
path = ".devcontainer/devcontainer.json"
cli = "devcontainer"             # AM_DEVCONTAINER_BIN
agent_install = "auto"           # "feature" | "bootstrap" | "none" | "auto"
allow_host_commands = false
skip_lifecycle = false
home = "/home/vscode"            # override derived container home
extra_features = {}

[agents.claude]
devcontainer_feature = "ghcr.io/anthropics/devcontainer-features/claude-code:1"
```

### `am doctor`

Report: devcontainer CLI present + version, Node ≥ 20, config discovered, whether the built
image is current, and whether the config uses unsupported (compose) or gated
(`initializeCommand`, `privileged`) constructs.

---

## Implementation

New module `devcontainer.rs` beside `container.rs`:

- `find_config(worktree, override) -> Result<Option<PathBuf>>`
- `parse_config(&Path) -> Result<DevcontainerJson>` — JSONC + substitution, only the
  properties `am` consumes
- `config_hash(...) -> String`
- `build_build_command(...) -> Vec<String>` / `build(...) -> Result<String>` (image name)
- `read_image_metadata(runtime, image) -> Result<Vec<MetadataSnippet>>` — `podman inspect`,
  accepting array or bare object
- `merge(json, metadata) -> ResolvedConfig` — the spec's merge rules, pure and unit-testable
- `lifecycle::run(...)` — hook execution

`container.rs` grows a `ResolvedConfig` parameter on `build_run_command`; image mode passes a
default. Path-handling rules unchanged: `&Path` parameters, conversion at the argv boundary.

`cmd_start` ordering: the config lives in the worktree, so the worktree must be created before
discovery/build. Split preflight into pre-worktree (runtime, CLI, agent credentials, slug) and
post-worktree (discovery, build, trust), and add a `WorktreeGuard` with an explicit `commit()`
so any post-worktree failure rolls the worktree back.

## Tests

- `AM_DEVCONTAINER_BIN` → a script logging args and printing
  `{"outcome":"success","imageName":"am-dc-abc"}`, matching the `AM_TMUX_BIN`/`AM_PODMAN_BIN`
  pattern. `AM_PODMAN_BIN` mock returns a canned `devcontainer.metadata` label.
- Unit: discovery precedence and the multi-folder error; hash stability and invalidation;
  `--docker-path` placed after the subcommand; merge rules (union, boolean-OR, last-wins,
  json-last) against spec examples; array *and* bare-object metadata; entrypoint composition;
  container home for root vs named user; trust gate drops `privileged` without opt-in and
  refuses `initializeCommand`; lifecycle once-only via the session record; compose rejected.
- Integration (cucumber): `am init` → `am start` with mocked CLI+runtime → `am list` shows
  devcontainer mode → `am destroy`; second start on an unchanged config makes **no** CLI call;
  worktree rollback when the build fails.
- Real CLI output captured in the spike lives in
  [`tests/fixtures/devcontainer/`](../tests/fixtures/devcontainer/) — see its README.
  `features-metadata-label.json` is the merge fixture (base-image inheritance, two
  entrypoint-contributing Features, object- *and* string-form mounts, an unsubstituted
  `${localWorkspaceFolder}`, a Feature-contributed `securityOpt`).
  `properties-metadata-label.json` is the boundary fixture: assert that `runArgs`,
  `workspaceFolder`, `workspaceMount`, and `initializeCommand` are **absent** from it, so a
  future CLI release that starts emitting them shows up as a failing test rather than a
  missed simplification.
- Remaining live check (phase 1, not automatable in CI): git *and* jj worktrees usable inside a
  container built from a real devcontainer config.

## Phases

0. ~~**Spike**~~ — **done 2026-08-09**, see *Spike results*.
1. ~~**Phase 1**~~ — **done 2026-08-10.** Mode selection, discovery, hash/caching, build,
   merge, run, lifecycle hooks, agent injection, trust gate, worktree rollback, session
   state, docs; git and jj both. Landed as `src/devcontainer.rs` plus a `DevcontainerRuntime`
   parameter on `container::build_run_command` and a `plan_container` split in `cmd_start`.
   Two deviations from this plan, both forced by `am` running containers with `--rm`:
   create-time lifecycle hooks re-run on every start (the previous container's filesystem is
   gone, so anything they installed must be reinstalled), and `postAttachCommand` is not run
   at all because `am attach` moves tmux focus rather than attaching to the container.
2. **Phase 2** — `userEnvProbe`, `forwardPorts`, vendored CLI bundle if friction warrants.
3. **Phase 3** — compose, if worth owning.
4. **Optional** — replace the build step with a native Rust feature-builder, ideally as its own
   crate. The run path is unaffected by design.

## Spike results

Run 2026-08-09 against `@devcontainers/cli` 0.88.0 (installed locally, not globally) with
podman, Node v26.7.0. Configs: `mcr.microsoft.com/devcontainers/base:bookworm` plus
`docker-outside-of-docker` and `sshd` (chosen because they contribute entrypoints, mounts, and
`securityOpt`); a second config exercising every property `am` might consume; a third with a
nonexistent base image.

**The run path is Node-free after build — confirmed, with more margin than assumed.** The
label carries feature-contributed `entrypoint` *and* `mounts`, and also embeds the whole
`devcontainer.json` as its final element. No `read-configuration` call is needed. `am` still
parses `devcontainer.json`, but only for the six properties in the table above.

**`build` exits 1 on `outcome: "error"`.** Verified directly: a nonexistent base image gave
exit 1 with `{"outcome":"error","message":"Command failed: podman pull …","description":"An
error occurred building the container."}` on stdout, build chatter on stderr.

**Four things that were not on the question list and change the implementation:**

1. **No variable substitution in the label.** `${localWorkspaceFolder}` survives verbatim —
   convenient, but it means substitution is `am`'s job for label-sourced properties too, not
   just for `devcontainer.json`.
2. **`runArgs`, `workspaceFolder`, `workspaceMount`, and `initializeCommand` are absent from
   the label.** The metadata schema drops them. This is the whole reason `am` still needs a
   JSONC parser; without these four it could have read the label alone.
3. **Mounts are heterogeneous** — objects from Features, strings from `devcontainer.json`,
   both in one array. The parser needs an untagged enum.
4. **Base-image metadata is merged in automatically.** The `mcr` base contributed
   `common-utils`, `git`, and `{"remoteUser":"vscode"}` without either config asking. `am`
   inherits correct precedence for free by walking the array left-to-right.

**Operational notes.** `--docker-path podman` works; podman has no buildx, the CLI falls back
cleanly. It emits a harmless `one or more build args were not consumed:
[BUILDKIT_INLINE_CACHE _DEV_CONTAINERS_FEATURE_CONTENT_SOURCE]` warning — do not treat build
stderr as failure. JSONC (comments, trailing commas) parsed fine. `imageName` came back as an
array in every case.

**Agent injection works.** `--additional-features
'{"ghcr.io/anthropics/devcontainer-features/claude-code:1":{}}'` built clean, ordered the
injected Feature before the `devcontainer.json` snippet, contributed no entrypoint or mounts,
and put `claude` (2.1.197) on `PATH` for the `vscode` user.

## Resolved: the default mode

**`auto` is the default.** Phase 1 shipped with `image` while this was open; it was flipped
once the implementation existed to judge.

The argument for `image` was that `auto` changes what `am start` does for any repo that
already has a `.devcontainer/`. That reads the change the wrong way round. A repo that has
taken the trouble to describe its environment means for that description to be used —
preferring an `am`-specific image over it is the surprising behaviour, and it is exactly the
duplication this feature exists to remove. Repos with no config are unaffected, because
`auto` falls back to an image.

The escape hatch is `mode = "image"`, and every error for an unsupported construct names it.

No open questions remain.

---

# Phase N: `am` builds the image itself

The original decision above — *delegate the build, own the run* — was right about where the
seam goes and wrong about how much lives on the build side. This phase moves the common case
across that seam, keeping the CLI as a fallback.

## What the spike found

Reverse-engineering `@devcontainers/cli` 0.88.0 with `--log-level trace` (it leaves its
generated Dockerfile and staged Feature tree in `/tmp/devcontainercli-vscode/`) turned the
"real project" estimate into something much smaller:

1. **Feature resolution needs no layer download.** The OCI manifest's `dev.containers.metadata`
   annotation carries the whole `devcontainer-feature.json` — options with defaults,
   `installsAfter`, `customizations`. Ordering and option resolution are decidable from three
   small GETs. Blobs are needed only for the install itself.
2. **The registry protocol is three requests**, and following the `WWW-Authenticate` challenge
   rather than hardcoding ghcr's token endpoint makes it work against any registry. No OCI
   client library, and no async runtime dragged into a synchronous codebase — `ureq` + `tar`.
3. **Dockerfile generation is templating.** Docker does the real work. The CLI's three-stage
   content-image dance exists because it must accommodate arbitrary user build contexts; `am`
   resolves the base image first and owns the context, so one stage is equivalent.
4. **The layer is a plain tar** despite the `.tgz` in its title annotation — sniff magic bytes.
5. **Metadata properties are emitted in schema order, not declaration order.** A config writing
   `remoteUser` before `containerEnv` still produces `containerEnv` first. This is invisible to
   `merge()` and matters only for byte-comparison, which is precisely why it is worth pinning.

## Why this is safe to do incrementally

The label is the entire contract. Both builders emit the same `devcontainer.metadata`, and
`merge`/`finalize`/the trust gate/the run path are shared and cannot tell them apart. That
makes the correctness question empirical rather than argued: build the same config both ways
and diff the label. Two `#[ignore]`d differential tests in `src/devcontainer/native/mod.rs` do
exactly that, against labels captured from real CLI runs, and both match byte for byte —
including a base image that carries its own inherited label and a `${localWorkspaceFolder}`
that has to survive Docker's variable expansion to be substituted later by the run path.

## Scope

Implemented natively: a base `image` or `build.dockerfile`, and Features pulled from an OCI
registry, ordered by the spec's round-based algorithm over both `dependsOn` (hard, resolved
recursively) and `installsAfter` (soft, ordering only).

Falls back to the CLI, naming the construct: `dockerComposeFile`,
`overrideFeatureInstallOrder`, and Features referenced by local path or tarball URL — including
one reached through another Feature's `dependsOn`. `devcontainer.builder = "native"` turns the
fallback into an error, for users who want a guarantee that no config silently reintroduces
Node.

## `dependsOn`, and the ordering bug it uncovered

Implementing `dependsOn` meant implementing the spec's ordering algorithm properly, and that
turned out to fix a defect in what shipped first.

**The install order is round-based.** Each round takes *every* Feature whose dependencies are
already placed, sorts that whole group by the spec's "Round Stable Sort", and commits it. The
first implementation read the rule as "repeatedly take the first eligible Feature", which is
the natural reading and is wrong: given `a` after `b` and `c` after `d`, one-at-a-time yields
`b, a, d, c` while the spec yields `b, d, a, c`. Any config with two *independent*
`installsAfter` chains diverges — and since `installsAfter` appears in nearly every published
Feature while `dependsOn` appears in almost none, this was the bug that actually mattered.

**`devcontainer features resolve-dependencies` is the cheap oracle.** The reference CLI will
walk the graph and print the install order without building anything. That turns an ordering
check from a multi-minute image build into a few manifest GETs, and it is what
`install_order_matches_the_reference_cli_resolver` uses. Reach for it before reaching for a
differential build.

**A dependency appears in the label under the id its dependent wrote.** Verified by building a
local Feature whose `dependsOn` names a registry Feature: the label carried
`ghcr.io/…/apt-get-packages:1`, the id as written in the `dependsOn` map, not the resolved
digest form the resolver reports. So dependencies contribute to the label exactly like
config-declared Features.

**Feature equality is contents plus options, not the written id.** Two ids resolving to the
same digest with the same options are one install; the same id with different options is two.
`am` keys identity on the layer digest, which is what makes a diamond collapse and a
`dependsOn` cycle terminate during resolution rather than during sorting.

## Known gaps

- **`dependsOn` has no differential test.** The ordering algorithm and the graph resolver are
  unit-tested, and the resolver is checked against the CLI for `installsAfter` — but no Feature
  in the common registries declares `dependsOn` (15 popular ones were checked, none did), so
  the recursive-fetch path is never exercised against the reference implementation. Closing
  this needs a Feature published for the purpose.
- **`overrideFeatureInstallOrder` is now cheap.** It is the spec's `roundPriority`: commit only
  the maximum-priority nodes of each round and return the rest to the worklist. The round
  machinery it needs is in place, so this is a small change rather than the structural one it
  used to be.
- **Registry auth is anonymous only.** Private Feature registries need `docker config.json`
  credentials and credential helpers; today they fall back to the CLI only if the ref happens
  to be non-registry, otherwise they fail with the registry's own 401 text.
- **The build-context hash gap is unchanged.** `config_hash` still does not cover files the
  Dockerfile `COPY`s. The native builder now knows the context and could close this — a
  git-aware hash of tracked files under it — but that is a separate change affecting both
  builders, so it was left alone.

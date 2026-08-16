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
| `workspaceMount` | Added as a mount *alongside* host-path mirroring, so both paths resolve |
| `initializeCommand` | Only to detect and refuse it — `am` never runs it |
| `dockerComposeFile` | Drives the compose run model |
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

`dockerComposeFile` configs need run-time orchestration `am` has to own — see *Compose is a
second run model* below. Phases 1–2 detected them and errored.

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

Implemented natively: a base `image` or `build.dockerfile`, and Features from all three sources
the spec defines — an OCI registry, a local path, or a tarball URL — ordered by the spec's
round-based algorithm over `dependsOn` (hard, resolved recursively), `installsAfter` (soft,
ordering only), and `overrideFeatureInstallOrder` (`roundPriority`).

Compose projects are supported too, which leaves **no fallback at all**. The last remaining
`Unsupported` case was "the config names nothing to build from" — and that is not an `am`
limitation: the reference CLI rejects the same configs with "No image information specified in
devcontainer.json", and `build.dockerfile` has no default there either. Delegating it asked a
second tool the same unanswerable question, and when the CLI was not installed it answered a
typo'd config with "install Node". It is an error now, naming what to add.

That emptied `Unsupported`, and with it the fallback machinery. The CLI delegation went next:
with nothing selecting it and nothing needing it, `devcontainer.builder`, `devcontainer.cli`,
`AM_DEVCONTAINER_BUILDER`, `AM_DEVCONTAINER_BIN`, the `devcontainer build` invocation and its
result-JSON parsing, and `doctor`'s CLI and Node checks are all gone.

**The design in *Why this over the alternatives* has therefore inverted, and it is worth saying
why that is not an admission the split was wrong.** The build/run split was chosen so the Node
dependency sat behind one replaceable call, and predicted that a Rust builder would "swap that
one call" and leave the run path untouched. That is exactly what happened — the run path never
learned which builder produced an image, because the `devcontainer.metadata` label was the only
contract between them. The spike's estimate of the build half (~8,000 lines, "a project with its
own cadence") was the part that proved wrong: reading the reference CLI's actual output rather
than the specification made it about a tenth of that.

The reference CLI is still a dependency of the *test suite* — the differential tests build the
same configs both ways and compare labels, and `devcontainer features resolve-dependencies` is
the oracle for install order. That is a development-time dependency, not a runtime one, and it
is what keeps the builder honest.

## Compose is a second run model, not a builder change

The build half of compose is nearly free: `devcontainer build` on a compose config builds the
*service's* image with Features baked in and stamps the same `devcontainer.metadata` label, so
`am`'s builder only had to learn where the base image comes from — the service's own `image:` or
`build:`, read back from the runtime.

The run half is the actual feature, and it is why this was refused for so long. A compose config
is a whole project, so `am` brings it up, execs the agent into the named service, and takes it
down on destroy. Three decisions worth recording:

- **`am` never parses YAML.** Compose files use anchors, `extends`, interpolation and profiles;
  re-implementing that would be a second source of truth that drifts. The resolved model comes
  from `compose config --format json`, and the override `am` contributes is *written* as JSON —
  which compose accepts, because JSON is valid YAML. Correct quoting for paths and env values
  falls out for free, with no new dependency.
- **The override is a separate file layered last**, never an edit of the project's own. Nothing
  `am` does can corrupt a file the repo owns, and `am`'s contribution wins on conflict because
  compose merges later files over earlier ones.
- **`container.network = "none"` is refused rather than ignored.** Compose services reach each
  other over the project network, so honouring it would cut the agent off from the very services
  the config exists to provide. It is a security control; silently dropping it would be worse
  than refusing.

`am` contributes to the agent's service only. The rest of the project — the database, the cache
— is left exactly as the repo described it, and the compose file stays responsible for keeping
the service alive (`command: sleep infinity`, per the devcontainer convention), because the spec
defaults `overrideCommand` to false for compose.

## The three Feature sources differ only in where the bytes come from

Once a Feature's directory exists on disk, everything downstream — options, ordering, staging,
the label — is identical. The differences are confined to fetching:

| | metadata from | identity | cacheable |
|---|---|---|---|
| Registry | the manifest annotation | layer digest | yes, digests are immutable |
| Local | `devcontainer-feature.json` on disk | its resolved path | n/a |
| Tarball | the same file, after unpacking | sha256 of the bytes fetched | no, a URL is mutable |

Identity follows the spec's equality rule in each case. A local Feature has no content hash
because the spec says every local Feature is distinct — so its path *is* its identity, and two
directories with byte-identical contents are still two Features. A tarball is hashed from the
bytes, so two URLs serving the same archive are one Feature. Only the registry case can be
looked up in the cache before it is fetched; a tarball must be downloaded every build because
there is no immutable name to check first, and only the unpacking is skipped on a hit.

Three details found by running the reference CLI rather than by reading the spec:

- **The staging directory is named from the Feature's declared `id`, not from where it came
  from.** A local Feature in a folder called `folderx` declaring `"id": "featy"` is staged as
  `featy_0`. For a registry Feature the two normally coincide, which is why this went unnoticed.
- **A tarball's filename is load-bearing.** The CLI requires `devcontainer-feature-<id>.tgz` and
  refuses anything else. It also rejects `http://` outright, treating it as a malformed registry
  reference rather than a URL.
- **A local Feature's label id is the path as written** (`./folderx`), like every other source.

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

## `overrideFeatureInstallOrder` is a priority, not an order

It reads like a list of Features to install in that sequence. It is not. Each entry raises the
named Feature's `roundPriority` to `n - idx`, and a round commits only its highest-priority
members, returning the rest to the worklist. Two consequences, both verified against the CLI:

- **It cannot jump a dependency.** Eligibility is decided first and priority only breaks ties
  among Features already ready to install. Raising `git` above `common-utils` does not move
  `git` first; `git` still waits, and the override only decides that it beats its round-mates.
- **It splits rounds.** Raising a Feature that shares a round with others takes that round
  alone and sends the rest back to compete again. A "sort the round by priority" shortcut
  produces the same answer in easy cases and the wrong one here, which is why
  `override-order-devcontainer.json` raises `common-utils` specifically.

Entries match either the fully qualified name without its tag or the Feature's short alias
(`git`); the CLI accepts both. One deliberate divergence: an entry that matches nothing being
installed is ignored, where the CLI resolves every entry and errors if it cannot fetch it. Such
an entry cannot change any ordering, so the label is identical either way — and the label is
the contract.

## Ports, and the label-order bug they uncovered

`forwardPorts` is an *editor* key: the reference CLI publishes nothing for it, writing it into
the label for an editor to act on. `am` has no editor, so it publishes — bound to `127.0.0.1`,
which is both the conservative reading of "forward this to me" and what the CLI does for a bare
`appPort`. Not publishing would leave the key inert, which is what it was.

Adding it required fixing something else first, found by asking the CLI what it emits:

**Features and configs use different metadata schemas, in different orders.** `am` had one list
for both, taken from the Feature order. Against a real `devcontainer build`:

- a **Feature** contributes `init, privileged, capAdd, securityOpt, entrypoint, mounts,
  customizations` — and nothing else. A Feature declaring `containerEnv` or `forwardPorts` has
  them dropped.
- a **`devcontainer.json`** contributes 23 properties in a different order entirely:
  `onCreateCommand` first, `customizations` seventh, `init` eleventh, the three port keys near
  the end.

The shipped code therefore emitted config properties in Feature order — wrong for any config
setting more than one of them — and dropped `forwardPorts`, `portsAttributes` and
`otherPortsAttributes` from the label entirely, since the Feature list has no such keys. Both
fixtures in place at the time agreed with the CLI by luck: each exercised one or two keys whose
relative order happens to match under either list. `ports-devcontainer.json` exercises seven at
once and fails loudly if the lists are ever merged again.

## userEnvProbe: capture the environment, do not run inside it

The reference CLI resolves the container user's shell and runs
`<shell> -lic 'cat /proc/self/environ'`, parsing the NUL-separated result and applying it to the
processes it starts. `/proc/self/environ` rather than `env` is the detail worth keeping: it is
NUL-separated, so a value containing a newline survives.

`am` does the same, as a shell snippet ahead of the agent in the command it already composes for
Feature entrypoints. The tempting shortcut — run the agent under `bash -lic` directly — was
rejected for two reasons:

- A `.bashrc` that prints a banner, enables job control, or `exec`s another shell would land in
  the agent's own process tree. Probing puts all of that in a throwaway process instead.
- Precedence. `am` sets `containerEnv`, `remoteEnv`, agent credentials and the jj identity
  deliberately; under a login shell a dotfile could overwrite any of them silently. The
  generated snippet skips exactly the names `am` set, derived from the same inputs the run
  command emits `-e` flags for so the two cannot drift.

The probe is joined to the agent with a newline rather than `&&`: finding no variables is not a
failure. Feature entrypoints keep their `&&`, because one of those failing genuinely should stop
the session.

The spec's default is `loginInteractiveShell`, so this applies to every devcontainer session
unless the config opts out — a behaviour change, and the point of the property. Image-mode
sessions have no config to ask for a probe and are untouched.

## postAttachCommand needs an exec, which is why it waited

Every other lifecycle hook runs against a container `am` is in the middle of creating, so it can
be chained into the command. `postAttachCommand` is the exception: the spec runs it every time a
tool attaches, and `am attach` frequently attaches to a session that is already live, where the
only thing it does is move tmux focus. There is no new container command to chain anything into.

So the hook is reached two ways:

- **Chained**, when a container is being created — `am start`, and the `am attach` paths that
  recreate a gone container. Starting a session is also attaching to it.
- **`exec`'d into the running container**, when `am attach` finds one already up.

The hooks for the second route come from the **image's metadata label**, not from re-reading the
`devcontainer.json`. The label describes the container that is actually running; a config edited
since the session started describes one that does not exist yet. This is the same reason the run
path reads everything else from the label.

It stays out of `startup_commands` and therefore out of `lifecycle_done`, which tracks hooks
meant to run once per container. This one is meant to run every time.

The exec is best-effort and never fails the attach: it runs on the path whose entire job is
switching to a window that already works. A config with no `postAttachCommand` execs nothing, so
the common case costs no extra process at all.

## Spec details that only a probe would have caught

Four of these were found by review rather than by the differential tests, which is worth noting:
the label is the contract between builders, so anything that does not change the label — option
*names*, identity, reference parsing — is invisible to them.

- **Option names are normalised, not just uppercased.** `my-option` becomes `MY_OPTION`. The rule
  was measured by building a Feature whose options exercise each case: every non-word character
  becomes `_`, then a *leading run* of digits and underscores collapses to a single `_`, then the
  whole thing is uppercased. So `2fa` → `_FA` (replaced, not prefixed), `12ab` → `_AB`,
  `__dunder` → `_DUNDER`, but `a--b` → `A__B` — repeated separators are not collapsed anywhere
  except at the front. Uppercasing alone produced `MY-OPTION`, which is not a valid shell
  assignment, so the generated env file failed to source and took the Feature install with it.
- **Identity is the manifest digest, not the layer's.** Two manifests can share a layer while
  differing in metadata or `dependsOn`; keying on the layer would collapse them into one install
  and silently drop the other's dependencies.
- **A digest-pinned reference is a distinct form.** `…/git@sha256:…` has a colon that is not a
  tag separator, and the spec allows the form anywhere a Feature is named, `dependsOn` included.
- **A tarball must be HTTPS.** Refused at fetch time rather than at parse time, deliberately:
  classifying `http://` as anything else leaves it read as a registry reference with the host
  `http:`, and "no such host" is a worse answer than naming the real problem.

## The lockfile closes the staleness gap without a network round trip

`am` names an image by hashing its inputs and skips the build when that image exists. A Feature
from a registry has no input to hash: resolving `…/git:1` means asking the registry, and doing
that per `am start` would undo the caching the hash exists to provide.

`devcontainer-lock.json` is the ecosystem's answer, and `am` now reads and writes it in the
reference format. It records the digest each id resolved to; hashing *that file* stands in for
hashing the Features. A moved tag changes the lockfile, which changes the image name, which
rebuilds — and the fast path fetches nothing.

The same file solves reproducibility, which is what it was designed for: a registry Feature is
fetched at its recorded digest rather than its tag, so two people building the same config get
the same Feature.

Details worth keeping:

- **`integrity` is the manifest digest** for a registry Feature, which is the same value the
  identity fix above keys on — the two are the same question. For a tarball it is the hash of the
  downloaded bytes, and a mismatch is an **error**: it is the only way to detect that a mutable
  URL changed, and installing different code than the file records would make it worse than
  useless.
- **Local Features are excluded**, per the spec. `am` hashes their files directly instead, which
  is cheaper and exact.
- **Adopting a lockfile renames the image once.** A repo that had none gets a different hash on
  the next start and rebuilds — a one-time cost, mostly a layer-cache hit.
- **The write happens during a build, not on every start**, since the builder only runs when the
  image is missing.

The format is pinned against the reference implementation two ways: the resolver differential
test compares `am`'s resolved `repo@digest` strings to the CLI's, and a test asserts `am`'s
rendering of *this repo's own committed lockfile* is byte-identical — so `am` writing it does
not produce a spurious diff.

## Private registries reuse the runtime's login

`am` reads the auth files `docker login` and `podman login` write, and their credential helpers.
It asks for nothing and stores nothing: a user who can already pull the *image* can already pull
the *Feature*, which is the only property worth having here.

The two runtimes disagree on where the file lives, so all five locations are consulted —
`$REGISTRY_AUTH_FILE`, `$DOCKER_CONFIG/config.json`, `~/.docker/config.json`,
`$XDG_RUNTIME_DIR/containers/auth.json`, `~/.config/containers/auth.json` — in that order. The
format is identical either way.

Two details worth keeping:

- **Credentials go to the token endpoint, not the API endpoint.** A registry answers `401` with a
  bearer challenge; the credentials buy a token scoped to the private repository, and the token
  is what the API call carries. A registry that challenges with `Basic` instead is handled
  directly.
- **The lookup is memoised per registry.** A build resolves one manifest per Feature and they
  usually share a registry, so without it a keychain-backed helper would prompt once per
  Feature.

Nothing is logged. A credential in an error message is a credential in a terminal scrollback.

## `<runtime> compose` is not a given

Docker ships Compose as a plugin and podman grew the subcommand in **4.7**. podman 4.3 — what
Debian 12 and Ubuntu 22.04 ship — has none, and answers `am`'s invocation with `unknown
shorthand flag: 'f'`, which is a baffling way to say "your podman is too old".

`am` resolves a provider instead of assuming one: `<runtime> compose` if it answers `version`,
else a standalone `docker-compose`. That is the same binary `podman compose` delegates to
internally, so the fallback is what a newer podman would have done anyway — including pointing it
at podman's socket via `DOCKER_HOST`, which `am` fills in from `podman info` rather than guessing
a path that varies between rootless, root, and machine setups.

`podman-compose` is deliberately excluded. It is a separate implementation, and version 1.0.3 has
no `config` subcommand at all — `config --format json` being exactly what lets `am` read a
compose file without carrying a YAML parser. Selecting it would fail later and less clearly, so
the error names it and says it will not do.

## A Feature's containerEnv is baked in, not carried in the label

This was the worst bug in the builder and the differential tests could not see it.

The reference CLI drops a Feature's `containerEnv` from its label snippet — which an early probe
here observed and recorded as "the CLI drops it". What that probe did not ask was where it went
*instead*: the CLI emits it as `ENV` in the generated Dockerfile, before that Feature's own
install step so later Features inherit it. `am` did neither, so the property vanished.

That is the entire toolchain contract for `go`, `node`, `python`, `rust`, `java`, `ruby` and
others: they install into a prefix and put it on `PATH` this way. Under the old builder the
Feature installed and its tools were not on `PATH`. Because the CLI omits it from the label too,
a broken build and a working one produce **identical labels** — the difference is in the image's
`Config.Env`, which no label comparison inspects.

The lesson generalises beyond this one property: *the label is the contract between builders, so
anything the label does not carry is invisible to a test that compares labels.* The differential
test added with the fix asserts the built image's environment as well.

The same review found the two `*_METADATA_KEYS` lists had drifted — the Feature list was missing
all five lifecycle hooks (so a Feature's own `postCreateCommand` never ran; `git-lfs` declares
one to pull its artifacts) and the config list was missing `shutdownAction`. Both lists are now
transcribed from the CLI's `pickFeatureProperties`/`pickConfigProperties` and asserted whole,
because a probe can only reveal the properties the probed Feature happened to declare — which is
exactly how both omissions survived.

## Variable substitution

Four rules, each of which was wrong in a way that only shows up in a running container:

- **An unknown variable is left literal**, not collapsed to `""`. Every unmatched branch in the
  reference implementation returns the original text. Collapsing was actively harmful:
  `${devcontainerId}` became the empty string, so a Feature naming a volume
  `dind-var-lib-docker-${devcontainerId}` produced the *same* name in every session — two
  `docker-in-docker` sessions on one host ran two daemons over one `/var/lib/docker`, which is
  the exact collision the variable exists to prevent.
- **`${devcontainerId}` is the session's container name** — derived from the repository path and
  the slug, so it is unique on the host and stable across rebuilds, which is what the spec asks.
  Deriving it from the config would have renamed a Feature's volumes on every config edit.
- **`${containerEnv:VAR}` resolves against the container's environment**, which is the image's
  own plus the config's contributions — not the config's `containerEnv` alone. The documented
  idiom `"PATH": "${containerEnv:PATH}:/extra"` used to yield `:/extra` and *replace* the
  image's `PATH`, leaving the agent with almost nothing resolvable.
- **`${localEnv:VAR:default}` supports defaults**, and `${env:VAR}` is an accepted alias.

Two ordering details: `workspaceFolder` is substituted *before* it becomes the value
`${containerWorkspaceFolder}` expands to, since substitution does not re-scan its own output and
`/workspaces/${localWorkspaceFolderBasename}` is the common spelling; and Feature entrypoints are
substituted, which the spec requires for `${devcontainerId}`.

## The run path: workspaceMount and the container's UID

**`workspaceMount` is honoured by *adding* a mount, not by replacing the mirroring.** It was
parsed, substituted, and then never read, so a config pairing it with `workspaceFolder` pointed
`--workdir` at a path nothing was mounted at — the agent started in an empty, root-owned
directory. The reference CLI mounts the workspace only at that target; `am` mounts it there *and*
at its host path, because the mirroring is what makes a git worktree's absolute `gitdir:` pointer
and a jj workspace's relative repo path resolve. Both paths are the same bind, so this is a
superset rather than a compromise. An explicit `mounts` entry on the same target wins, since two
mounts on one target is a runtime error.

**`updateRemoteUserUID` is applied on the Docker path.** Podman's `--userns=keep-id:uid=,gid=`
already did the right thing. Docker skipped the numeric mapping whenever the config named a
user, on the reasoning that a devcontainer user is uid 1000 and therefore the same mapping by
another name — true only for a host user who is also 1000. Everyone else got a container that
could not write its own worktree.

`am` maps the *process* rather than rewriting the image's passwd entry as the CLI does, which is
a smaller hammer with one consequence worth stating: a bare numeric uid has no passwd entry, so
`HOME` is set explicitly to the path the credential mounts already use.

## Who runs what

Measured against the reference CLI rather than reasoned from the prose, because the prose
distinguishes `containerUser` from `remoteUser` without saying which applies to a Feature
entrypoint:

| | runs as |
|---|---|
| the container itself | `containerUser` — **root** unless the config says otherwise |
| a Feature entrypoint | the container user |
| `postCreateCommand` and the other hooks | `remoteUser` |
| the agent | `remoteUser` |

`am` used to run all of it as `remoteUser`, so a `docker-in-docker` entrypoint starting `dockerd`
or an `sshd` one binding a privileged port failed — and because entrypoints are `&&`-chained
ahead of the agent, the failure took the whole session with it rather than degrading.

The container now starts as the container user when a Feature contributes an entrypoint, and
drops to `remoteUser` with `su` for the hooks and the agent. `exec` on the drop keeps the pane's
tty and leaves the agent as PID 1's direct child, so signals and exit codes still propagate.
**With no entrypoint nothing is dropped** — there is nothing needing elevation, the container runs
as the remote user exactly as before, and the common path is unchanged.

One consequence worth stating, because it collides with `updateRemoteUserUID` above: the
container has to *start* as the container user to run an entrypoint, so the numeric UID mapping
cannot also apply. When privileges are dropped the agent runs as the image's remote user, whose
UID may differ from the host's. The reference CLI reconciles this by rewriting the image so that
user's UID matches; `am` does not. A test asserts the two cannot both emit a `--user` flag.

## Compose: what stops, and what survives

**`down -v` was deleting the project's data.** `-v` removes *named* volumes, not merely the
anonymous ones a comment here claimed — so a compose file declaring `volumes: { postgres-data: }`
lost its database on every `am destroy`. Outliving a session is the entire point of naming a
volume. `am destroy` now runs a plain `down`: the containers and the network go, the data stays,
and anonymous volumes are still collected because nothing references them afterwards.

**`shutdownAction` decides what happens when the session ends**, which is the spec's "the tool
window was closed" — not what `am destroy` does. Destroy is an explicit instruction and always
takes the project down; refusing would leave a project `am` no longer tracks. A single container
is already `--rm`, which *is* `stopContainer`; a compose project outlives its pane unless
something stops it, so the pane chains a `compose stop` after the agent exits. `stop` rather than
`down`, so `am attach` brings it back. `"none"` opts out.

**`runServices` narrows what starts.** The agent's own service is always included: a list that
forgets it is an easy mistake, and starting a project with nowhere to run the agent is a worse
answer than quietly adding it. An empty list stays empty rather than becoming an enumeration,
so services the compose file gains later are still started.

## Known gaps

- **`dependsOn` has no differential test.** The recursive walk is exercised offline through
  local Features — transitive pull-in, diamond dedup, and cycle termination all have unit
  tests — and the ordering is checked against the CLI for `installsAfter`. What is missing is a
  comparison of the two implementations on a `dependsOn` graph, because no Feature in the
  common registries declares one (15 popular ones were checked, none did). Closing this needs
  a Feature published for the purpose.
- **Tarball Features have no differential test.** The CLI's *resolver* accepts one served from
  a local TLS server, but its *build* path refuses to fetch from one even with the certificate
  trusted, so no reference label can be produced locally. Unpacking is unit tested for both
  plain and gzipped archives and everything after the fetch is shared with the other two
  sources, but the HTTP fetch itself, and the end-to-end comparison that backs everything else
  here, are untested for this case.
- **`postAttachCommand` runs once per `am attach`, not once per human attach.** tmux has no
  event for "the user looked at this window", so re-running `am attach` on a live session runs
  the hook again. Idempotent hooks are unaffected; an appending one is not.
- **The probe's variable list is line-based.** `/proc/self/environ` is read NUL-separated and
  converted to lines, so a value containing a literal newline is truncated at it. The reference
  CLI parses the NUL stream directly and does not have this limit.
- **`portsAttributes` and `otherPortsAttributes` are carried but not acted on.** They exist to
  tell an editor how to treat a forwarded port — a label, whether to open a browser — and `am`
  has no equivalent. They now reach the label, which is what a downstream reader needs.
- **With a Feature entrypoint present, the agent's UID is the image's, not the host's.** The
  container must start as the container user to run the entrypoint, so the numeric mapping is
  skipped. A host user whose UID differs from the image's remote user may find bind-mounted
  files unwritable in that configuration. Closing it means rewriting the image the way the
  reference CLI does.
- **A repo with no lockfile still cannot detect a moved tag.** Hashing the lockfile is what
  makes registry Features participate; without one there is nothing to hash, and `--rebuild`
  remains the answer. `am` writes the file on its first build, so this self-corrects.
- **A port conflict surfaces as a runtime failure**, not a preflight one. `am` does not check
  whether a forwarded port is already bound before starting the session.
- **A typo'd `overrideFeatureInstallOrder` entry is silently ignored** rather than being the
  error the CLI raises. Harmless for the label, but it does mean a misspelled entry quietly
  does nothing. Closing it means resolving every entry against the registry — a network round
  trip spent purely on validation.
- **The build-context hash gap is unchanged.** `config_hash` still does not cover files the
  Dockerfile `COPY`s. The native builder now knows the context and could close this — a
  git-aware hash of tracked files under it — but that is a separate change affecting both
  builders, so it was left alone.

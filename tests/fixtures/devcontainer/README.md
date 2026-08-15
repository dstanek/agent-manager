# Dev container fixtures

Real output captured during the phase 0 spike for
[`specs/devcontainer-support.md`](../../../specs/devcontainer-support.md). These are not
hand-written — they came from `@devcontainers/cli` 0.88.0 driving podman on 2026-08-09, so
they encode the CLI's actual behaviour rather than our reading of the specification.

Use them for the `devcontainer.rs` merge and parse unit tests. Regenerate only if the CLI's
label format changes; hand-editing defeats the purpose.

## Files

| File | What it is |
| --- | --- |
| `features-devcontainer.json` | Input config: base image + two Features that contribute entrypoints and mounts |
| `features-metadata-label.json` | The `devcontainer.metadata` label the build produced from it |
| `properties-devcontainer.json` | Input config exercising every property `am` might consume |
| `properties-metadata-label.json` | Its label — the authority on which properties survive into the label and which do not |
| `build-success.stdout.json` | `devcontainer build` stdout on success (exit 0) |
| `build-error.stdout.json` | `devcontainer build` stdout on a nonexistent base image (exit 1) |

Labels are pretty-printed for reviewability. Podman emits them as one line; JSON is
whitespace-insensitive, so parsers see the same document either way.

## What each label is meant to catch

`features-metadata-label.json` is the merge fixture. In one document it covers:

- **Base-image inheritance** — `common-utils`, `git`, and `{"remoteUser":"vscode"}` come from
  `mcr.microsoft.com/devcontainers/base:bookworm`; neither config asked for them. Array order
  encodes precedence, so a left-to-right walk gets this right for free.
- **Two entrypoint-contributing Features** — `docker-outside-of-docker` and `sshd`. Both
  entrypoints must survive into the composed `sh -c` invocation; dropping either is the
  failure mode this fixture exists to catch.
- **Both mount shapes in one array** — an object from `docker-outside-of-docker`, a string
  from `devcontainer.json`. The mount parser needs an untagged enum over the two.
- **An unsubstituted variable** — `${localWorkspaceFolder}` survives verbatim into the label.
  The CLI does no substitution here, so `am` must apply its own to label-sourced properties,
  not just to `devcontainer.json`.
- **`securityOpt` from a Feature** (`label=disable`), which the trust gate must surface.

`properties-metadata-label.json` is the boundary fixture. Its input sets every property `am`
might want; the label shows which ones the metadata schema keeps and which it drops. The four
it drops are the entire reason `am` still needs a JSONC parser:

- `runArgs`
- `workspaceFolder`
- `workspaceMount`
- `initializeCommand` — absent from the label, and never executed by `build`

A test asserting these are *missing* is as valuable as the ones asserting the rest are
present: if a future CLI release starts emitting them, `am` can drop a dependency.

Both input configs use JSONC — comments and a trailing comma — which the CLI accepted. Any
parser we pick has to as well.

## `native/` — fixtures for `am`'s own builder

Captured the same way, from `@devcontainers/cli` 0.88.0 driving Docker on 2026-08-11. These
exist so `am`'s builder can be diffed against the reference implementation rather than against
our reading of the spec.

| File | What it is |
| --- | --- |
| `git-devcontainer.json` | Input: a plain base image plus one registry Feature |
| `cli-git-label.json` | The label the CLI produced from it |
| `features-devcontainer.json` | Input: a base image that *already carries a label*, two Features with an `installsAfter` relationship, and a `${localWorkspaceFolder}` in `containerEnv` |
| `cli-features-label.json` | Its label — the authority on base-image inheritance and on variables surviving the build |
| `git-oci-manifest.json` | The OCI manifest for `ghcr.io/devcontainers/features/git:1` |
| `git-devcontainer-feature.json` | That Feature's own `devcontainer-feature.json` |
| `cli-Dockerfile.extended` | The Dockerfile the CLI generates to install Features |
| `cli-Dockerfile.buildContent` | Its throwaway `FROM scratch` content image |
| `cli-builtin.env`, `cli-git_0-features.env`, `cli-git_0-install-wrapper.sh` | The Feature install contract: the env files and the generated wrapper that sources them |
| `two-chains-devcontainer.json` | Input: four Features forming *two independent* `installsAfter` chains — the shape that tells the round-based install order apart from a one-at-a-time one |
| `ports-devcontainer.json` | Input: seven config-level metadata properties at once, including `forwardPorts` and `portsAttributes` |
| `cli-ports-label.json` | Its label — the authority on the **config** metadata order, which differs from the Feature one |
| `override-order-devcontainer.json` | The same four, with an `overrideFeatureInstallOrder` raising `common-utils` — chosen because it makes the override *split* a round rather than merely reorder one |

### What these catch

- **`cli-*-label.json`** are compared byte-for-byte by the `#[ignore]`d differential tests in
  `src/devcontainer/native/mod.rs`. Run them with
  `cargo test --bin am -- --ignored` (add `AM_TEST_NO_CACHE=1` to force a cold build).
  They need Docker and network access to ghcr.io, which is why they are not in `cargo test`.
- **Key order is part of the comparison.** The CLI emits metadata properties in *schema* order,
  not the order the config declares them — a `devcontainer.json` writing `remoteUser` before
  `containerEnv` still yields `containerEnv` first. `FEATURE_METADATA_KEYS` and
  `CONFIG_METADATA_KEYS` in `native/feature.rs` encode the two orders — they are genuinely
  different lists — and `cli-ports-label.json` is what pins the config one.
- **`two-chains-devcontainer.json`** is fed to `devcontainer features resolve-dependencies`,
  which prints the CLI's install order without building anything — so that differential test
  costs a few manifest GETs rather than minutes. Prefer it when the question is about ordering;
  the label fixtures only pin the order of the Features they happen to contain. The four
  Features are chosen so the two orderings differ: one-at-a-time selection gives
  `gh-release, act, common-utils, git`, the spec's rounds give `gh-release, common-utils, act,
  git`.
- **`cli-ports-label.json`** is what stops the two metadata key lists being merged back into
  one. Features and configs use different schemas *in different orders* — a Feature emits
  `customizations` last, a config emits it seventh — and the other label fixtures each exercise
  too few keys to tell the two apart, so they agreed with the CLI by luck. This one exercises
  seven properties at once and fails loudly.
- **`cli-Dockerfile.extended`** documents the install contract `am` reproduces: where Features
  are copied, the `_CONTAINER_USER`/`_REMOTE_USER` env files, and the `getent` home probe.
  `am` deliberately generates a *simpler* Dockerfile (one stage, not three) — this fixture is
  the reference for the parts that must not diverge, not a target to match line for line.

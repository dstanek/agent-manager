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

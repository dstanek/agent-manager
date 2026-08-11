# Dependency Updates

[Renovate](https://docs.renovatebot.com/) keeps the project's dependencies
current. It runs self-hosted from a scheduled GitHub Actions workflow rather than
as the hosted app, so nothing has to be installed on the repository beyond one
secret.

- Config: [`.github/renovate.json5`](https://github.com/dstanek/agent-manager/blob/main/.github/renovate.json5)
- Workflow: [`.github/workflows/renovate.yml`](https://github.com/dstanek/agent-manager/blob/main/.github/workflows/renovate.yml)

## Schedule

The workflow's cron — Mondays at 06:00 UTC — is the schedule. The Renovate config
deliberately has no `schedule` of its own: a second, narrower window would only
produce runs that silently do nothing. Change the cadence in the workflow.

The workflow also runs on `workflow_dispatch` (with optional `debug`/`trace`
logging and a dry-run toggle) and on any push to `main` that touches the config,
so a config change takes effect immediately instead of a week later.

## What is managed

| Manager | Files | Notes |
|---|---|---|
| `cargo` | `Cargo.toml`, `Cargo.lock` | Non-major bumps are grouped into one PR; majors are separate |
| `github-actions` | `.github/workflows/*.yml` | All actions grouped into one PR |
| `dockerfile` | `dockerfiles/`, `examples/`, `.devcontainer/Dockerfile` | `FROM` base images; non-majors grouped. Ubuntu uses `ubuntu` versioning restricted to `YY.MM`, or its dated rebuild tags (`questing-20260610`) parse as newer than `25.10` |
| `devcontainer` | `.devcontainer/devcontainer.json` | Feature versions (`ghcr.io/devcontainers/features/*`) |
| `pip_requirements` | `requirements-docs.txt` | `rangeStrategy: bump`, so the `>=` floors actually move |
| `custom.regex` | Any `Dockerfile*` | Versions in `ARG`/`ENV` lines carrying a `# renovate:` annotation |
| `custom.regex` | `.devcontainer/devcontainer.json` | Versions passed as feature *options*, carrying a `// renovate:` annotation |
| `custom.regex` | `src/*.rs` | OCI references compiled in as defaults, carrying a `// renovate:` annotation |

Lock file maintenance is enabled, so `Cargo.lock` gets a periodic refresh of
transitive crates instead of only moving when a direct dependency happens to be
bumped.

Commits use the `chore(deps):` type. `git-cliff` skips `^chore`, so dependency
bumps follow the repo's Conventional Commits rule without filling the changelog.

## Annotating a version Renovate cannot see

The `dockerfile` manager only reads `FROM` and `COPY --from` lines. A tool
version pinned in an `ARG` is invisible to it, so it needs an annotation naming
the datasource and the upstream package:

```dockerfile
# renovate: datasource=github-releases depName=jj-vcs/jj
ARG JJ_VERSION=0.44.0
```

The custom manager in `renovate.json5` matches that comment followed by an
`ARG`/`ENV` line whose name ends in `_VERSION`. A `versioning=` field may be
appended to the comment when the default (`semver`) is wrong. Upstream tags with
a leading `v` are stripped, since the download URLs in these Dockerfiles add it
back themselves.

The same manager also reads a bare shell assignment in a workflow `run:` block,
so a version pinned in CI is annotated identically:

```yaml
run: |
  # renovate: datasource=github-releases depName=jj-vcs/jj
  JJ_VERSION=0.44.0
```

Four versions are annotated this way today:

- `JJ_VERSION` in `.devcontainer/Dockerfile` — the jj release the project is
  developed against
- `JJ_VERSION` in `.github/workflows/ci.yml` — the same release, used by the
  cucumber suite's "a jj repository" step
- `GO_VERSION` in `examples/Dockerfile.golang`
- `RENOVATE_VERSION` in `.github/workflows/renovate.yml` — see below

The two jj pins are the reason one manager covers both file types. They name the
same `depName`, so Renovate bumps them in a single PR. Split across two managers
they would still update, but nothing would stop one landing without the other —
and a CI that tests against a different jj than local development is exactly what
the pin exists to prevent.

A second custom manager does the same for devcontainer **feature options**. The
`devcontainer` manager updates the feature reference (`node:1`) but never looks
inside the option object passed to it, so the Node line needs its own
annotation — with `//` comments, since `devcontainer.json` is JSONC:

```jsonc
"ghcr.io/devcontainers/features/node:1": {
  // renovate: datasource=node-version depName=node versioning=node
  "version": "22"
}
```

`node` versioning reads a bare major as a range, so Renovate rewrites this to
`"24"` rather than pinning `"24.19.0"` — the feature keeps resolving the latest
patch itself. That shape is preserved by the versioning scheme, not by a
`rangeStrategy` setting; setting one here has no effect.

## Two annotations worth knowing about

**`src/config.rs`** ships `ghcr.io/anthropics/devcontainer-features/claude-code:1`
as a compiled-in default for the `claude` agent. That is an external OCI artifact
every user receives, and no built-in manager can see it — the `devcontainer`
manager reads `devcontainer.json` files, not Rust source. Without the annotation
it would sit at `:1` forever with nothing to surface a `:2`. The neighbouring
`ghcr.io/dstanek/am-*-minimal:latest` defaults are this project's own images at
`:latest`, so there is nothing to pin there.

**`RENOVATE_VERSION`** in the workflow is read by both the validate job (via
`npx --package renovate@$RENOVATE_VERSION`) and the scheduled run (via the
action's `renovate-version` input). They have to agree on the major: otherwise a
config using a newer schema validates green and then fails at runtime, which is
precisely the delayed failure the validate job exists to catch. It is pinned to
the **major only** — Renovate ships several releases a week, so a full version
pin would open a PR for each one while changing nothing that matters. `npm`
versioning reads a bare major as a range, so Renovate rewrites it to `45`, never
to `44.23.3`.

## What is deliberately not managed

Some versions have nothing for Renovate to bump, because nothing is pinned:

- **jj in `Dockerfile.claude` / `Dockerfile.copilot`** and **Terragrunt in
  `examples/Dockerfile.terragrunt`** resolve GitHub's `releases/latest` redirect
  at build time. These are agent images, not the tested dev environment — they
  should track upstream, which is why `.devcontainer/Dockerfile` pins jj and
  these do not.
- **Claude Code** (`claude.ai/install.sh`), **rustup**, **NodeSource LTS**, and
  the globally installed **`@github/copilot`** npm package all install latest.
- **Distro packages** (`apt-get install`, `apk add`) follow whatever the base
  image's package index offers.

One thing is still manual: **`.devcontainer/devcontainer-lock.json`** is not
updated by the `devcontainer` manager. Re-resolve it with the Dev Containers CLI
after changing a feature.

`tests/fixtures/**` is in `ignorePaths` — the devcontainer configs there are
recorded test inputs and expected CLI output, so bumping a version in them
changes what the suite asserts rather than what ships.

## What checks a dependency PR

`ci.yml` covers `src/**`, `tests/**`, and the Cargo manifests, so it gates crate
bumps and lock file maintenance.

Everything else is gated by
[`images-ci.yml`](https://github.com/dstanek/agent-manager/blob/main/.github/workflows/images-ci.yml),
which builds each Dockerfile — amd64 only, never pushed — plus the dev container
via the Dev Containers CLI, the same path `am` takes in devcontainer mode. That
is what verifies a base image bump, `JJ_VERSION`, `GO_VERSION`, and the docs
requirements. It lives in its own workflow because GitHub path filters are
workflow-level: widening `ci.yml`'s filter would spend a macOS and a Windows
runner cross-building Rust that a Dockerfile change cannot affect.

`dockerfiles/Dockerfile.am-dev` is not built there. It starts `FROM
am-rust:latest`, a tag only `make build-am-dev` produces, and `.devcontainer/`
has replaced it for development.

## Setup

The workflow needs a `RENOVATE_TOKEN` repository secret. GitHub exposes no API
for creating personal access tokens, so this is a web UI step:
[create a fine-grained PAT](https://github.com/settings/personal-access-tokens/new)
scoped to this repository with the permissions Renovate documents:

| Permission | Access | Why |
|---|---|---|
| Contents | Read and write | Push branches |
| Pull requests | Read and write | Open and update PRs |
| Issues | Read and write | The dependency dashboard |
| Workflows | Read and write | Edit `.github/workflows/*.yml` |
| Commit statuses | Read and write | Read CI results for automerge decisions |
| Dependabot alerts | Read-only | Vulnerability-driven updates |
| Metadata | Read-only | Required by GitHub on every fine-grained token |

**Workflows is not optional.** GitHub rejects any push that touches
`.github/workflows/` from a token lacking it, so without it every GitHub Actions
bump fails — and it fails at push time, not at startup, so the token check in the
workflow will not catch it.

Then set the secret, which does have a `gh` command:

```sh
gh secret set RENOVATE_TOKEN --repo dstanek/agent-manager
```

A classic PAT with the `repo` and `workflow` scopes works too, and is what
Renovate's docs suggest first — it is simply much broader.

The built-in `GITHUB_TOKEN` is not used. Pull requests it creates do not trigger
other workflows, so CI would never run on a Renovate PR.

## Validating a config change

A malformed config makes the scheduled run fail a week later, so the workflow
validates before it runs — and validates on its own pull requests without
executing. To check locally:

```sh
npx --yes --package renovate renovate-config-validator --strict
```

To see what Renovate would actually extract from the working tree, without a
token or network writes:

```sh
npx --yes --package renovate renovate --platform=local --dry-run=extract
```

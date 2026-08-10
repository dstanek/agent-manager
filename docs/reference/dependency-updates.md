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
| `dockerfile` | `dockerfiles/`, `examples/`, `.devcontainer/Dockerfile` | `FROM` base images; non-majors grouped |
| `devcontainer` | `.devcontainer/devcontainer.json` | Feature versions (`ghcr.io/devcontainers/features/*`) |
| `pip_requirements` | `requirements-docs.txt` | `rangeStrategy: bump`, so the `>=` floors actually move |
| `custom.regex` | Any `Dockerfile*` | Versions in `ARG`/`ENV` lines carrying a `# renovate:` annotation |

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

Two versions are annotated this way today:

- `JJ_VERSION` in `.devcontainer/Dockerfile` — the jj release the project is
  developed and tested against
- `GO_VERSION` in `examples/Dockerfile.golang`

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

Two more are pinned but intentionally manual:

- **The Node major in `.devcontainer/devcontainer.json`** (`"version": "22"` on
  the Node feature). Renovate manages the *feature* version, not the option
  passed to it; managing the option would replace the coarse major with a hard
  patch pin and lose "latest 22.x". Bump it by hand.
- **`.devcontainer/devcontainer-lock.json`** is not updated by the `devcontainer`
  manager. Re-resolve it with the Dev Containers CLI after changing a feature.

`tests/fixtures/**` is in `ignorePaths` — the devcontainer configs there are
recorded test inputs and expected CLI output, so bumping a version in them
changes what the suite asserts rather than what ships.

## Setup

The workflow needs a `RENOVATE_TOKEN` repository secret: a fine-grained PAT with
**Contents: Read and write**, **Pull requests: Read and write**, and **Issues:
Read and write** on this repository. Issues access is for the dependency
dashboard.

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

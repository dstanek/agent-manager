# Custom Container Images

`am` works with any container image — the images published by this project are a convenient starting point, not a requirement. A **custom image** lets you bake in exactly the tools your project needs so every agent session starts in a fully-prepared environment without any extra setup.

For most real projects, building a project-specific image is the recommended approach. It gives you full control over the environment, speeds up agent sessions by pre-caching dependencies, and makes the setup reproducible across machines.

---

## Choosing a starting point

This project publishes two tiers of images for each agent:

| Image | Contents | Best for |
|---|---|---|
| `ghcr.io/dstanek/am-claude` / `ghcr.io/dstanek/am-copilot` | Agent + git + jj + ripgrep + fd + jq + neovim + build tools | Quickly trying `am` without any setup |
| `ghcr.io/dstanek/am-claude-minimal` / `ghcr.io/dstanek/am-copilot-minimal` | Agent + git only | Base for project-specific images; smaller download |

The full images are convenient for exploration. For real projects, start from a minimal image and add only what your project needs.

---

## When to use a custom image

The stock `am-claude` and `am-copilot` images contain a general-purpose set of tools (git, Node.js, ripgrep, jq, etc.). They are sufficient for many projects, but you will want a custom image when:

- Your project requires a specific language runtime (Rust, Go, Python, JVM, …) or version
- You need project-specific tools (linters, build systems, test runners)
- You want to pre-cache heavy dependencies so agent sessions start faster
- You need precise control over what is installed in the agent's environment

Custom images are recommended for any project where agents will actually build, test, or run code.

---

## The only hard requirement

Whatever base image you choose, **the agent binary must be present on the container's `PATH`**. Credentials are never baked in — `am` mounts them from your host at session start.

| Agent | Required binary |
|---|---|
| `claude` | `claude` |
| `copilot` | `gh` with the Copilot extension |
| `gemini` | `gemini` |
| `codex` | `codex` |

Only these four agent names are valid. Using any other value with `--agent` will produce an error before any session is created.

---

## Option 1: Extend an `am` image

The simplest approach is to layer your project's tooling on top of an existing `am` image. The base image already has the agent installed; you just add what your project needs.

### Agent-agnostic examples

The `examples/` directory contains ready-to-use Dockerfiles for common project types. Each one accepts a `BASE_IMAGE` build argument so you can use it with any agent without maintaining separate files per agent:

```sh
# Claude
podman build --build-arg BASE_IMAGE=ghcr.io/dstanek/am-claude-minimal:latest \
    -f examples/Dockerfile.python -t my-project:latest .

# Copilot — same Dockerfile, different base
podman build --build-arg BASE_IMAGE=ghcr.io/dstanek/am-copilot-minimal:latest \
    -f examples/Dockerfile.python -t my-project:latest .
```

Available examples:

| File | Language / toolchain |
|---|---|
| `examples/Dockerfile.python` | Python 3 + pip + venv |
| `examples/Dockerfile.rust` | Rust via rustup |
| `examples/Dockerfile.golang` | Go via official tarball |
| `examples/Dockerfile.terraform` | OpenTofu (or Terraform) |
| `examples/Dockerfile.terragrunt` | Terragrunt + OpenTofu (or Terraform) |

Each example also includes a commented-out dependency pre-caching block. Uncomment and adapt it to your project's manifest files so that the cached layer survives source-code rebuilds.

### The pattern

Using `ARG BASE_IMAGE` in your own Dockerfile keeps it agent-agnostic:

```dockerfile
ARG BASE_IMAGE=ghcr.io/dstanek/am-claude-minimal:latest
FROM ${BASE_IMAGE}

USER root
RUN apt-get update && apt-get install -y your-tools \
 && rm -rf /var/lib/apt/lists/*

USER am
WORKDIR /workspace
```

### Extending an image for one project

`dockerfiles/Dockerfile.am-dev` is a worked example of this pattern. It extends
`am-rust:latest` (built from `examples/Dockerfile.rust`) and adds only what one
project needs on top — a git identity for tests that run `git commit`, and a
pre-fetched Cargo registry so incremental builds start warm:

```dockerfile
FROM am-rust:latest

RUN git config --global user.email "dev@example.com" \
 && git config --global user.name "Dev"

WORKDIR /workspace
COPY --chown=am:am Cargo.toml Cargo.lock ./
RUN mkdir -p src tests \
 && echo 'fn main() {}' > src/main.rs \
 && echo 'fn main() {}' > tests/cucumber.rs \
 && cargo fetch \
 && rm -rf src tests
```

Build it with:

```sh
make build-am-dev   # also builds am-rust:latest as a dependency
```

!!! note "`am` itself now develops in a dev container"
    This three-image chain is kept as an example of the layering pattern, but it
    is no longer how the project builds its own environment — `.devcontainer/`
    is. See [Dev Containers](devcontainers.md) and
    [Building from Source](../reference/building.md#developing-in-a-dev-container).
    That switch is the argument for this whole page in miniature: one checked-in
    file that editors, CI, and `am` all read beats three Dockerfiles that have to
    be built locally and in order.

---

## Option 2: Build from scratch

You do not have to use an `am` base image at all. Start from any image you like, install your language toolchain and project tools, then install the agent on top.

Here is a minimal example for a Python project using Claude Code:

```dockerfile
FROM python:3.12-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y curl git \
 && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash am \
 && mkdir -p /workspace && chown am:am /workspace

USER am
ENV HOME=/home/am

RUN curl -fsSL https://claude.ai/install.sh | bash
ENV PATH="/home/am/.local/bin:${PATH}"
ENV DISABLE_AUTOUPDATER=1

# Install project dependencies
COPY --chown=am:am requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

WORKDIR /workspace
```

The key steps are:
1. Start from whatever base image suits your project.
2. Create the `am` user and install the agent binary.
3. Pre-install project dependencies against a static manifest file (not the live source tree, which is mounted at runtime).

---

## Configuring `am` to use your image

Set the agent and override its image in `.am/config.toml`:

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "am-dev:latest"
```

The `[agents.<name>]` entry tells `am` both which credential mount preset to use and which image to run. The image name does not have to match the agent name — an image named `am-dev` can still use the `claude` preset.

---

## Tips

**Keep credentials out of the image**

Never `RUN` an authentication command (`claude auth login`, `gh auth login`, etc.) inside a Dockerfile. `am` mounts credentials from your host at session start. Baking them into an image makes the image unsafe to share or publish.

**Rebuilding after dependency changes**

When you update `Cargo.toml`, `package.json`, `requirements.txt`, or similar manifests, rebuild the image to refresh the pre-cached layer. The image acts as a warm dependency cache — it is worth keeping up to date.

**Testing the image manually**

Before using an image with `am`, verify it works standalone:

```sh
podman run --rm -it am-dev:latest bash
# inside the container:
cargo --version
claude --version
```

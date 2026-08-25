# Installation

## Prerequisites

Before installing `am`, make sure you have the following tools available:

- **Podman** (preferred) or **Docker** — required for container isolation. [Install Podman](https://podman.io/docs/installation) or [Install Docker](https://docs.docker.com/engine/install/).
- **A git or jj repository** — `am` must be run from inside an initialized repository. `am` detects `.jj/` first, then falls back to `.git/`.
- **A container image with your agent installed** — by default `am` runs agents inside a container, so the agent software (e.g. Claude Code, GitHub Copilot) must be present in the image, not on the host. Pre-built images are available at `ghcr.io/dstanek/am-claude:latest` and `ghcr.io/dstanek/am-copilot:latest`. See the [Claude Code guide](../guides/claude-code.md) or [GitHub Copilot guide](../guides/github-copilot.md) for full setup instructions. If you run with `--no-container`, the agent executable must be on your host `PATH` instead.
- **tmux** *(optional)* — required only for split-pane sessions. Without it, `am` still creates the worktree and launches the container, but no terminal window is opened. Install with your system package manager (`apt install tmux`, `brew install tmux`, etc.) or see the [tmux wiki](https://github.com/tmux/tmux/wiki/Installing).

Container isolation is enabled by default but can be disabled per-session with `--no-container` or globally in the project config.

---

## Install methods

=== "Shell script (Linux & macOS)"

    The fastest way to get started. The script downloads the appropriate binary for your platform and installs it to `~/.local/bin`:

    ```sh
    curl -fsSL https://raw.githubusercontent.com/dstanek/agent-manager/main/install.sh | sh
    ```

    Make sure `~/.local/bin` is on your `PATH`. If it isn't, add the following to your shell profile (`.bashrc`, `.zshrc`, etc.):

    ```sh
    export PATH="$HOME/.local/bin:$PATH"
    ```

    To install to a different directory, set `AM_INSTALL_DIR` before running the script:

    ```sh
    curl -fsSL https://raw.githubusercontent.com/dstanek/agent-manager/main/install.sh \
        | AM_INSTALL_DIR=/usr/local/bin sh
    ```

=== "Homebrew (macOS & Linux)"

    If you use [Homebrew](https://brew.sh/), tap the `dstanek/am` formula:

    ```sh
    brew tap dstanek/am
    brew install am
    ```

    To upgrade later:

    ```sh
    brew upgrade am
    ```

=== "Debian / Ubuntu"

    Download the `.deb` package for your architecture from the [latest release](https://github.com/dstanek/agent-manager/releases/latest), then install it with `dpkg`:

    ```sh
    # x86_64
    sudo dpkg -i am-x86_64-unknown-linux-gnu.deb

    # aarch64 (ARM64)
    sudo dpkg -i am-aarch64-unknown-linux-gnu.deb
    ```

    You can also download and install in one step:

    ```sh
    # Adjust the filename for your architecture
    curl -fsSL -O https://github.com/dstanek/agent-manager/releases/latest/download/am-x86_64-unknown-linux-gnu.deb
    sudo dpkg -i am-x86_64-unknown-linux-gnu.deb
    ```

=== "Red Hat / Fedora"

    Download the `.rpm` package for your architecture from the [latest release](https://github.com/dstanek/agent-manager/releases/latest), then install it with `rpm`:

    ```sh
    # x86_64
    sudo rpm -i am-x86_64-unknown-linux-gnu.rpm

    # aarch64 (ARM64)
    sudo rpm -i am-aarch64-unknown-linux-gnu.rpm
    ```

    On Fedora 37+ or RHEL 9+, you can use `dnf` instead:

    ```sh
    sudo dnf install ./am-x86_64-unknown-linux-gnu.rpm
    ```

=== "Windows"

    Download `am-x86_64-pc-windows-msvc.zip` from the [latest release](https://github.com/dstanek/agent-manager/releases/latest), extract the archive, and place `am.exe` somewhere on your `PATH`.

    !!! note
        Windows support is experimental. Container isolation requires Docker Desktop (Podman on Windows is not yet tested). tmux is not available natively; consider using `am` inside WSL2 for the full experience.

=== "Build from source"

    Building from source requires [Rust](https://rustup.rs) 1.88 or later.
    See [the minimum supported Rust version](../reference/building.md#the-minimum-supported-rust-version)
    for why that number moves.

    ```sh
    git clone https://github.com/dstanek/agent-manager.git
    cd agent-manager
    cargo build --release
    ```

    The compiled binary will be at `target/release/am`. Copy it to a directory on your `PATH`:

    ```sh
    cp target/release/am ~/.local/bin/am
    ```

---

## Verify the installation

After installing, confirm that `am` is on your `PATH` and the version is correct:

```sh
am --version
```

Expected output:

```
am 0.3.0
```

To see all available commands and options:

```sh
am --help
```

!!! tip
    If the shell reports `am: command not found`, check that the install directory is included in your `PATH` and restart your shell session (or run `source ~/.bashrc` / `source ~/.zshrc`).

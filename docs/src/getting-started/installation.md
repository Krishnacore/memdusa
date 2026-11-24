# Installation

memdusa is a single binary. Install it on each machine where an agent runs; a team server uses the same binary. The quickest way is Homebrew or the one-line installer.

## Install with Homebrew

```sh
brew install krishnacore/tap/memdusa
```

The formula supports macOS and Linux on arm64 and x86_64. Upgrade it with `brew upgrade memdusa`.

## Install with a script

If you do not use Homebrew, use the one-line installer for your platform. It downloads the release archive for your OS and architecture, checks its SHA-256 checksum and installs `memdusa` into `~/.local/bin` (Windows: `%LOCALAPPDATA%\memdusa\bin`).

macOS and Linux (arm64 or x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.sh | sh
```

Windows (PowerShell, x86_64):

```powershell
irm https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.ps1 | iex
```

If the install directory is not on your `PATH` yet, the installer prints the line to add to your shell's startup file (for example `~/.zshrc`, the default shell on macOS) and an `export` line for the current terminal. macOS does not put `~/.local/bin` on `PATH` by default.

Two environment variables control the script:

| Variable | Meaning |
|---|---|
| `MEMDUSA_VERSION` | Pin a release tag, for example `MEMDUSA_VERSION=v0.1.12`. Default: the latest release. |
| `MEMDUSA_INSTALL_DIR` | Change the destination directory. Default: `~/.local/bin` (Windows: `%LOCALAPPDATA%\memdusa\bin`). |

Set them for one command like this:

```sh
MEMDUSA_VERSION=v0.1.12 MEMDUSA_INSTALL_DIR=~/.local/bin sh -c \
  "$(curl -fsSL https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.sh)"
```

## Install a prebuilt binary

Download the archive for your platform from the GitHub releases page for the `v*` tag you want. Platforms are Linux x86_64 and arm64, macOS Intel and Apple Silicon, and Windows. Unpack it, make the file executable where needed, and put `memdusa` on your `PATH`.

## Build from source

From the repository root, with a Rust toolchain installed:

```sh
cargo install --path crates/memdusa
```

## Check the install

```sh
memdusa --version
memdusa status
```

`memdusa status` tells you whether this machine uses a local database or a remote server, and whether the server is reachable. Example output for a local install:

```text
mode: local database sqlite:/Users/alice/.memdusa/memdusa.db (schema ok, 0 users)
semantic search: off (set MEMDUSA_EMBED_URL to enable)
project: github.com/acme/shop
```

## Update memdusa

```sh
memdusa self-update
```

`self-update` downloads the latest release for your platform, verifies its SHA-256 checksum and replaces the running binary. Two variants:

| Command | What it does |
|---|---|
| `memdusa self-update --check` | Reports the available version without downloading anything. |
| `memdusa self-update --version v0.1.12` | Installs that specific version. |

A binary installed by Homebrew or cargo is not replaced: use `brew upgrade memdusa`, or rerun `cargo install --path crates/memdusa` on an updated checkout.

## Run the server image

```sh
docker run -p 7077:7077 -v memdusa:/data ghcr.io/krishnacore/memdusa
```

The image stores SQLite data in the `/data` volume. Set `MEMDUSA_DATABASE_URL=postgres://…` to use Postgres, and `MEMDUSA_ALLOWED_HOSTS` to your domain. Create the first user inside the container:

```sh
docker exec <container> memdusa user add alice --admin
```

This is enough to try a server locally. For deployment, TLS, authentication and OAuth sign-in, see [Deploying the server](../team/deploy.md) and [OAuth sign-in](../team/oauth.md).

## Connect a harness

```sh
memdusa install claude-code
```

One command registers the MCP server and installs that harness's hooks. See [Harnesses: overview](../harnesses/index.md) for the full list and [Quick start](quick-start.md) for the first things to try.

See also: [CLI reference](../reference/cli.md), [Configuration reference](../reference/configuration.md).

#!/bin/sh
# Install memdusa from a GitHub release, verifying the published SHA-256 checksum.
#
#   curl -fsSL https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.sh | sh
#
# Environment variables:
#   MEMDUSA_VERSION       release tag to install (default: the latest release, e.g. v0.1.1)
#   MEMDUSA_INSTALL_DIR   install directory (default: $HOME/.local/bin)
set -eu

repo="Krishnacore/memdusa"
base_url="https://github.com/$repo/releases"

usage() {
  cat <<EOF
memdusa installer

Usage:
  curl -fsSL https://raw.githubusercontent.com/$repo/main/install.sh | sh
  sh install.sh [--help]

Environment variables:
  MEMDUSA_VERSION       version to install, e.g. v0.1.1 (default: latest release)
  MEMDUSA_INSTALL_DIR   install directory (default: \$HOME/.local/bin)
EOF
}

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
  "")
    ;;
  *)
    printf 'error: unexpected argument: %s\n' "$1" >&2
    usage >&2
    exit 1
    ;;
esac

# Map the uname values onto the release target triple.
os="$(uname -s)"
case "$os" in
  Darwin) target_os="apple-darwin" ;;
  Linux) target_os="unknown-linux-gnu" ;;
  *)
    printf 'error: unsupported operating system: %s\n' "$os" >&2
    exit 1
    ;;
esac

arch="$(uname -m)"
case "$arch" in
  arm64 | aarch64) target_arch="aarch64" ;;
  x86_64 | amd64) target_arch="x86_64" ;;
  *)
    printf 'error: unsupported architecture: %s\n' "$arch" >&2
    exit 1
    ;;
esac

target="$target_arch-$target_os"

# Download a URL to a file; curl first, wget as a fallback.
fetch() {
  url=$1
  dest=$2
  if command -v curl >/dev/null 2>&1; then
    if ! curl -fsSL "$url" -o "$dest"; then
      printf 'error: failed to download %s (check MEMDUSA_VERSION)\n' "$url" >&2
      exit 1
    fi
  elif command -v wget >/dev/null 2>&1; then
    if ! wget -q -O "$dest" "$url"; then
      printf 'error: failed to download %s (check MEMDUSA_VERSION)\n' "$url" >&2
      exit 1
    fi
  else
    printf 'error: curl or wget is required\n' >&2
    exit 1
  fi
}

# The /releases/latest redirect ends at /releases/tag/<tag>, so this needs no
# API token and does not count against the unauthenticated API rate limit.
latest_version() {
  if command -v curl >/dev/null 2>&1; then
    final_url="$(curl -fsSL -o /dev/null -w '%{url_effective}' "$base_url/latest")" || {
      printf 'error: could not determine the latest memdusa release\n' >&2
      exit 1
    }
    version="${final_url##*/}"
  elif command -v wget >/dev/null 2>&1; then
    version="$(wget -S -O /dev/null "$base_url/latest" 2>&1 | grep -o 'releases/tag/[^[:space:]]*' | tail -n 1)" || true
    version="$(printf '%s' "$version" | tr -d '\r')"
    version="${version##*/}"
  else
    printf 'error: curl or wget is required\n' >&2
    exit 1
  fi
  case "$version" in
    v[0-9]*) printf '%s\n' "$version" ;;
    *)
      printf 'error: could not determine the latest memdusa release version\n' >&2
      exit 1
      ;;
  esac
}

version="${MEMDUSA_VERSION:-}"
if [ -n "$version" ]; then
  case "$version" in
    v*) ;;
    *) version="v$version" ;;
  esac
else
  version="$(latest_version)"
fi

archive="memdusa-$version-$target.tar.gz"
download_url="$base_url/download/$version"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

printf 'Downloading %s ...\n' "$archive"
fetch "$download_url/$archive" "$tmpdir/$archive"
fetch "$download_url/$archive.sha256" "$tmpdir/$archive.sha256"

printf 'Verifying SHA-256 checksum ...\n'
if command -v sha256sum >/dev/null 2>&1; then
  if ! (cd "$tmpdir" && sha256sum -c "$archive.sha256"); then
    printf 'error: checksum mismatch; aborting\n' >&2
    exit 1
  fi
elif command -v shasum >/dev/null 2>&1; then
  if ! (cd "$tmpdir" && shasum -a 256 -c "$archive.sha256"); then
    printf 'error: checksum mismatch; aborting\n' >&2
    exit 1
  fi
else
  printf 'error: sha256sum or shasum is required to verify the download\n' >&2
  exit 1
fi

tar xzf "$tmpdir/$archive" -C "$tmpdir"
binary="$tmpdir/memdusa-$version-$target/memdusa"
if [ ! -f "$binary" ]; then
  printf 'error: %s did not contain a memdusa binary\n' "$archive" >&2
  exit 1
fi
chmod +x "$binary"

install_dir="${MEMDUSA_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
cp "$binary" "$install_dir/memdusa"
chmod 755 "$install_dir/memdusa"
printf 'Installed %s\n' "$install_dir/memdusa"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    dollar='$' # printed literally in the PATH hints below
    # The startup file of the user's login shell, so the PATH change survives new terminals.
    case "$(basename "${SHELL:-sh}")" in
      zsh) rc="$HOME/.zshrc" ;;
      bash) if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
      fish) rc="" ;;
      *) rc="$HOME/.profile" ;;
    esac
    printf '\n%s is not on your PATH yet.\n' "$install_dir"
    if [ -n "$rc" ]; then
      printf 'Add it for new terminals, then open a new one (or run the export line in this one):\n'
      printf "  echo 'export PATH=\"%s:%sPATH\"' >> %s\n" "$install_dir" "$dollar" "$rc"
    else
      printf 'Add it with:\n  fish_add_path %s\n' "$install_dir"
    fi
    printf '  export PATH="%s:%sPATH"\n' "$install_dir" "$dollar"
    ;;
esac

printf '\n'
"$install_dir/memdusa" --version
printf '\nConnect your harness: memdusa install claude-code\n'
printf 'Other harnesses: codex, cursor, antigravity, gemini, opencode, pi, hermes, git.\n'

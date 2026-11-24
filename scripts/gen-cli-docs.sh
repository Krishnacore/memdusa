#!/bin/sh
# Backticks in the printf formats are Markdown, not command substitution.
# shellcheck disable=SC2016
# Regenerate docs/src/reference/cli.md from `memdusa --help` (every command and subcommand).
# Usage: scripts/gen-cli-docs.sh [path-to-memdusa]   (default: builds target/debug/memdusa)
set -eu
cd "$(dirname "$0")/.."
bin="${1:-}"
if [ -z "$bin" ]; then
  cargo build -q -p memdusa
  bin=target/debug/memdusa
fi
out=docs/src/reference/cli.md

# The subcommand names listed under "Commands:" in a help text (skipping `help`).
subcommands() {
  sed -n '/^Commands:/,/^$/p' | sed -n 's/^  \([a-z][a-z0-9-]*\) .*/\1/p; s/^  \([a-z][a-z0-9-]*\)$/\1/p' | grep -v '^help$' || true
}

section() ( # $1 = heading level, rest = command path; a subshell, so recursion keeps its own variables
  level=$1; shift
  hashes=$(printf '%*s' "$level" '' | tr ' ' '#')
  printf '%s `memdusa %s`\n\n```text\n' "$hashes" "$*"
  "$bin" "$@" --help | sed 's/[[:space:]]*$//'
  printf '```\n\n'
  for sub in $("$bin" "$@" --help | subcommands); do
    section $((level + 1)) "$@" "$sub"
  done
)

{
  printf '# CLI reference\n\n'
  printf 'Every `memdusa` command with its options, generated from `memdusa --help` by\n'
  printf '`scripts/gen-cli-docs.sh` (CI checks that this page is up to date). For what the commands are for,\n'
  printf 'start with the [quick start](../getting-started/quick-start.md) and the guides.\n\n'
  printf '## `memdusa`\n\n```text\n'
  "$bin" --help | sed 's/[[:space:]]*$//'
  printf '```\n\n'
  for cmd in $("$bin" --help | subcommands); do
    section 2 "$cmd"
  done
} > "$out"
echo "wrote $out"

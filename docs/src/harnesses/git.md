# Git hooks

Git hooks record commits, branch switches and merges in memdusa's journal, whoever made them: an agent, another tool or a human. They are installed per clone, in addition to any harness hooks.

## Install

```sh
memdusa install git
```

Run this inside the repository you want to track. The installer adds one line to each of `post-commit`, `post-checkout` and `post-merge`. It writes into `git rev-parse --git-path hooks`, so a custom `core.hooksPath` (for example Husky) is respected. Scope options do not apply: git decides the hook directory.

The added line runs `memdusa git-hook <hook> "$@"` in the background and ends with `|| true`. It cannot fail or block a git command, and it never prints. Existing hook scripts are kept: memdusa adds its line to them, placing it before a trailing `exit` when needed. Hooks that contained only a shebang and memdusa's line are deleted on uninstall.

## What gets recorded

| Git event | Hook | Recorded as |
|---|---|---|
| Commit | `post-commit` | `commit` |
| Branch switch or checkout | `post-checkout` | `checkout` |
| Merge | `post-merge` | `merge` |

Events go through the same hook pipeline as harness events, so secret redaction, the offline queue and remote mode apply. In remote mode the hooks send events to your team server; the project key is still detected from the local git remote.

## Trust and first run

No allowlist step is needed. The hooks are ordinary git hook scripts. Re-run `memdusa install git` in each clone; hooks are not copied when you clone a repository again. The install is idempotent: a re-install replaces memdusa's old line instead of adding a duplicate.

## Known limitations

- Installation is per clone, not per repository or per user.
- Events describe git activity only. Prompts, file edits and checkpoints come from harness hooks; git hooks do not capture them.
- The hook runs in the background. An event appears in the journal shortly after the git command, not before it returns.

## Verify it works

```sh
cat "$(git rev-parse --git-path hooks)/post-commit"   # one line ending in # memdusa
memdusa status
memdusa resume                                        # recent activity after a commit
```

Make a small commit in the repository, then check `memdusa resume` for the recorded activity. `--dry-run` prints the hook contents without writing them.

## Uninstall

```sh
memdusa uninstall git
```

Run it inside the same repository. Uninstall removes only memdusa's own lines from the three hook scripts. Your other hooks stay as they were, and a script that then contains nothing but a shebang is removed.

See also: [Harnesses overview](index.md), [Hook events](../reference/hooks.md), [Quick start](../getting-started/quick-start.md).

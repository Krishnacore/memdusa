# Quick start

This page takes you from a fresh install to an agent that resumes its own work, on one machine with a local
database. It takes about five minutes. You need memdusa [installed](installation.md) and one supported
harness, for example Claude Code.

## 1. Connect your harness

Run this once per machine (or per project, see [scopes](../harnesses/index.md)):

```sh
memdusa install claude-code     # or: codex, cursor, opencode, pi, antigravity, gemini, hermes
```

This installs the harness's hooks and registers memdusa's MCP server. Some harnesses need a one-time trust
step before their hooks run; the installer prints it, and each [harness page](../harnesses/index.md) explains
it.

## 2. Work as usual

Start a new session in a git repository and give the agent a task. You don't need to tell it about memdusa.
In the background memdusa:

- gives the agent a **resume brief** when the session starts,
- journals your prompts, the files it edits and the commands it runs,
- recalls relevant memories into each prompt,
- reminds the agent to save a **checkpoint** (goal, done, next) after it changes files, and writes a fallback
  checkpoint itself before compaction or when the session ends.

## 3. Pick up where you left off

End the session, or switch to another harness, and start a new session in the same repository. The agent
starts with the brief, so it knows what was being done and what comes next. To see what it sees:

```sh
memdusa resume
```

```text
# Resume brief: github.com/acme/shop

## Where things stand

### Goal: add a refunds module
_checkpoint 4m ago by claude-code_

**Done:**
- Added refund() with the over-refund guard

**Next:**
- Tests for the empty-reason check
...
```

## 4. Teach it things that should stick

Memories are facts, preferences, decisions, todos and notes that outlive sessions. Agents save them with the
`memory_save` tool; you can too:

```sh
memdusa remember "Prefers small PRs" --kind preference --global
memdusa remember "Amounts are integer cents" --kind decision
memdusa recall "cents"
```

`--global` makes a memory apply to every project. Without it, the memory belongs to the current project.

## 5. Check the setup

```sh
memdusa project    # the project key memdusa detected, e.g. github.com/acme/shop
memdusa status     # local database or remote server, and whether it is reachable
```

The project key comes from the git remote, so the same repository has the same key on every machine and in
every harness. See [Core concepts](concepts.md#project-key).

## Next steps

- [Core concepts](concepts.md): what memories, checkpoints, the journal and durable runs are.
- [Harnesses](../harnesses/index.md): details for your tool.
- [Multi-agent work and delegation](../guides/multi-agent.md): several agents on one project.
- [Importing past sessions](../guides/importing-sessions.md): bring in work done before memdusa.
- [Team server](../team/index.md): share memory across machines and people.

# Roadmap

What's done, what's next. Ideas and requests are welcome as
[GitHub issues](https://github.com/Krishnacore/memdusa/issues).

## Done

- Hooks for Claude Code, Codex, Cursor, OpenCode (1.x and 2.x), pi, Antigravity, Gemini CLI and Hermes:
  automatic resume, journaling, checkpoint reminders and fallback checkpoints. Git hooks for commits,
  branch switches and merges.
- Memories with full-text and semantic search (OpenAI-compatible APIs, Ollama, or a built-in local
  model), recall into each prompt, deduplication, and agent-driven review and consolidation.
- A task-aware resume brief with a fixed size budget.
- Durable runs with step leases, and multi-agent delegation across harnesses with takeover of stale
  tasks.
- Importing past sessions from Claude Code, Codex and pi transcripts.
- Remote mode, users and API keys, per-project access control and an audit log.
- An OAuth 2.1 server for MCP clients and the CLI, with GitHub and Google sign-in.
- Sync between a local database and a team server, with a background watcher.
- Installers: a one-line script for macOS, Linux and Windows, a Homebrew tap, a Docker image, and `memdusa self-update`.
- A security review of the team server, with every finding fixed (v0.1.12).

## Next

- Packages: Scoop and winget for Windows, `.deb`/`.rpm` and AUR for Linux, publishing to crates.io, and
  a homebrew-core submission.
- Importing OpenCode transcripts.
- Linking workers to their coordinator from harness hook fields (for example subagent ids), not only
  through environment variables.
- Cleanup of expired OAuth records on the team server.

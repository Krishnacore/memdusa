# memdusa docs style guide

The docs live in `docs/src/` and are built with mdBook (`mdbook serve docs`). Every page must also read
well as plain Markdown on GitHub.

## Audience and voice

- Readers are developers who use AI coding agents (Claude Code, Codex, Cursor, OpenCode, ...) and the
  people who run a memdusa server for a team. Assume they know a terminal; don't assume they know memdusa.
- Write in plain English, second person ("you"), present tense, active voice. Short sentences.
  One idea per paragraph.
- No marketing language ("blazingly", "seamless", "powerful"), no emoji, no exclamation marks.
- Use the same word for the same thing everywhere: *harness* (the agent tool), *project key*,
  *memory*, *checkpoint*, *brief* (the resume brief), *durable run*, *step*, *team server*,
  *remote mode*, *sync mode*.

## Page shape

1. `# Title` — what the page is about, in a few words.
2. One short paragraph: what this is and when you need it.
3. Task-oriented sections (`## Set up X`, `## Change Y`), each starting with the command or the
   one-sentence answer, then details.
4. End with related links when they help ("See also: [Access control](../team/access-control.md)").

Keep pages focused. If a section grows past one screen and has its own audience, it gets its own page.

## Accuracy rules (most important)

- Every command, flag, environment variable, default value, path and output shown must match the
  code. Check flags with `cargo run -q -p memdusa -- <command> --help` or by reading `crates/memdusa/src/main.rs`.
  Check behavior in the code when the old README is unclear. **The code wins over the old README.**
- Commands in code blocks must be copy-pasteable. Use `memdusa.example.com`, `alice`, `github.com/acme/shop`
  as example values. Never real hostnames, emails, tokens or personal data.
- Mark example output as such (` ```text `), and keep it short and realistic.
- Don't document internal or test-only settings (`MEMDUSA_TEST_*`).
- When something is a limitation, say so plainly in the relevant section.

## Formatting

- Shell blocks use ` ```sh `; config files use their language (` ```toml `, ` ```json `, ` ```yaml `).
- Tables for options and comparisons; bullets for lists; numbered steps only for real sequences.
- Link between pages with relative paths (`../team/oauth.md`). Link to reference pages instead of
  repeating option lists.
- Use admonition-style notes sparingly, as a bold lead: `**Note:** ...`.

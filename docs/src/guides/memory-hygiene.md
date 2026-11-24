# Memory hygiene

memdusa keeps memories outlives sessions, but memories go stale, overlap and get abandoned. Use the memory tools to keep the set small and correct, and let the brief flag what needs attention.

## Correct a memory in place

Use the `memory_update` MCP tool (REST: `PATCH /v1/memories/{id}`) instead of saving a near-copy. Pass the memory's `id` and any of `content`, `kind` (`fact`, `preference`, `decision`, `todo`, `note`) and `tags`. Only the fields you pass change. Redaction applies and the search index follows.

Save with `memory_save` (or `memdusa remember "…"`) for new knowledge. A near-duplicate is merged instead of added: a memory of the same kind and scope (a project, or your globals) whose significant words overlap by at least 75% (Jaccard similarity; short words and stopwords are ignored, so "auth" matches "authentication") is updated with the newer text and merged tags. The result says so: `merged: true` in REST/JSON, or `updated existing memory <id> (near-duplicate)` in the MCP tool.

On a multi-user server, a global memory can only be updated or forgotten by its owner or an admin (403 otherwise). Project memories stay editable by any user with write access. Use `memory_forget` (REST: `DELETE /v1/memories/{id}`) on a memory that is wrong or obsolete.

The brief flags a memory with ` _(may be outdated: src/auth/jwt.rs changed 2h ago)_` when the memory mentions a file that a recent `file_change` event touched after the memory was last updated.

## Search and recall

`memory_search` is ranked full-text search over memory content and tags. It uses SQLite FTS5 with `bm25`, or Postgres `tsvector` with `ts_rank` (`simple` configuration, no stemming). Any word may match; the best match comes first, and newest first among ties. Query text is split into plain words and never interpreted as query syntax. With no query, it lists the most recent memories.

On every prompt, the `UserPromptSubmit` hook searches the project's memories plus visible globals with the prompt. It injects up to 5 memories that match at least two of the prompt's distinctive words (one if the prompt has only one) in a `<memdusa-recall>` block. A memory is injected at most once per session, and not if it was already in the session's brief. Harnesses that cannot inject context on a prompt (Cursor) get nothing. Set `MEMDUSA_RECALL=off` to disable prompt recall; the resume brief at session start is unaffected.

When [semantic search](semantic-search.md) is on, ranking also uses meaning and prompt recall can add a memory by similarity alone. See that page for the ranking formula and `MEMDUSA_EMBED_RECALL_MIN`.

## Review what to consolidate

```sh
memdusa review          # or the `memory_review` MCP tool
```

The review lists three groups:

| Group | Meaning | What to do |
|---|---|---|
| Related memories | Same kind and scope, similar wording or (with semantic search) very close embeddings | `memory_update` the best one with the combined content, then `memory_forget` the rest |
| May be outdated | The memory mentions a file changed after it was written | Check it against the code, then `memory_update` or `memory_forget` |
| Old todos | Todos not updated for 30 or more days | Close them with `memory_forget`, or update them if still open |

memdusa has no LLM, so it only *finds* candidates; you decide and act. The related-memories check groups at most 6 memories per group and reports at most 20 groups. With semantic search, two memories are also related when their embedding cosine is at least 0.80, even with different words. Merging on save stays lexical. The brief nudges you when there is enough to review. Old hook activity is summarized separately by [retention](retention.md).

For the full list of tool arguments, see [MCP tools, resources and prompts](../reference/mcp.md). For command options, see the [CLI reference](../reference/cli.md).

See also: [Semantic search](semantic-search.md), [Retention and backups](retention.md).

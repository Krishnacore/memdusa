# Secret redaction

Hooks capture raw prompts and shell commands, and agents paste things into memories. Secrets will show up in that data. memdusa redacts everything **before it is stored**: in the store layer, which every write path goes through, and again on the client before a remote hook sends anything to the server.

Redaction replaces the secret with a `[REDACTED:<kind>]` marker and keeps the surrounding text, so the record stays useful:

```text
PGPASSWORD=[REDACTED:assignment] psql postgres://app:[REDACTED:url_password]@db/prod
```

## What is detected

| Detected | Examples |
|---|---|
| Provider tokens | AWS, GitHub, GitLab, Anthropic, OpenAI, Stripe, Slack, Google and npm tokens; JWTs; memdusa `mdk_` keys |
| Structure | private key blocks, `user:password@` in URLs, `Bearer …`, `Authorization: Basic …` |
| Assignments and flags | `*_TOKEN=…`, `password: …`, `"api_key": "…"`, `--password …`, `--api-key=…` |

In JSON values, a string stored under a secret-looking key (for example `"api_key"`) is replaced with `[REDACTED:field]`. Custom patterns produce `[REDACTED:custom]`. A full memdusa key (`mdk_` followed by 64 hex characters) is redacted; a prefix such as `mdk_1a2b3c4d` is not.

Redaction applies to memories, events, checkpoints, run inputs and outputs, step inputs and outputs, delegated assignments, and audit detail. Step outputs are redacted too, so a replayed step returns the redacted output. Do not pass secrets through runs.

**Note:** Redaction is precision-tuned. Variable references (`TOKEN=$GH_TOKEN`), numbers (`max_tokens=1000`), paths (`--token-file ~/.config/token`), placeholders (`<required>`) and ordinary words ("rotate tokens monthly") are deliberately left alone.

## Add your own patterns

Put one regular expression per line in `MEMDUSA_REDACT_PATTERNS` or in `~/.memdusa/redact-patterns.txt` (the file may contain `#` comments). Every match is replaced with `[REDACTED:custom]`:

```sh
export MEMDUSA_REDACT_PATTERNS='ACME-[0-9]{6}'
```

An invalid regular expression stops the command with a clear error rather than silently dropping the rule. `$MEMDUSA_HOME` overrides the directory that holds the pattern file.

## Turn redaction off

```sh
export MEMDUSA_REDACT=off
```

This stores everything verbatim. Use it only when you are certain the recorded data contains no secrets. Leaving redaction on is the default, and the client redacts again before sending a remote hook.

See also: [Semantic search](../guides/semantic-search.md) (with an HTTP provider, already-redacted memory text and queries leave the machine), [Configuration](../reference/configuration.md), [Hook events](../reference/hooks.md).

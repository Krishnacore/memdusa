# Semantic search

Search is lexical by default: full-text search where any word may match. That misses memories worded differently; "how do we ship releases" will not find "the release workflow pushes binaries on tag". Turn on semantic search to compare meaning as well, using an OpenAI-compatible embeddings API or a small model that runs inside memdusa.

Semantic search is off unless you set `MEMDUSA_EMBED_URL` or `MEMDUSA_EMBED_PROVIDER`.

## Use an embeddings API

Point `MEMDUSA_EMBED_URL` at a base URL that serves `POST {url}/embeddings`, then backfill your existing memories:

```sh
# OpenAI (uses OPENAI_API_KEY; that key is only sent when the host is api.openai.com)
export MEMDUSA_EMBED_URL=https://api.openai.com/v1

# Fully local with Ollama (first run: ollama pull nomic-embed-text)
export MEMDUSA_EMBED_URL=http://localhost:11434/v1
export MEMDUSA_EMBED_MODEL=nomic-embed-text

memdusa embed          # embed the memories you already have
memdusa status         # "semantic search: on (model ..., 12/12 memories embedded)"
```

New and edited memories are embedded when they are saved. `memdusa embed` only fills in memories that lack a vector for the current model; add `--all` to re-embed everything, or `--only-project` to limit it to one project. Changing `MEMDUSA_EMBED_MODEL` re-embeds on the next `memdusa embed`. The default HTTP model is `text-embedding-3-small`, and `MEMDUSA_EMBED_API_KEY` overrides `OPENAI_API_KEY` for any provider. For the full option list, see the [CLI reference](../reference/cli.md).

**Privacy:** With an HTTP provider, memory text (already redacted before storage) and your search queries are sent to that provider. Use Ollama or the built-in model to keep everything on your machine.

## Use the built-in local model

The `local-embed` cargo feature compiles in a retrieval-trained sentence transformer (`all-MiniLM-L6-v2`, 384 dimensions) that runs on ONNX Runtime inside memdusa. Nothing leaves your machine, and no API key is needed. Select it and fetch the model once:

```sh
export MEMDUSA_EMBED_URL=local                    # or MEMDUSA_EMBED_PROVIDER=local
export MEMDUSA_EMBED_MODEL=all-MiniLM-L6-v2       # default; bge-small-en-v1.5 is also supported
memdusa embed --download-model                    # once: ~91 MB into $MEMDUSA_HOME/models/ (default ~/.memdusa/models/)
memdusa embed                                     # backfill existing memories
memdusa status                                    # shows the model and coverage, or how to fetch it
```

You can also write `local:<model>` in `MEMDUSA_EMBED_URL`, or use the aliases `minilm` and `bge-small` in `MEMDUSA_EMBED_MODEL`.

Where the built-in model is available:

| Build | Built-in local model |
|---|---|
| macOS Apple Silicon (release binary, Homebrew) | included |
| Windows x64 (release binary) | included |
| Docker image (`ghcr.io/krishnacore/memdusa`) | included |
| Linux release binaries, Intel macOS | not included: ONNX Runtime's prebuilt library needs glibc 2.38+ and has no Intel-Mac build, while the release binaries support Ubuntu 22.04 / Debian 12 / RHEL 9. Use Ollama, or build with `--features local-embed` on a glibc 2.38+ system. |

From source: `cargo install --path crates/memdusa --features local-embed` (or `cargo build --release --features local-embed`).

The model is loaded at most once per process. The ONNX Runtime binaries (~20 MB per platform, fetched at build time by `ort`) are separate from the model download. Hooks and searches **never download**: if the model is missing they use lexical search, and `memdusa status` / `memdusa embed` print the one-line fetch command above. After the first Hugging Face download, memdusa works offline. Model text, memories and queries are processed in the same process.

## How ranking works

One vector per memory (content and tags) is stored in the database next to it, in both SQLite and Postgres. No database extension is needed. A memory is re-embedded when its text changes; vectors from another model are ignored until `memdusa embed` redoes them.

A search embeds the query (one API call) and ranks the 1,000 most recent visible memories by `0.6 × cosine similarity + 0.4 × lexical relevance`. Memories without a vector rank by lexical relevance alone. A memory found by meaning alone must also have a cosine of at least 0.3 and be within 0.08 of the best match. This ranking feeds the `memory_search` tool, `memdusa recall`, the relevant memories shown with each prompt, and the brief.

`memory_review` also groups memories whose embeddings are very close (cosine ≥ 0.80), not only those sharing words. Merging on save stays lexical. See [Memory hygiene](memory-hygiene.md).

Embedding never gets in the way. A request times out after 10 seconds (3 seconds for prompt recall and the brief); on failure or slowness memdusa logs one warning line and uses lexical search. A save or a hook never fails because of embedding.

## Tune prompt recall

`MEMDUSA_EMBED_RECALL_MIN` sets the similarity from which a memory is added to a prompt by meaning alone, without sharing words with it. It is a value from 0 to 1. The default is `0.5`, except with the built-in `all-MiniLM-L6-v2`, where it is `0.4`.

Scores differ by model, so tune the value for the model you chose:

| Model | Typical related score | Typical unrelated score | Suggested minimum |
|---|---|---|---|
| `nomic-embed-text` | about 0.45–0.60 | about 0.30–0.42 | the 0.4–0.5 default is a good start |
| `text-embedding-3-*` | lower overall | lower overall | try `0.35`–`0.4` |
| built-in `all-MiniLM-L6-v2` | about 0.50–0.60 for currency hits, ~0.23 for release-only hits | ≤ 0.20 | `0.4` |

Too low a value pulls loosely related memories into every prompt. This setting is read where the hook runs: on the server in hosted mode. See [Configuration](../reference/configuration.md) for the other embedding variables.

In the same quality probe, `bge-small-en-v1.5` also answered 3 of 4 target queries correctly but scored unrelated text much higher (up to 0.586). MiniLM is the default because it is smaller and better calibrated. Warm single-query latency is about 11 ms for MiniLM and 18 ms for BGE small on Apple Silicon.

## Hosted mode

The server's environment decides the model: set the embedding variables where `memdusa serve` runs. Clients need no embedding configuration. Backfill a server's existing memories with `memdusa embed` as an admin.

See also: [Memory hygiene](memory-hygiene.md), [Secret redaction](redaction.md), [Configuration](../reference/configuration.md).

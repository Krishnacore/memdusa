# Contributing

Issues and pull requests are welcome at
[github.com/Krishnacore/memdusa](https://github.com/Krishnacore/memdusa). To report a vulnerability, use
GitHub's private vulnerability reporting instead of a public issue (see [Security model](../team/security.md)).

## Build and test

You need a recent stable Rust toolchain.

```sh
cargo build
cargo test                                                               # SQLite
MEMDUSA_TEST_POSTGRES_URL=postgres://localhost/memdusa_test cargo test   # also Postgres
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy and the tests on Linux, macOS and Windows (also with the
`local-embed` feature), the store tests against Postgres 15 and 17, the install scripts, the Homebrew
formula and a Docker build. A pull request must pass all of them.

## Changing the code

- **Schema changes:** append a new entry to `MIGRATIONS` in `crates/memdusa-core/src/store.rs`. Never
  edit an entry that has been released.
- **CLI changes:** regenerate the CLI reference with `scripts/gen-cli-docs.sh`. CI fails when
  `docs/src/reference/cli.md` is out of date.
- **New harness:** add an adapter in `adapters.rs` (payload in, canonical format out, output rendered
  back), installation in `install.rs`, a fixture of the real payloads under `tests/fixtures/harness`, and
  a page under `docs/src/harnesses/`.
- **Docs:** follow [docs/STYLE.md](https://github.com/Krishnacore/memdusa/blob/main/docs/STYLE.md). Preview
  with `mdbook serve docs`.

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`. It builds binaries for five targets with SHA-256
checksums, attaches them to a GitHub release, pushes a multi-arch image to GHCR, and updates the formula in
the `homebrew-tap` repository (rendered by `packaging/homebrew/render-formula.sh`, skipped for
pre-release tags). The tap update needs the repository secret `HOMEBREW_TAP_DEPLOY_KEY`, the private half of
a deploy key with write access to the tap; without it the job only prints a warning.

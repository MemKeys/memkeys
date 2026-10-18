# Contributing to MemKeys

Thanks for contributing to MemKeys! This project is a Rust workspace; the
PR check suite is defined in `.github/workflows/` (see
[docs/branch-protection.md](docs/branch-protection.md) for the full gate list).

## Development loop

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --locked
cargo test --workspace   # integration tests need TEST_DATABASE_URL; they skip when unset
```

Integration tests and the conformance suite expect a reachable Postgres, e.g.:

```bash
docker run -d -p 5432:5432 -e POSTGRES_USER=memkeys -e POSTGRES_PASSWORD=memkeys \
  -e POSTGRES_DB=memkeys_test pgvector/pgvector:pg18
export TEST_DATABASE_URL=postgres://memkeys:memkeys@localhost:5432/memkeys_test
```

## DCO: sign your commits

This project enforces the [Developer Certificate of Origin](https://developercertificate.org/)
via the `dco` CI check on every PR: **every commit** must carry a
`Signed-off-by:` trailer matching the commit author, e.g.

```
Signed-off-by: Jane Developer <jane@example.com>
```

Add it automatically by committing with:

```bash
git commit -s
```

If you already have commits on your branch, add the trailer without changing
the commits' content:

```bash
git rebase --signoff origin/main
```

This applies to human contributors and agents alike: agents must commit with
`git commit -s` (or `git rebase --signoff`) before opening a PR, matching the
configured git identity.

## Security and governance

- Secret scanning (`gitleaks`), CodeQL and image scanning (`grype`) run on
  every PR. Never commit credentials; if a secret lands in a commit, rotate it
  — do not just remove it.
- Branch protection on `main` requires all PR checks plus linear history; see
  [docs/branch-protection.md](docs/branch-protection.md).

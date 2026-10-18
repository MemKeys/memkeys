# Branch protection on `main` — governance checklist

This is the P0 CI governance contract (ISI-6094 / ISI-6097): the PR check suite
defined in `.github/workflows/` is enforced on `main` via GitHub branch
protection. Apply these settings in **Settings → Branches → Branch protection
rules → main**.

## Required status checks

Enable **Require status checks to pass before merging** and select all of:

| Check (status-check name) | Workflow / job | Purpose |
|---|---|---|
| `fmt + clippy + test` | `ci.yml` / `ci` | rustfmt, clippy `-D warnings`, unit + integration tests (Postgres service), conformance + write-bench smoke |
| `build` | `ci.yml` / `build` | `cargo build --workspace --locked` build gate |
| `coverage` | `ci.yml` / `coverage` | `cargo-llvm-cov` lcov report, uploaded as artifact + PR step summary (report-only, no threshold yet) |
| `actionlint` | `ci.yml` / `actionlint` | Lints the workflow files themselves |
| `cargo-machete` | `ci.yml` / `machete` | Fails on unused dependencies |
| `CodeQL (rust)` | `security.yml` / `codeql` | CodeQL static security analysis for Rust |
| `gitleaks` | `security.yml` / `gitleaks` | Secret scan over full PR git history |
| `docker-build + sbom + grype` | `docker.yml` / `docker` | buildx image build (load only on PRs), `syft` SPDX SBOM, `grype` scan failing on **critical** (high reported, non-blocking initially) |
| `cargo-deny` | `docker.yml` / `deny` | Advisories (RustSec, yanked crates denied), bans (wildcards denied), licenses (permissive allow-list in `deny.toml`) |
| `dco` | `dco.yml` / `dco` | Every commit carries a matching `Signed-off-by:` trailer |

Note: once these workflows have run at least once on `main`, the check names
become selectable in the branch-protection UI. Until then they are absent from
the picker — apply this checklist in the same PR cycle that introduces the
workflows.

## Rule settings

- **Require a pull request before merging**
  - Required approving reviews: at least 1
  - **Dismiss stale pull request approvals when new commits are pushed**
  - Require review from Code Owners (once a CODEOWNERS file exists)
- **Require status checks to pass before merging** — all checks in the table
  above; do not leave any check as optional/ informational.
- **Require conversation resolution before merging**
- **Require linear history** — merge commits are not allowed on `main`; use
  "Rebase and merge" (keeps each commit's `Signed-off-by` intact) or
  "Squash and merge" (squash must be signed off by the PR author).
- **Require branches to be up to date before merging**
- **Include administrators** — the rules apply to admins too.
- Block force pushes and deletions (`main` only).

## CI invariants

- All PR-triggered jobs run with the default `GITHUB_TOKEN` only — **no
  repository secrets are required or used on `pull_request`**.
- GHCR pushes (`ghcr.io/memkeys/memkeys`) are a later phase and will run on
  `main`/tags only — PR builds use `load: true`, never `push`.
- Coverage is report-only in P0; a coverage threshold can be added later as a
  separate gate.

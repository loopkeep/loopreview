---
name: release
description: Use when asked to cut or publish a new loopreview release — the version bump, the changelog, the tag, and the release / skills-mirror workflows. Handles `/release` and `/release X.Y.Z`.
---

# Cut a loopreview release

Drive a loopreview release end to end: verify the tree is releasable, pick the
version, bump it, regenerate the changelog, commit, tag, and watch the workflows
publish it. A `v*` tag is the point of no return, so the human confirms before it
is pushed, and a failed release is never fixed by retagging — stop and consult.

Everything you write here is English (commit, tag message, changelog).

## Preflight — run all; abort on the first failure

Report which check failed and stop; do not "fix it up" silently.

```sh
git status --porcelain                       # must be empty (clean tree)
test "$(git rev-parse --abbrev-ref HEAD)" = main   # must be on main
git fetch origin
test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)"   # local == origin/main
```

- CI is green on `main`. CI is path-filtered (it runs only on code-affecting
  changes), so a docs-only HEAD has no run of its own — check `main`'s latest run:

  ```sh
  gh run list --workflow ci.yml --branch main --limit 1 \
    --json headSha,status,conclusion \
    --jq '.[0] | "\(.headSha[0:8]) \(.status)/\(.conclusion)"'
  ```

  It must read `completed`/`success`; a `failure` or an in-progress run stops the
  release.
- Local gates green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace`.
- The changelog has something to ship: `CHANGELOG.md`'s `## [Unreleased]` section
  is non-empty. If it is empty, stop — "nothing to release".

## 1. Decide the version

An explicit argument wins: `/release 0.2.0` uses `0.2.0`. Otherwise derive it
from the `[Unreleased]` groups by this deterministic rule — no prompt here (the
one confirmation is step 5, before the tag). The project is pre-1.0, so use the
left column; the ≥1.0 column is written down so reaching it needs no new ruling.

| `[Unreleased]` contains | pre-1.0 (`0.y.z`) | ≥1.0 |
| --- | --- | --- |
| a **Breaking** entry (`!`) | minor (`0.1→0.2`) | major |
| any **Added** (`feat`) | minor | minor |
| only **Fixed** / **Performance** (and/or docs) | patch | patch |
| **docs** only | patch — and warn "docs-only release — proceed?" | patch (warn) |

Why: under Cargo/semver's `0.y.z` convention the `y` is the compatibility
boundary, so a pre-1.0 breaking change is a minor bump. `feat` → minor (not
patch) deliberately, to signal the size of the change — loopreview is an
application, and a new capability is worth a minor.

Steps 2–4 are local and reversible, so run them without stopping; the single
human checkpoint is step 5.

## 2. Bump the version

Set `version` under `[workspace.package]` in the top-level `Cargo.toml` to
`X.Y.Z` — the workspace's single source of version. The crates inherit it with
`version.workspace = true`, and internal dependencies are declared path-only
(`{ path = "..." }`, no `version`), so there is nothing else to bump. The release
workflow refuses a tag that disagrees with it. Then update the lockfile:

```sh
cargo check
```

## 3. Regenerate the changelog

```sh
git cliff v0.1.0..HEAD --tag vX.Y.Z -o CHANGELOG.md
```

The `v0.1.0..HEAD` range keeps the hand-written `0.1.0` section (in `cliff.toml`'s
footer) out of regeneration. Skim the result and reword any entry whose commit
summary reads awkwardly — the file is hand-editable; only the range matters to
regeneration. If `git cliff` is missing: `cargo binstall git-cliff` (or grab a
prebuilt binary from https://github.com/orhun/git-cliff/releases).

## 4. Release commit

Only the bump and the changelog — never fold other changes into it:

```sh
git commit -am "chore(release): vX.Y.Z"
```

## 5. Tag and push — the one human checkpoint

Everything so far is local. Pushing the tag is irreversible (it publishes a
Release and mirrors the skill), so stop here and get the human's explicit
go-ahead with a one-line summary of what will ship — the version, the breakdown
that chose it, and that the local work is done. For example:

> `v0.2.0` (3 fix, 2 feat → minor) — bump, CHANGELOG, and `chore(release)`
> commit are done locally. Push the tag?

Add the "docs-only release — proceed?" note here if the version came from a
docs-only `[Unreleased]`. On a yes:

```sh
git tag -a vX.Y.Z -m "vX.Y.Z"     # annotated
git push origin main               # the release commit first
git push origin vX.Y.Z             # then the tag, which triggers the workflows
git ls-remote origin main "refs/tags/vX.Y.Z"   # confirm both arrived
```

## 6. Watch it publish

The tag triggers two workflows — `release.yml` (build + GitHub Release) and
`mirror-skills.yml` (sync the agent skill).

```sh
gh run watch    # or: gh run list --limit 5
gh release view vX.Y.Z
```

Confirm the Release has all five build archives (aarch64/x86_64 macOS,
x86_64/aarch64 Linux, x86_64 Windows) plus `checksums.txt`, then report the
release URL. If a workflow fails, report the cause and the fix and **stop** —
do not retag.

## Guardrails

- Never force-push.
- Never move or re-create a tag. If the release failed, work out why with the
  human; a new tag (`vX.Y.Z+1`) is the only forward path, never a retag.
- The release commit carries the bump and changelog and nothing else.

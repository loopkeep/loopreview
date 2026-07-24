# Releasing

loopreview ships prebuilt binaries from a version tag; the changelog is kept from
the Conventional Commit history with [git-cliff](https://git-cliff.org) (see
[`cliff.toml`](cliff.toml)).

The step-by-step procedure is encoded as the **`/release` skill**
([`.claude/skills/release/SKILL.md`](.claude/skills/release/SKILL.md)) — that is
the way to cut a release. This file is the policy behind it and the essentials for
doing it by hand.

## What a tag does

Pushing a `v*` tag runs [`release.yml`](.github/workflows/release.yml) — it
verifies the tag matches the workspace version, builds the macOS / Linux / Windows
archives, and publishes a GitHub Release — and
[`mirror-skills.yml`](.github/workflows/mirror-skills.yml), which syncs the agent
skill. The release body is this version's changelog section, regenerated from the
commits since the previous tag (`git cliff --latest --strip all`), so it matches
`CHANGELOG.md`.

## The v0.1.0 boundary

The `## [0.1.0]` section is a hand-written backfill kept in `cliff.toml`'s
`footer`; git-cliff never regenerates it. Always generate over the `v0.1.0..HEAD`
range — that boundary keeps the hand-written entry safe. Between releases,
`CHANGELOG.md`'s `[Unreleased]` needs no manual upkeep; refresh it on demand with
`git cliff v0.1.0..HEAD -o CHANGELOG.md`.

## By hand (without the skill)

1. Clean tree on an up-to-date `main`, with green CI and green gates
   (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`).
2. Bump `version` under `[workspace.package]` in `Cargo.toml`; `cargo check` to
   update the lockfile.
3. `git cliff v0.1.0..HEAD --tag vX.Y.Z -o CHANGELOG.md`, then skim it.
4. `git commit -am "chore(release): vX.Y.Z"` — the bump and changelog only.
5. `git tag -a vX.Y.Z -m vX.Y.Z && git push origin main vX.Y.Z`.

Never force-push and never move a tag; a failed release goes forward as a new
version, not a retag.

## Grouping

Commit types map to changelog sections in `cliff.toml`: `feat` → Added,
`fix` → Fixed, `perf` → Performance, `docs` → Documentation; `refactor`, `test`,
`chore`, `ci`, `build`, and `style` are omitted. A breaking change (`type!:` or a
`BREAKING CHANGE:` footer) is flagged **Breaking**. Only the commit summary is
used, so trailers never leak into the changelog.

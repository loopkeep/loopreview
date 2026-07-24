# Releasing

loopreview ships prebuilt binaries from a version tag. The changelog is kept from
the Conventional Commit history with [git-cliff](https://git-cliff.org) — see
[`cliff.toml`](cliff.toml).

## What is automated

Pushing a `v*` tag runs [`.github/workflows/release.yml`](.github/workflows/release.yml):
it checks the tag matches the workspace version, builds the macOS / Linux / Windows
archives, and publishes a GitHub Release. The release body is that version's
`CHANGELOG.md` section, regenerated from the commits since the previous tag
(`git cliff --latest --strip all`) — so it matches the committed changelog.

## Cutting a release

The `## [0.1.0]` section is a hand-written backfill kept in `cliff.toml`'s
`footer`; git-cliff never regenerates it. Everything above it is generated from
commits **after** `v0.1.0`, which is why every command below uses the
`v0.1.0..HEAD` range — that boundary keeps the hand-written entry safe.

1. Pick the version `X.Y.Z` (semver over the changes since the last tag).
2. Bump `[workspace.package].version` in `Cargo.toml` to `X.Y.Z` (the release
   workflow refuses a tag that disagrees with it), and run `cargo build` so
   `Cargo.lock` updates.
3. Roll the changelog's `[Unreleased]` into the new version:

   ```sh
   git cliff v0.1.0..HEAD --tag vX.Y.Z -o CHANGELOG.md
   ```

   Skim the result — reword any entry whose commit summary reads awkwardly (the
   file is hand-editable; only the range/boundary matters to regeneration).
4. Commit: `git commit -am "chore(release): vX.Y.Z"`.
5. Tag and push:

   ```sh
   git tag vX.Y.Z
   git push origin main --tags
   ```

The workflow does the rest. To preview the release body first:
`git cliff --latest --strip all`.

## Between releases

`CHANGELOG.md`'s `[Unreleased]` section is regenerated (not hand-appended) from
the commit history, so it needs no manual upkeep. To refresh it on demand:

```sh
git cliff v0.1.0..HEAD -o CHANGELOG.md
```

## Grouping

Commit types map to changelog sections in `cliff.toml`: `feat` → Added,
`fix` → Fixed, `perf` → Performance, `docs` → Documentation. `refactor`, `test`,
`chore`, `ci`, `build`, and `style` are omitted. A breaking change (`type!:` or a
`BREAKING CHANGE:` footer) is flagged **Breaking** in its entry. Only the commit
summary is used, so trailers never leak into the changelog.

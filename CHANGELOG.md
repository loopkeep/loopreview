# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-07-24

### Added

- Show the version on -v as well as -V/--version

### Fixed

- Let h reach the sidebar at narrow widths (single source of truth)

### Documentation

- Lead Install with the curl | sh installer

## [0.1.0] - 2026-07-24

Initial release — a review-first diff TUI.

- Review any diff in an interactive terminal UI: unified or side-by-side layout (auto by width), syntax highlighting, a file sidebar, and a finder.
- Sources: the working tree, staged changes, a `git diff <target>` range, one commit's own changes (`lr show`), or a unified-diff patch from a file or stdin.
- Local review that persists across sessions: line, range, and file comments, plus suggestions — kept in a per-repository store.
- GitHub pull requests, reviewed without a checkout, with two-way sync: pull the diff and every comment thread; push drafts, replies, resolutions, and reviews.
- A bare `lr <ref>` (a number, `#N`, `owner/repo#N`, or a URL) opens a pull request or an issue — the type is resolved from the API, not the reference.
- An Overview tab with the subject's title, facts, and rendered description; markdown renders faithfully with clickable links, images, and `#N` references.
- An agent control plane (`lr session …`) to inspect and steer a live review.
- Built-in self-update (`lr update`) from GitHub releases.

[0.2.0]: https://github.com/loopkeep/loopreview/compare/v0.1.0..v0.2.0
[0.1.0]: https://github.com/loopkeep/loopreview/releases/tag/v0.1.0

# loopreview

**Preview the loop, review the change.**

_A review-first diff TUI for the agent era._

loopreview (`lr`) is where a human inspects, comments on, and signs off the
output of an agent loop — a worktree full of changes, or a pull request. It opens
a diff in an interactive terminal UI built for reading: hunk by hunk, with syntax
highlighting, word-level emphasis of exactly what changed, and unified or
side-by-side layout. Leave inline comments and threads on a local review, or pull
a GitHub pull request into the same TUI — comments, replies, resolve, and submit
included — without checking anything out. And because reviewing agent work means
an agent is often on the other side, loopreview exposes a control plane so an
agent can read the diff, steer your view, and leave draft comments while you
watch.

It ships as a single static binary (`loopreview`, with the short alias `lr`) for
macOS, Linux, and Windows. Reading diffs needs only `git`; the pull-request
features use the `gh` CLI.

<!-- TODO: screenshot — unified review with an inline comment thread -->

## Features

- **Two layouts, responsive** — unified or side-by-side, chosen automatically by
  terminal width, and toggled at any time.
- **Built for reading** — syntect syntax highlighting, word-level intra-line
  emphasis, a line cursor for precise navigation, and full mouse support.
- **Built for big diffs** — fold files to their header, a file-explorer sidebar,
  and a fuzzy file finder (`Ctrl-P`); a large diff opens collapsed so it loads
  fast, and collapsed files are never highlighted.
- **Live by default** — a `notify`-based watcher refreshes the diff the moment
  files change on disk (event-driven, not polling); disable with `--no-watch`.
- **Local review** — leave markdown comments and threads on any line, reply,
  resolve, and mark outdated threads; everything renders inline and in a
  GitHub-style Conversation view, and persists per repository.
- **GitHub pull requests** — review a PR in the TUI without a checkout, with
  two-way comment sync, drafts that persist across sessions, and a submit modal
  (Comment / Approve / Request changes / Pending).
- **Agent control plane** — a running review hosts a local socket so an agent can
  read the diff structure, move your cursor, leave draft comments, and block on
  review events (`lr session …`, `lr skill …`).
- **Small dependency surface** — plain diff viewing needs only `git`; `gh` is
  required only for the pull-request features.

## Install

### Prebuilt binaries

Download the archive for your platform from the
[Releases](https://github.com/loopkeep/loopreview/releases) page
(`.tar.gz` for macOS/Linux, `.zip` for Windows), unpack it, and put the
`loopreview` binary (and its `lr` alias) somewhere on your `PATH`:

```sh
tar xzf loopreview-*.tar.gz
install -m755 loopreview lr ~/.local/bin/
```

Homebrew and `cargo-binstall` support are coming soon.

### From source

Requires a recent Rust toolchain (edition 2024).

```sh
cargo build --release
# binaries land in target/release/{loopreview,lr}
```

## Quickstart

```sh
lr                    # review the working tree (staged + unstaged vs HEAD)
lr diff main...       # review this branch's changes off main
lr diff --staged      # review only the staged changes
lr patch fix.patch    # review a saved patch...
git diff | lr         # ...or one piped in
lr pr 123             # review GitHub pull request #123 in the TUI
```

Useful global options: `--mode auto|unified|split` picks the layout,
`--no-watch` turns off auto-refresh, and `--exclude-untracked` drops untracked
files from a working-tree review.

## Keybindings

**Anywhere**

| Key | Action |
| --- | --- |
| `q` / `Esc` / `Ctrl-C` | Quit |
| `Tab` | Switch between the Files and Conversation views (once the review has threads) |
| `b` | Toggle the file-explorer sidebar |
| `Ctrl-P` | Open the fuzzy file finder |
| `Ctrl-R` | Refresh from GitHub (pull requests) |
| `Ctrl-S` | Open the submit modal (pull requests) |

**Files (diff) view**

| Key | Action |
| --- | --- |
| `j` / `k` (or `↓` / `↑`) | Move the cursor (over file headers and lines) |
| `l` (or `→`) | Go in: expand a folded file, or move from a header to its first line |
| `h` (or `←`) | Go out: line → its file header, expanded header → fold, folded header → sidebar (`b` jumps straight to the sidebar) |
| `Ctrl-D` / `Ctrl-U` | Half-page down / up |
| `Space` / `PageDown`, `PageUp` | Page down / up |
| `g` / `G` (or `Home` / `End`) | First / last line |
| `n` / `p` | Next / previous file |
| `]` `}` / `[` `{` | Next / previous hunk |
| `<` / `>` | Scroll the diff content left / right (also Shift+wheel or a trackpad swipe; the gutter stays fixed) |
| `v` | Toggle unified / side-by-side |
| `c` | Comment on the cursor line (or the selected range) |
| `V` | Start / cancel a line-range selection (`j` / `k` to extend, then `c`) |
| `r` | Reply to the thread on the cursor line |
| `x` | Resolve / reopen that thread |
| `o` | Fold: expand the current file if collapsed, else fold the thread at the cursor, else collapse the current file |
| Wheel | Scroll the diff (or the sidebar, when the pointer is over it) |
| Click | Move the cursor; a file header folds/unfolds it; a sidebar row toggles that file (opens it, or collapses an open one); a tab switches views; the footer's layout indicator (`[unified]`/`[split]`) toggles the layout |
| Drag | Select a line range (then `c` to comment) |

**Sidebar** (focused after `b`)

| Key | Action |
| --- | --- |
| `j` / `k` | Select a file (the list scrolls to follow; wheel also scrolls it) |
| `Enter` (or `l` / `→`) | Toggle the file: open a collapsed one (jumping the diff to it), or collapse an open one |
| `o` | Fold / unfold the selected file in place |
| `Esc` (or `h` / `←`) | Return focus to the diff |

**File finder** (`Ctrl-P`): type to fuzzy-filter; `↑` / `↓` (or `Ctrl-P` / `Ctrl-N`)
move; `Enter` opens the file; `Esc` closes.

**Conversation view**

| Key | Action |
| --- | --- |
| `j` / `k` | Select a thread |
| `g` / `G` | First / last thread |
| `Ctrl-D` / `Ctrl-U` (or `PageDown` / `PageUp`) | Scroll |
| `r` | Reply to the selected thread |
| `x` | Resolve / reopen |
| `o` | Collapse / expand |
| `X` | Close the review (asks to confirm) |

**Comment composer** (after `c` or `r`): type or paste markdown (multi-line);
`Ctrl-S` saves, `Esc` discards (confirming if the comment is non-empty).

## Reviewing

Press `c` on any line to open a markdown composer; the comment shows inline under
its line and starts a thread. To comment on several lines, select a range first —
`V` then `j` / `k`, or drag over the lines — and the composer's title shows the
`file:start-end` range. `r` replies to the thread on the cursor's line and `x`
resolves or reopens it. Once a review has any threads, `Tab` opens a
Conversation view — every thread as a root comment with its nested replies and
relative timestamps, GitHub-style — where `r` and `x` also work and `X` closes
(deletes) the review. Comments are authored as your `git config user.name`.

A local review is stored per repository under your config directory and shared
across that repo's worktrees (thread anchors carry the commit, so they stay
unambiguous). When the line a thread was pinned to moves out of the current diff,
the thread is marked **outdated** and its original context is reconstructed from
history — you can still read and reply to it.

<!-- TODO: screenshot — Conversation view with a resolved and an outdated thread -->

### Pull requests

`lr pr <number | url | owner/repo#n>` (or `lr pr --detect` for the current
branch) opens a pull request in the same TUI, fetching the diff and existing
review threads without touching your working tree. Your comments and replies are
kept as **drafts** — they persist across sessions — until you submit. `Ctrl-S`
opens the submit modal to choose the review event and an optional summary;
`Ctrl-R` re-pulls from GitHub while keeping your drafts. Resolving a published
thread syncs to GitHub. Multi-line comments round-trip as ranges (GitHub's
`start_line` / `startLine`). (The pull-request features use the `gh` CLI, which
must be installed and authenticated.)

## Agent integration

Reviewing agent work usually means an agent is on the other side of the loop. A
running loopreview hosts a per-session local socket (no central daemon) and
registers itself, so an agent — or a second terminal — can drive the review you
have open through `lr session`:

```sh
lr session list --json                       # find live sessions
lr session review --json                     # diff structure + threads
lr session review --patch --json             # ...including the raw lines
lr session navigate --file src/app.rs --line 42
lr session comment add --file src/app.rs --line 42 \
  --body "Is this retry safe under load?" --author agent
lr session wait --for reply                  # block until the human replies
```

The design keeps the human in charge: an agent reads, navigates (moving _your_
cursor and view, for explanation), and leaves **draft** comments — it cannot
publish a pull-request review (that stays your `Ctrl-S`) and cannot resolve a
published thread. Its actions appear in your status line as they happen, and
`wait` lets it hold a turn open until you reply, resolve, or submit.

`lr skill path` writes a bundled `SKILL.md` — a full reference to the workflow and
etiquette — and prints its path, so you can hand the manual straight to an agent.

## Configuration

Settings live in `<config-dir>/loopreview/config.toml` (all keys optional). A
legacy `config.json` is still read with a migration hint, but TOML is preferred.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `split_min_width` | integer | `160` | Body width, in columns, at which `auto` layout switches to side-by-side |
| `auto_collapse_files` | integer | `50` | A diff with more changed files than this opens with every file collapsed |
| `auto_collapse_lines` | integer | `20000` | A diff with more changed lines than this opens with every file collapsed |
| `sidebar` | `"auto"` / `"open"` / `"closed"` | `"auto"` | Whether the file-explorer sidebar is shown by default (`auto` = when wide enough) |
| `sidebar_min_content` | integer | `44` | Minimum diff width kept beside the sidebar; below this it auto-hides |

### Key bindings

Every command's key can be remapped in a `[keys]` table by action name. Keys are
written like `j`, `V` (or `shift+v`), `ctrl+p`, `enter`, `esc`, `tab`, `space`,
`pageup`. The arrow, page, and home/end keys always work and are not remappable;
an invalid binding is reported at startup with the offending line. Action names:
`cursor_down`, `cursor_up`, `half_page_down`, `half_page_up`, `top`, `bottom`,
`next_file`, `prev_file`, `next_hunk`, `prev_hunk`, `nav_in`, `nav_out`,
`scroll_left`, `scroll_right`, `layout_toggle`, `comment`, `reply`, `resolve`,
`fold`, `select`, `close_review`, `sidebar`, `file_finder`, `refresh`, `submit`.

```toml
split_min_width = 160
sidebar = "auto"

[keys]
comment = "m"
file_finder = "ctrl+t"
layout_toggle = "s"
```

Data is stored under the same directory: local reviews in `reviews/`, live
control-plane sessions in `sessions/`. `<config-dir>` is `$XDG_CONFIG_HOME` (or
`~/.config`) on macOS/Linux and `%APPDATA%` on Windows.

## Development

The workspace has four crates:

- **`loopreview-core`** — the diff model and source abstraction (worktree, ref,
  patch), UI-free and publishable.
- **`loopreview-control`** — the control-plane protocol, registry, and client,
  also UI-free.
- **`loopreview-github`** — GitHub pull-request sync (diff source + comment
  threads) via the `gh` CLI.
- **`loopreview-cli`** — the ratatui terminal UI, building the `loopreview` and
  `lr` binaries.

```sh
cargo run --bin lr             # run against the current repo's working tree
cargo test                     # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release          # optimized binaries in target/release/
```

## About the name

_loopreview_ folds three words into one — **loop**, **preview**, and **review** —
sharing the middle `p` (loo·**p**·review). It reviews the changes an agent
**loop** produces, and lets you **preview** the change before it lands.

## License

MIT — see [LICENSE](LICENSE).

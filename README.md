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
agent can read the diff, steer your view, and leave notes while you
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
  read the diff structure, move your cursor, leave local notes (or drafts to
  submit), and block on review events (`lr session …`).
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

## Updating

A prebuilt binary updates itself in place from GitHub Releases:

```sh
lr update           # download and install the latest release
lr update --check   # only report whether a newer release exists
```

`lr update` replaces both installed binaries (`loopreview` and its `lr` alias)
with the latest build for your platform, verifying the download's sha256 checksum
first; on Windows the previous executable is swept on the next launch. Installed
from source or a package manager, update through that channel instead — after a
`cargo install`, re-run it (`cargo install --path .`).

## Quickstart

```sh
lr                    # review the working tree (staged + unstaged vs HEAD)
lr diff main...       # review this branch's changes off main
lr diff --staged      # review only the staged changes
lr patch fix.patch    # review a saved patch...
git diff | lr         # ...or one piped in
lr pr 123             # review GitHub pull request #123 in the TUI
lr pr "#123"          # quote #N — a bare # is a shell comment
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
| `?` | Open the command palette — a searchable list of every action with its current key; actions that apply right now are listed first, the rest greyed |
| `b` | Toggle the file-explorer sidebar |
| `Ctrl-P` | Open the fuzzy file finder |
| `Ctrl-O` | Open the current spot on github.com — a published comment under the Conversation cursor deep-links to it, otherwise the PR page (pull requests) |
| `Ctrl-R` | Refresh from GitHub (pull requests) |
| `Ctrl-S` | Open the submit modal (pull requests) |

**Finding your way.** Two things share the work, so you never have to memorize
keys. The footer shows just the few actions worth reaching for at the cursor's
exact spot — it changes as you move (`r reply` appears over a thread, `s suggest`
over a changed line) — and always ends with `? all`. That `?` opens the command
palette: a searchable, runnable list of *every* action with its current key, the
ones that apply here listed first and the rest greyed with the reason. So you can
just press `?`, type what you want, and run it. In the palette, type to
fuzzy-filter, `↑`/`↓` (or `Ctrl-N`/`Ctrl-P`) to move, `Enter` to run the selected
action (an inapplicable one reports why), `Esc` to close.

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
| `s` | Suggest a change to the line (or range) — opens the composer with a `suggestion` code block pre-filled from the current new-side lines |
| `V` | Start / cancel a line-range selection (`j` / `k` to extend, then `c` or `s`) |
| `r` | Reply to the thread on the cursor line |
| `x` | Resolve / reopen that thread |
| `e` | Edit your own comment (the thread's root; opens the composer pre-filled — a published edit syncs to GitHub) |
| `d` | Remove your own comment at the cursor (a draft locally; your own published comment is deleted from GitHub, with confirmation) |
| `t` | Toggle the thread's root between a draft and a local note (pull requests) |
| `o` | Fold: expand the current file if collapsed, else fold the thread at the cursor, else collapse the current file |
| Wheel | Scroll the diff (or the sidebar, when the pointer is over it) |
| Click | Move the cursor; a file header folds/unfolds it; a sidebar row toggles that file (opens it, or collapses an open one); a tab switches views; the footer's layout indicator (`[unified]`/`[split]`) toggles the layout |
| Drag | Select a line range (then `c` to comment) |

**Sidebar** (focused after `b`)

The sidebar and the diff are framed side by side; the pane holding focus has an
accent-colored frame, and the inactive pane's cursor dims — so it is always
clear where your keys will land. Each file row carries a change-status letter
(`A`/`D`/`M`/`R`/`C` — added, deleted, modified, renamed, copied), its name, and
its `+`/`−` line counts (plus a badge when it has comments); files are grouped
under dim directory header rows (root files first, without a header). The header
rows are labels only — the cursor and clicks land on files. Pin a fixed width
with the `sidebar_width` config key.

| Key | Action |
| --- | --- |
| `j` / `k` | Select a file (skipping directory headers; the list scrolls to follow, wheel also scrolls it) |
| `Enter` (or `l` / `→`) | Toggle the file: open a collapsed one (jumping the diff to it), or collapse an open one |
| `o` | Fold / unfold the selected file in place |
| `Esc` (or `h` / `←`) | Return focus to the diff |

**File finder** (`Ctrl-P`): type to fuzzy-filter; `↑` / `↓` (or `Ctrl-P` / `Ctrl-N`)
move; `Enter` opens the file; `Esc` closes.

**Conversation view**

| Key | Action |
| --- | --- |
| `j` / `k` | Move between comments (root and replies), crossing into the next/previous thread at a thread's ends |
| `g` / `G` | First / last thread |
| `Ctrl-D` / `Ctrl-U` (or `PageDown` / `PageUp`) | Scroll |
| `r` | Reply to the selected thread |
| `x` | Resolve / reopen |
| `e` | Edit your own comment at the cursor (root or reply; a published edit syncs to GitHub) |
| `d` | Remove your own comment at the cursor (a draft reply removes itself, a draft root its thread; your own published comment is deleted from GitHub, with confirmation) |
| `t` | Toggle the comment at the cursor between a draft and a local note — a reply needs its root to be a draft first (pull requests) |
| `o` | Collapse / expand |
| `X` | Close the review (asks to confirm) |

**Comment composer** (after `c`, `s`, `r`, or `e`): type or paste markdown
(multi-line). `Enter` inserts a newline and `Ctrl-S` saves — so multi-line
comments and suggestions never hinge on `Shift+Enter`. `Esc` discards (confirming
if non-empty). Outside the composer `Ctrl-S` opens the submit modal, so the one
key saves inside and submits outside; the composer's hint bar always names the
key that saves. Prefer `Enter` to save? Set `composer_enter = "save"` (below) —
worth it only where your terminal reports `Shift+Enter` (the Kitty protocol),
which then becomes the newline (with `Alt+Enter` as a fallback).

## Reviewing

Press `c` on any line to open a markdown composer; the comment shows inline under
its line and starts a thread. To comment on several lines, select a range first —
`V` then `j` / `k`, or drag over the lines — and the composer's title shows the
`file:start-end` range. `r` replies to the thread on the cursor's line and `x`
resolves or reopens it. Once a review has any threads, `Tab` opens a
Conversation view — every thread as a root comment with its nested replies and
relative timestamps, GitHub-style — where `r` and `x` also work and `X` closes
(deletes) the review. Comments are authored as your `git config user.name`.

**Suggesting a change.** `s` — on a line, or over a `V` / drag range — opens the
composer already holding a `suggestion` code block filled with those lines'
current new-side text. Rewrite the block into the code you want (add prose above
or below if you like) and save it like any other comment. On a pull request a
submitted suggestion shows up on GitHub as an **Apply suggestion** the author can
commit in one click; in a local review it is kept as a plain fenced code block.
Suggestions replace the new side, so a line that was only deleted can't take one.

A local review is stored per repository under your config directory and shared
across that repo's worktrees (thread anchors carry the commit, so they stay
unambiguous). When the line a thread was pinned to moves out of the current diff,
the thread is marked **outdated** and its original context is reconstructed from
history — you can still read and reply to it.

<!-- TODO: screenshot — Conversation view with a resolved and an outdated thread -->

### Pull requests

`lr pr <number | url | owner/repo#n | #n>` (or `lr pr --detect` for the current
branch) opens a pull request in the same TUI, fetching the diff and existing
review threads without touching your working tree. Your comments and replies are
kept as **drafts** (a `[draft]` badge) — they persist across sessions — until you
submit. `Ctrl-S` opens the submit modal, which lists what will be sent and by
whom (flagging any draft not authored by you, since it goes out under your
identity); pick the review event and an optional summary. `Ctrl-R` re-pulls from
GitHub while keeping your drafts. Resolving a published thread syncs to GitHub.
Multi-line comments round-trip as ranges (GitHub's `start_line` / `startLine`).
(The pull-request features use the `gh` CLI, which must be installed and
authenticated.)

A comment also has a **kind**. Yours default to drafts; an agent's default to
**local notes** (a `[local]` badge) — attached to the review but never sent. `t`
toggles the comment at the cursor between the two (in Files it toggles the
thread's root), so you can adopt an agent's note as a draft to send, or drop one
of your drafts to a local note. The kinds stay coherent: a reply can only become
a draft under a draft or published root (otherwise `t` says to promote the root
first), and demoting a root to a local note takes its draft replies down with it.
Only drafts are ever submitted; local notes stay off GitHub.

You can also edit (`e`) or delete (`d`) your **own already-published** comment —
matched against your GitHub login — and it syncs straight to GitHub: an edit via
a PATCH, a delete via a DELETE (with a confirmation, since it is irreversible).
Both run in the background and report failures. Someone else's comment is never
editable or deletable, and an agent can only touch its own unpublished notes.

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
  --body "Is this retry safe under load?"          # a local note; add --draft to queue it
lr session wait --for reply                  # block until the human replies
```

The design keeps the human in charge: an agent reads, navigates (moving _your_
cursor and view, for explanation), and leaves **local notes** — attached to the
review for you to read, never sent. It marks the few worth publishing with
`--draft`; even then it cannot submit a pull-request review (that stays your
`Ctrl-S`) or resolve a published thread. Its actions appear in your status line
as they happen, and `wait` lets it hold a turn open until you reply, resolve, or
submit.

A full `SKILL.md` — the workflow and etiquette an agent needs to drive a review —
is published to [`loopkeep/skills`](https://github.com/loopkeep/skills). Install
it into an agent with `npx skills add loopkeep/skills -s loopreview-session`.

The same `lr session` control plane is what higher-level tooling builds on. One
example is
[`herdr-plugin-loopreview`](https://github.com/loopkeep/herdr-plugin-loopreview),
a plugin for [herdr](https://herdr.dev) (a terminal multiplexer for coding
agents): a single popup lists the repository's git worktrees and open GitHub pull
requests, and picking one opens its diff with `lr` (or `lr pr <ref>`) in a pane
the plugin reuses — swapping that one pane between targets instead of spawning
more — and it cleans up throwaway agent worktrees safely. Install it with herdr:

```sh
herdr plugin install loopkeep/herdr-plugin-loopreview
```

It leans only on tools you already have — `lr`, `gh`, `git`, and `herdr`.

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
| `sidebar_width` | integer | auto | Pin the sidebar to a fixed width in columns (clamped to 22–44); unset auto-fits the longest file row |
| `composer_enter` | `"newline"` / `"save"` | `"newline"` | What `Enter` does in the comment composer: insert a newline (with `Ctrl-S` to save — the reliable default) or save (with `Shift`/`Alt+Enter` for a newline, where the Kitty protocol reaches your terminal) |

### Key bindings

Every command's key can be remapped in a `[keys]` table by action name. Keys are
written like `j`, `V` (or `shift+v`), `ctrl+p`, `enter`, `esc`, `tab`, `space`,
`pageup`. The arrow, page, and home/end keys always work and are not remappable;
an invalid binding is reported at startup with the offending line. A few keys are
reserved by the UI and cannot be reassigned to an action: `q` / `Esc` / `Ctrl-C`
(quit), `Tab` (switch view), and `y` / `Enter` (confirm); the finder and comment
composer likewise keep their own keys while open. Action names:
`cursor_down`, `cursor_up`, `half_page_down`, `half_page_up`, `top`, `bottom`,
`next_file`, `prev_file`, `next_hunk`, `prev_hunk`, `nav_in`, `nav_out`,
`scroll_left`, `scroll_right`, `layout_toggle`, `comment`, `suggest`, `reply`,
`resolve`, `fold`, `select`, `close_review`, `delete`, `edit`, `toggle_kind`,
`sidebar`, `file_finder`, `refresh`, `submit`, `palette`, `open_github`.

```toml
split_min_width = 160
sidebar = "auto"

[keys]
comment = "m"
file_finder = "ctrl+t"
layout_toggle = "w"
```

Data is stored under the same directory: local reviews in `reviews/`, live
control-plane sessions in `sessions/`. `<config-dir>` is `$XDG_CONFIG_HOME` (or
`~/.config`) on macOS/Linux and `%APPDATA%` on Windows.

## Dependencies

loopreview keeps a deliberately small footprint at runtime:

- **`git`** — the only requirement for reviewing diffs (worktree, ref, or a
  piped/saved patch). No library binding; loopreview shells out to `git`.
- **`gh`** — the GitHub CLI, needed only for the pull-request features
  (`lr pr`, submitting a review, resolving a published thread, and editing your
  own published comments). Install it and run `gh auth login`; loopreview never
  handles a token itself. Local reviews need none of this — every local-review
  feature works with `git` alone.

The binary itself is self-contained — syntax highlighting, the control-plane
socket, and the self-updater are all built in. Notable libraries: `ratatui` and
`crossterm` for the terminal UI, `syntect` with `two-face` for highlighting,
`interprocess` for the per-session control socket, `nucleo-matcher` for the
fuzzy finder, and `notify` for the live file watcher. `lr update` adds `ureq`
(rustls), `flate2`, `tar`, and `sha2` for downloading and verifying releases.

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

## License

MIT — see [LICENSE](LICENSE).

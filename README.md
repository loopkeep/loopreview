# loopreview

loopreview (`lr`) is a review-first diff viewer for the terminal. It opens a
diff in an interactive TUI so you can read changes hunk by hunk, with syntax
highlighting, intra-line emphasis of exactly what changed, and unified or
side-by-side layout (chosen automatically by width). With no arguments it shows
the working tree; `lr diff <target>` compares git refs; `lr patch` reviews a
saved or piped patch.

```sh
lr                 # review the working tree (staged + unstaged vs HEAD)
lr diff main...    # review this branch's changes off main
lr diff --staged   # review only the staged changes
lr patch fix.diff  # review a saved patch (or: git diff | lr patch)
git diff | lr      # review a patch piped in
```

Navigate with `j`/`k` (line cursor), `n`/`p` between files, `[`/`]` between
hunks, `Ctrl-D`/`Ctrl-U` to page, `g`/`G` for the ends, `v` to toggle
unified/side-by-side, and `q` to quit. The mouse wheel scrolls and a click moves
the cursor. Working-tree and ref views refresh automatically as files change
(pass `--no-watch` to disable).

## Reviewing

Press `c` on any line to leave a comment (markdown, multi-line); `r` replies to a
thread on the cursor's line and `x` resolves it. Comments show inline under their
line and persist to a per-repository store under your config directory, shared
across the repo's worktrees. Once a review has threads, `Tab` opens a
Conversation view listing every thread — root comment, replies, and relative
times — where `r`/`x` also work and `X` closes (deletes) the review. Comments are
authored as your `git config user.name`.

## Development

The workspace has two crates: `loopreview-core` (the diff model and source
abstraction, UI-free) and `loopreview-cli` (the ratatui terminal UI, building
the `loopreview` and `lr` binaries).

```sh
cargo run --bin lr             # run against the current repo's working tree
cargo test                     # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release          # optimized binaries in target/release/
```

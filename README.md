# loopreview

loopreview (`lr`) is a review-first diff viewer for the terminal. It opens a
diff in an interactive TUI so you can read changes hunk by hunk, with syntax
highlighting and file/hunk navigation. With no arguments it shows the working
tree; give a target such as `main...` or `HEAD~3` to compare git refs; or pipe a
patch straight in.

```sh
lr                 # review the working tree (staged + unstaged vs HEAD)
lr main...         # review this branch's changes off main
lr diff HEAD~3     # review the last three commits
git diff | lr      # review a patch from stdin
```

Navigate with `j`/`k` to scroll, `n`/`p` to jump between files, `[`/`]` between
hunks, `g`/`G` for top/bottom, and `q` to quit.

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

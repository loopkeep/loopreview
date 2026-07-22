# loopreview-session skill

Agent skill for steering a live loopreview (`lr`) diff-review session over its
control plane: read the diff structure and threads, navigate the reviewer's
view, leave draft comments, and wait for the human to react.

The `SKILL.md` here is the same document bundled into the `lr` binary
(`lr skill path`), mirrored from the loopreview repository on each release —
the published skill always matches a released binary.

Install just this skill:

```sh
npx skills add loopkeep/skills -s loopreview-session
```

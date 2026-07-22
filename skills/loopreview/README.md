# loopreview-session skill

Agent skill for steering a live loopreview (`lr`) diff-review session over its
control plane: read the diff structure and threads, navigate the reviewer's
view, leave local notes (or drafts to submit), and wait for the human to react.

`SKILL.md` here is the source of truth, mirrored to this public repo from the
loopreview repository on each release — so the published skill always matches a
released binary.

Install just this skill:

```sh
npx skills add loopkeep/skills -s loopreview-session
```

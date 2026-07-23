---
name: loopreview-session
description: Read and steer a live loopreview diff-review session over its control plane. Inspect the diff structure and threads, move the human reviewer's view, leave local notes (or drafts to submit), and wait for the human to reply or resolve. Use when a loopreview (lr) session is running and you are helping review a changeset.
---

# loopreview session control

loopreview (`lr`) is an interactive terminal diff reviewer. **The TUI belongs to
the human** — do not run `lr`, `lr diff`, `lr <pr-ref>`, or other UI commands
yourself. Instead use `lr session *` to inspect and steer the review the human
already has open, through its local socket.

Each running `lr` hosts its own socket and registers itself; there is no daemon.
If no session is live, ask the human to open one (`lr` in their repository, or
`lr <ref>` — a number, `#N`, `owner/repo#N`, or a URL — for a pull request).

## Golden workflow

```text
1. lr session list                      # find live sessions
2. lr session get --json                # on a PR, read subject.title/body — the change's intent
3. lr session review --json             # understand the diff structure + threads
4. lr session review --patch --json     # read the actual lines for a closer look
5. lr session navigate --file F --line N  # steer the human's view to what matters
6. lr session comment add ...           # leave one focused local note (--draft to queue for GitHub)
7. lr session wait --for reply          # block until the human answers, then continue
```

On a pull request, read the `subject` (title + description) first — it is the
stated intent you judge the diff against. Then read structure (`review --json`),
pull the patch text only for the files you truly need, navigate before you
comment so the human sees what you mean, and wait on events instead of polling.

## Selecting a session

Every verb except `list` accepts a target:

- `--repo <path>` — match the session by its repository root (defaults to the
  current directory).
- `<session-id>` — match by exact id (use when several sessions share a repo).
- If exactly one session is live, it resolves automatically.

## Reading

```bash
lr session list [--json]
lr session get [<id>|--repo .] [--json]
lr session context [<id>|--repo .] [--json]
lr session review [<id>|--repo .] [--patch] [--json]
lr session comment list [<id>|--repo .] [--json]
```

- `list`/`get` report the session id, pid, repo, and **source** — a descriptive
  label such as `working tree`, `git diff main...`, `show HEAD~1 (a1b2c3d)` (one
  commit's changes), `PR #7`, or `issue owner/repo#5`. When a repo has more than
  one live session (say a worktree review and a `lr show`), match on the source
  string to pick the id you want.
- On a **pull-request or issue** session, `get` also carries a `subject` object:
  `{kind, number, title, status, author, base, head, body, url}`. `kind` is `pr`
  or `issue`; `status` is lowercase (a PR: `draft`/`open`/`merged`/`closed`; an
  issue: `open`/`closed`/`not_planned`); `base`/`head` are present only for a PR;
  `body` is the description in markdown. **Read it before you start reviewing** —
  the title and description are the stated intent, the primary context for judging
  the change. A plain diff session has no `subject`.
- An **issue** session has no diff: `review --json` returns an empty `files` list
  (its content is the conversation and the `subject` body), and `navigate --file`
  has nothing to point at. Read the `subject` and the threads; comment with
  `comment add --conversation` (a line comment has no meaning without files).
- `context` reports the human's current view (`files`/`conversation`), the line
  under their cursor, the thread there (if any), and `event_seq` — the latest
  event number, which you pass to `wait --after` to avoid missing events.
- `review --json` returns the file/hunk structure and every thread. It omits line
  text by default to keep your context small; add `--patch` for the raw diff
  lines of the files you need to read closely.
- Line numbers come from `review`: additions and context lines have a `new`
  number (side `new`); deletions have an `old` number (side `old`). Use those
  exact numbers and sides when navigating or commenting.

## Steering the view

`navigate` moves the human's actual cursor and view.

```bash
lr session navigate --repo . --file src/app.rs --line 120           # a new-side line
lr session navigate --repo . --file src/app.rs --line 88 --side old  # a deleted line
lr session navigate --repo . --thread <thread-id>                    # to a thread's anchor
```

Give exactly one target: `--thread`, or `--file` with `--line`. A thread on an
outdated or file-level anchor opens the Conversation view instead of a diff line.

## Commenting

Your comments carry a **kind**. By default they are **local** — attached to the
review for the human to read, but never sent to GitHub. Pass `--draft` to make a
comment a **draft**: on a pull request the human can submit drafts to GitHub
(Ctrl-S in the TUI). This split is deliberate — an agent converses in local
notes, and marks the few worth publishing with `--draft`, so nothing reaches
GitHub by accident. Outside a pull request there is nowhere to submit, so
`--draft` is a no-op and everything stays local. You never publish, and you
cannot resolve a published pull-request thread — that stays the human's call.

`--author` names who is speaking; it defaults to `agent`. Prefer a stable,
specific name (your role or model) so a multi-agent conversation reads clearly.

```bash
lr session comment add --repo . --file src/app.rs --line 120 \
  --body "This retries without a backoff — is that intended under load?"
lr session comment add --repo . --file src/app.rs --line 120 --draft --author reviewer-bot \
  --body "Nit: use the shared client here."         # queued for the human to submit
lr session comment add --repo . --conversation \
  --body "Overall the retry story is solid; one nit inline."   # a note on the whole change, no line
lr session comment reply --repo . --thread <id> --body "Good point — flagged."
lr session comment edit --repo . --comment <id> --body "Revised: use the shared client."  # fix your own wording
lr session comment resolve --repo . --thread <id> --author agent   # local reviews and your own drafts
lr session comment rm --repo . --comment <id>                     # withdraw a local note or draft
```

- `comment add` needs `--file`, `--line`, and `--body`; `--side` defaults to
  `new` and `--author` to `agent`. The line must be one shown in the current diff.
  Use `--conversation` instead of `--file`/`--line` to leave a comment on the
  whole change (an overall verdict or summary, tied to no line) — the two are
  mutually exclusive. Like any agent comment it is local by default; `--draft`
  queues it to submit as a PR comment.
- Comment `--body` is **markdown**: headings, emphasis, inline/fenced code, lists,
  block quotes, GitHub alerts (`> [!NOTE]` / `[!TIP]` / `[!IMPORTANT]` /
  `[!WARNING]` / `[!CAUTION]`), task lists (`- [ ]`), and tables all render.
  Reach for an alert only where it genuinely earns attention — a real risk or a
  must-read caveat — not as decoration on ordinary notes.
- `--draft` queues a comment for the human to submit; without it the comment (or
  reply) is a local note. `--draft` only matters on a pull request. A `--draft`
  **reply** is refused under a local-note root — it could never be sent while the
  root stays off GitHub — so reply without `--draft`, or have the root promoted
  to a draft first. A reply to a **conversation** thread (one not tied to a line)
  is always local: `--draft` on it is refused, since GitHub's conversation is
  flat and the reply would post as an unrelated top-level comment.
- `comment edit --comment <id> --body <text>` replaces the body of one of your
  own unpublished comments (a draft or local note, root or reply). It refuses a
  published comment (writing to GitHub is the human's action) and another
  author's comment (that would misattribute it).
- `comment resolve` works on local-review threads and your own drafts (`--reopen`
  flips it back). It refuses a published pull-request thread — the human's call.
- `comment rm` withdraws one of your own unpublished comments — `--comment <id>`
  removes that comment (and its thread if it empties), `--thread <id>` removes
  the whole thread (exactly one is required). It refuses anything published to
  GitHub. Both `comment rm` and `comment edit` take the id as a **flag**
  (`--comment` / `--thread`), never as a positional argument.

## Waiting for the human

`wait` blocks until a matching event, then returns it. This is how you hold a
turn open until the human reacts, instead of polling.

```bash
lr session wait --repo . --for reply                         # any reply
lr session wait --repo . --for resolve --for submit --timeout 600
lr session wait --repo . --for reply --after 12              # only events past seq 12
```

- `--for` takes `comment`, `reply`, `resolve`, `submit`, or `reload`; repeat it
  for several kinds, or omit it to wait for any event.
- `--timeout <seconds>` bounds the wait; on timeout the event is null and the
  command exits non-zero. A wait always returns within 600 seconds even with no
  `--timeout`, so a long vigil is a loop of waits, each chained with `--after`.
- To not miss an event between two waits, read `event_seq` from `context` (or the
  `event_seq` a previous `wait` returned) and pass it as `--after`.

## Reloading

```bash
lr session reload [<id>|--repo .]
```

Re-reads the session's source (a working-tree or ref diff reloads immediately; a
pull request re-pulls in the background — `wait --for reload` to know it landed).
A working-tree session usually auto-refreshes on save, so you rarely need this.

## Guiding a review

Your job is to narrate: steer the human to what matters and leave notes that
explain what they are looking at.

1. `review --json` to grasp the shape; `--patch` for the code you must read.
2. `navigate` to the first thing worth their attention.
3. `comment add` a focused note — intent, risk, or a question.
4. `wait --for reply` and continue the conversation, or move to the next point.

Guidelines:

- Work in the order that tells the clearest story, not strict file order.
- Navigate before commenting so the human sees the code you mean.
- Keep notes focused: intent, structure, risks, follow-ups — not every hunk.
- Do not churn the human's screen: navigate deliberately, one place at a time.
- Never publish for the human; converse in local notes and mark only the few
  worth sending with `--draft`, for the human to submit.

## Common errors

- **"no live review sessions"** — the human has no `lr` open; ask them to start
  one. If `lr` is visibly running, the socket may be blocked by a sandbox; retry
  with the needed access.
- **"several live sessions"** — pass a `<session-id>` from `lr session list`, or
  `--repo <path>`.
- **"line N (…) is not shown in the diff for F"** — use a line number and side
  from `review` output; the line must be part of the current diff.
- **"resolving a published pull-request thread is a human action"** — leave it
  for the human; you can still reply.
- **"the thread root is local — promote it first, or reply without --draft"** —
  you passed `--draft` on a reply whose thread root is a local note; drop
  `--draft` (the reply inherits local) or ask the human to make the root a draft.

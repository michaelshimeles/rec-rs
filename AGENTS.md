# Agent workflow

Choose the applicable parts of this workflow for the requested task. It comes
from [michaelshimeles/skills](https://github.com/michaelshimeles/skills). The
sections from "This repo" down are specific to rec-rs.

- Read-only reviews, explanations, and investigations need no worktree, commit,
  PR, or Greptile run. If an investigation leads to edits, isolate those edits.
- Code changes use isolation, relevant architecture guidance, checks, and
  evidence of the behavior changed. Capture a failure before fixing it.
- Small documentation edits use an isolated branch and relevant content or link
  checks. Do not manufacture runtime evidence or require a bot review for them.
- Ship when the task calls for a PR or a completed repository change. A request
  for a plan, review, or local experiment does not imply publishing it.

## Workflow

1. **Isolate.** Use `/new-feature` before repository edits. Start from
   `origin/main`, or preserve a worktree already assigned to this task.
   Never build on `main`.
2. **Build.** Use `/code-structure` when extracting shared operations or
   choosing boundaries. Follow the project's existing architecture; the
   service layer pattern is a default for projects without a convention.
3. **Prove.** Run relevant checks. Use `/evidence-driven-testing` for changed
   runtime behavior: record the failure before fixing it and the success
   afterward. For documentation, inspect the changed instructions and verify
   runnable examples and links as applicable.
4. **Ship.** Open the PR with evidence appropriate to the change. Use
   `/before-and-after` when screenshots help; use output pairs for CLI changes.
   Run `/greploop` for code or executable workflow changes, or when requested.
   Aim for **5/5 with zero unresolved actionable Greptile findings** on the
   current commit. Stop at the configured iteration cap, a review timeout, or
   an unavailable integration and report the actual score and remaining work.
   Do not claim success or raise the cap automatically. Present the PR URL.

Ship-beat notes:

- `/before-and-after` drives the `@vercel/before-and-after` CLI. `--markdown`
  uploads the pair and prints a PR-ready table; it also accepts existing
  PNGs, so evidence gathered while developing can be reused as-is.
- In containers/VMs where Chrome fails with "No usable sandbox", set
  `AGENT_BROWSER_ARGS="--no-sandbox"` for the capture command.
- The default upload host (0x0.st) is public — fine for ordinary UI shots;
  pass `--upload-url` for anything sensitive.

## Writing for humans

Run `/unslop` over anything a person will read, before you commit, post, or
send it: commit messages, the PR title and body, README and doc edits, code
comments, and the closing reply. It strips AI tells (em dashes, filler,
hedging, chatbot phrases, puffery, bold-label lists) and replaces fancy
words with plain ones and passive voice with active. Apply it to text you
wrote or changed, not to prose you didn't touch.

## Multi-agent rules

- Never commit directly to `main`.
- One worktree and one branch per task and per agent — never reuse or modify
  another agent's worktree, branch, or uncommitted work.
- **Scope check** before starting: skim open PRs' changed files
  (`gh pr list`, `gh pr diff <n> --name-only`) and look for uncommitted work
  in shared checkouts. Inspect overlapping diffs: independent edits to the same
  file may proceed in your own worktree. Ask only when changes conflict in
  behavior, depend on unfinished work, or ownership is unclear. Do not modify
  another task's checkout.
- Never force-push to `main` — and never plain `--force` anywhere; only
  `--force-with-lease`, only on your own task branch.
- Resolve lockfile conflicts by regenerating, never by hand-merging.
- Worktrees don't isolate shared resources: confirm a dev-server port
  answers *your* process before trusting it, and don't run schema
  experiments against a shared database.
- If a conflict can't be resolved confidently, stop and report instead of
  guessing.

## Completing a task that ships

1. Keep changes limited to the assigned task.
2. Run the applicable checks listed below.
3. Assemble applicable evidence into before/after pairs or content-check results.
4. Commit with a clear message, rebase onto the latest `origin/main`, and
   rerun the checks.
5. Push (`git push -u origin <branch>`; after rebasing an already-pushed
   branch, `--force-with-lease`).
6. Open the PR. The body must explain what changed, how it was tested (every
   claim backed by evidence), before/after proof, and any risks or follow-up
   work. Run the title and body through `/unslop` before posting.
7. Run `/greploop` when applicable, with the success and stopping conditions
   above. `/greploop-apps` selects the alternate trigger in that same workflow.
8. End by presenting the PR URL.

Do not merge the PR unless explicitly instructed. Keep the worktree until
the PR is merged or closed.

## This repo

rec-rs is Rec, a screen and camera recorder for macOS written in Rust on Apple
frameworks, plus its download page in `site/`. `README.md` covers the app's
design and layout, and `site/README.md` covers the page.

- Mutagen syncs `/Users/Shared/projects/rec-rs` to four other machines. Do task
  work in a separate clone or worktree, and keep build output, recordings,
  `node_modules` and evidence out of the synced checkout. `scripts/build.sh`
  already builds into `~/.cache/rec-rs-target`, and test recordings go to
  `~/Movies/rec-rs-tests/`.
- Commit messages are conventional commits, usually scoped to the area they
  touch, such as `feat(app):`, `fix(writer):`, `feat(site):` or `docs:`.
- rec-rs is a clean-room implementation. Don't copy OBS code or assets, and
  don't suggest that Rec is affiliated with the OBS Project.
- Performance numbers, in the README or on the site, come from measurements
  recorded in `PERFORMANCE.md`.
- The site stays one fast page with no third-party requests in production.
  Vercel Web Analytics is its only analytics. It sets no cookies, stores no
  personal data and loads from the site's own domain. Add no other tracking.
  The copy describes features the app ships today.
- Don't deploy the site, create Vercel projects, or publish releases or tags
  unless Micky asks.

## Checks in this repo

For the app:

- `CARGO_TARGET_DIR=$HOME/.cache/rec-rs-target cargo test` runs the unit tests.
- `scripts/build.sh` builds and signs `dist/Rec.app`. After it,
  `scripts/selftest.sh` records through every pipeline and verifies each file.
  Run it for changes to capture, compositing, encoding, audio or the writer. It
  needs the privacy grants, and `--no-screen` skips the cases that need Screen
  Recording.
- Format only the files you change. Running `cargo fmt` across the crate once
  reformatted `src/app/mod.rs` and `src/app/ui.rs` by accident, and commit
  `aed1e1a` undid it.

For the site, from `site/`:

- `npm ci`, then `npm run check` (expect 0 errors and 0 warnings) and
  `npm run build`.
- Run the Svelte MCP `svelte-autofixer` on every `.svelte` file you edit until
  it reports no issues.
- For page changes, run `npm run preview` and look at desktop and mobile widths
  in light and dark, then flip the theme toggle. Check for console errors and
  horizontal overflow. A running preview server doesn't pick up a new build, so
  restart it after each `npm run build`.
- `curl -sI http://localhost:<port>/download` returns a 302 whose `location` is
  `DOWNLOAD_URL` from `site/src/lib/site.ts`.

For every change, `git diff --check` checks whitespace. Keep generated evidence
under the ignored `.artifacts/` directory of a checkout outside the synced
folder.

## Skills

The workflow skills live in `.agents/skills/`, which Cursor and Codex read.
`.claude/skills/` holds symlinks to the same folders for Claude Code.
`npx skills add michaelshimeles/skills` installed them, and `skills-lock.json`
records each one's source and hash. Change a skill upstream, not here, then
pull the new version:

```sh
npx skills update -p
```

| Skill | Origin |
|---|---|
| `new-feature`, `code-structure`, `evidence-driven-testing` | michaelshimeles/skills |
| `before-and-after` | vendored there from [vercel-labs/before-and-after](https://github.com/vercel-labs/before-and-after) |
| `greploop` | vendored there from [greptileai/skills](https://github.com/greptileai/skills) |
| `greploop-apps` | michaelshimeles/skills; needs `greploop` beside it |
| `unslop` | vendored there from [cursor/plugins (pstack)](https://github.com/cursor/plugins/tree/main/pstack/skills/unslop), with frontmatter edited so agents apply it unprompted |

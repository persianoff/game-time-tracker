# GameTime: project rules

Repo: https://github.com/persianoff/game-time-tracker (private). Plan: `docs/PLAN.md`.

## Workflow: plan → implement → review
1. **Plan (Opus, main session).** The main session designs the work and keeps `docs/PLAN.md` up to date. Every task is a **GitHub issue** with a scope, acceptance criteria and labels (`tracker`, `viewer`, `infra`). Progress lives on the issues, not in chat.
2. **Implement (Sonnet, `implementer` subagent).** The main session briefs the implementer with the issue number:
   - The implementer works on branch `issue-<N>-<slug>`, writes code and tests, and runs `cargo build` and `cargo test`.
   - It commits and opens a PR whose body says `Closes #<N>`.
   - It doesn't change the plan or expand scope.
3. **Review (Opus, `reviewer` subagent).** The reviewer checks the PR diff against the issue and the plan. It reports findings and a verdict, and doesn't edit code. The main session posts the review summary as a PR comment.
4. **Loop.** If the verdict is CHANGES REQUIRED, the findings go back to the implementer on the same branch. When the verdict is APPROVED, the main session squash-merges the PR, which closes the issue.

The main session doesn't write production code itself. It plans, files issues, briefs agents, routes findings, merges and reports to the user.

## Project constraints (from the plan)
- Rust, native Windows. Target: about 2–4 MB working set and near-zero CPU.
- Dependencies are limited to `windows-sys`, `serde` and `serde_json`. Any new crate needs user approval.
- Storage is append-only JSONL plus a checkpoint, in `%APPDATA%\GameTime\`.
- The viewer is a single `viewer/index.html` in vanilla JS, with no libraries and no build step.
- Autostart uses the HKCU Run key, with self-heal, and respects Task Manager's disabled state.
